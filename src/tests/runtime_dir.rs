//! Unit tests for the parts of [`crate::runtime_dir::bounded_copy`] a caller
//! cannot reach from outside the crate: the refusals that only a source
//! changing *during* a copy can produce, and the one case where a changing
//! name must not matter at all. They drive the crate-internal seams instead of
//! a second process or a sleep, so the sequence is exact and the result is the
//! same on every run.

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use crate::runtime_dir::{DestMode, bounded_copy_inner};

/// How long the fifo test waits for the copy to answer. It only has to open
/// the source and refuse it, so a generous bound still keeps a regression from
/// being a hung suite.
const FIFO_WAIT: Duration = Duration::from_secs(5);

/// Empty scratch directory for one copy test, unique per tag and pid.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wwrpc-copy-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the scratch dir is created");
    dir
}

/// Nothing: the hook a test passes at the moment it has no interest in.
fn keep_the_source(_src: &Path) {}

/// Replace `path` with a fifo, whose read-only open waits for some other
/// process to write to it instead of returning. `mkfifo(2)` is the only way to
/// make one — safe Rust has none — and `libc` is already a dependency of this
/// crate.
fn plant_fifo(path: &Path) {
    fs::remove_file(path).expect("the regular file can be unlinked");
    let name =
        CString::new(path.as_os_str().as_bytes()).expect("a scratch path carries no interior nul");
    // SAFETY: `name` is nul-terminated and outlives the call; `mkfifo` only
    // creates the entry, and 0600 leaves it as private as the file it replaced.
    let rc = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "the fifo is created: {}", io::Error::last_os_error());
}

/// Append one byte to `path`, so it grows past the size already measured.
fn grow(path: &Path) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .expect("the source is appendable")
        .write_all(b"x")
        .expect("the source is writable");
}

/// Drop the last byte of `path`, so it ends up shorter than what was
/// measured. Length is the only thing this changes, deliberately: a
/// modification time moves at the filesystem's clock granularity, which two
/// calls a microsecond apart would not see.
fn shrink(path: &Path) {
    let len = fs::metadata(path).expect("the source is readable").len();
    OpenOptions::new()
        .write(true)
        .open(path)
        .expect("the source is writable")
        .set_len(len - 1)
        .expect("the source can be truncated");
}

/// Pins the byte cap as an enforced invariant rather than a pre-check: a
/// source that grows past `limit` after it was measured must be caught by
/// the read loop itself, and the destination it had already started must be
/// taken away instead of left behind as a silently truncated snapshot.
#[test]
fn cap_probe_refuses_a_source_that_grew() {
    let dir = scratch("cap");
    let src = dir.join("game.db");
    let dst = dir.join("snap.db");
    fs::write(&src, [b'a'; 16]).unwrap();

    let err = bounded_copy_inner(&src, &dst, 16, DestMode::Fresh, keep_the_source, grow)
        .expect_err("16 measured bytes plus one more must be refused");

    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("grew past the 16-byte limit"),
        "the read loop must be what refuses it, not the pre-check: {err}"
    );
    assert!(
        !dst.symlink_metadata().is_ok(),
        "the refused copy must leave no destination behind"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Pins the post-copy re-check: a source that shrank after it was measured
/// would be snapshotted as a whole file with a hole in it, so the copy must
/// be refused and the destination removed.
#[test]
fn recheck_refuses_a_source_that_shrank() {
    let dir = scratch("recheck");
    let src = dir.join("game.db");
    let dst = dir.join("snap.db");
    fs::write(&src, [b'a'; 16]).unwrap();

    let err = bounded_copy_inner(
        &src,
        &dst,
        1 << 20,
        DestMode::Fresh,
        keep_the_source,
        shrink,
    )
    .expect_err("a source that shrank mid-copy must be refused");

    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("changed while it was copied"),
        "the post-copy re-check must be what refuses it: {err}"
    );
    assert!(
        !dst.symlink_metadata().is_ok(),
        "the refused copy must leave no destination behind"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Pins that the post-copy re-check judges the file the bytes came from, not
/// the name they were reached by: a source replaced by a dangling symlink
/// mid-copy leaves behind a whole, unchanged file, so the copy stands. A
/// verdict taken from the path instead would report `NotFound` here and
/// throw a good snapshot away.
#[test]
fn recheck_judges_the_read_file_not_the_path() {
    let dir = scratch("renamed");
    let src = dir.join("game.db");
    let dst = dir.join("snap.db");
    fs::write(&src, [b'a'; 16]).unwrap();

    bounded_copy_inner(
        &src,
        &dst,
        1 << 20,
        DestMode::Fresh,
        keep_the_source,
        |path| {
            fs::remove_file(path).expect("the measured file can be unlinked");
            std::os::unix::fs::symlink(dir.join("never-existed.db"), path)
                .expect("the dangling link can be planted");
        },
    )
    .expect("a whole, unchanged source stays a good snapshot whatever the name becomes");

    assert_eq!(fs::read(&dst).unwrap(), [b'a'; 16]);
    let _ = fs::remove_dir_all(&dir);
}

/// Pins that the post-copy refusal also takes away a destination the copy
/// had truncated: the older bytes are already gone by then, so a
/// half-written remnant must not survive where the next reader expects a
/// whole snapshot.
#[test]
fn recheck_removes_a_truncated_destination() {
    let dir = scratch("truncated");
    let src = dir.join("game.db");
    let dst = dir.join("snap.db");
    fs::write(&src, [b'a'; 16]).unwrap();
    fs::write(&dst, b"an older, longer snapshot").unwrap();

    bounded_copy_inner(
        &src,
        &dst,
        1 << 20,
        DestMode::Overwrite,
        keep_the_source,
        shrink,
    )
    .expect_err("a source that shrank mid-copy must be refused");

    assert!(
        !dst.symlink_metadata().is_ok(),
        "a destination this call truncated must be taken away too"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Pins that the refusal of a source that is not a regular file is decided by
/// the opened handle, not by a check of the name: the window between such a
/// check and the open is exactly where a fifo is planted here, while a copy of
/// a plain file is in flight. Opening a fifo read-only blocks until some other
/// process opens it for writing, so a copy that checks the name and then opens
/// it blocks forever in a daemon that has no writer coming.
///
/// The copy runs on its own thread and the answer is awaited with a bounded
/// timeout, so a copy that blocks fails this test instead of the suite. The
/// blocked thread is detached rather than joined: it would never come back.
#[test]
fn a_fifo_planted_before_the_open_is_refused_not_awaited() {
    let dir = scratch("fifo");
    let src = dir.join("game.db");
    let dst = dir.join("snap.db");
    fs::write(&src, [b'a'; 16]).unwrap();

    let (sender, receiver) = mpsc::channel();
    let call_src = src.clone();
    let call_dst = dst.clone();
    let blocked = std::thread::spawn(move || {
        let _ = sender.send(bounded_copy_inner(
            &call_src,
            &call_dst,
            1 << 20,
            DestMode::Fresh,
            plant_fifo,
            keep_the_source,
        ));
    });

    let outcome = receiver.recv_timeout(FIFO_WAIT).unwrap_or_else(|err| {
        panic!(
            "the copy did not answer within {FIFO_WAIT:?} ({err}): it is blocked \
             opening the fifo, waiting for a writer that never arrives"
        )
    });

    let err = outcome.expect_err("a source that is a fifo must be refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("not a regular file"),
        "the refusal must name what is wrong: {err}"
    );
    assert!(
        !dst.symlink_metadata().is_ok(),
        "a refused source must not even create the destination"
    );
    drop(blocked);
    let _ = fs::remove_dir_all(&dir);
}
