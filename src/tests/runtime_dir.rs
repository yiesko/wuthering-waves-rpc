//! Unit tests for the parts of [`crate::runtime_dir::bounded_copy`] a caller
//! cannot reach from outside the crate: the refusals that only a source
//! changing *during* a copy can produce, and the one case where a changing
//! name must not matter at all. They drive the crate-internal
//! `after_measure` seam instead of a second thread or a sleep, so the
//! sequence is exact and the result is the same on every run.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::runtime_dir::{DestMode, bounded_copy_inner};

/// Empty scratch directory for one copy test, unique per tag and pid.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wwrpc-copy-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the scratch dir is created");
    dir
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

    let err = bounded_copy_inner(&src, &dst, 16, DestMode::Fresh, grow)
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

    let err = bounded_copy_inner(&src, &dst, 1 << 20, DestMode::Fresh, shrink)
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

    bounded_copy_inner(&src, &dst, 1 << 20, DestMode::Fresh, |path| {
        fs::remove_file(path).expect("the measured file can be unlinked");
        std::os::unix::fs::symlink(dir.join("never-existed.db"), path)
            .expect("the dangling link can be planted");
    })
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

    bounded_copy_inner(&src, &dst, 1 << 20, DestMode::Overwrite, shrink)
        .expect_err("a source that shrank mid-copy must be refused");

    assert!(
        !dst.symlink_metadata().is_ok(),
        "a destination this call truncated must be taken away too"
    );
    let _ = fs::remove_dir_all(&dir);
}
