// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Integration tests for [`wwrpc::runtime_dir`]: runtime-directory
//! selection and hardening, the RAII private temp dir, and exclusive
//! private file creation.
//!
//! Every test runs behind [`EnvGuard`]: the process environment is global,
//! and `XDG_RUNTIME_DIR` decides which directory the whole module uses.

use std::ffi::{CString, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::Duration;

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use wwrpc::runtime_dir::{
    DestMode, PrivateTempDir, bounded_copy, create_private_file, runtime_dir, signature_unchanged,
};

/// Environment variable this file manipulates to pick the runtime base.
const XDG: &str = "XDG_RUNTIME_DIR";

/// Environment variable `std::env::temp_dir()` honors. Set by the fallback
/// tests to sandbox the shared temp dir instead of touching the real `/tmp`.
const TMPDIR: &str = "TMPDIR";

/// How long the fifo test waits for [`bounded_copy`] to answer. It only ever
/// has to open a file and refuse it, so a generous bound still keeps a
/// regression from being a hung suite.
const FIFO_WAIT: Duration = Duration::from_secs(5);

/// Serializes the whole file: each test reads or writes these variables,
/// directly or through [`runtime_dir`].
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Holds [`ENV_LOCK`] and restores every variable it touched on drop, so a
/// test may set or unset them freely.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    /// Acquire the file-wide env lock and snapshot the managed variables.
    fn new() -> Self {
        // A panicking test must not wedge every later test in this file.
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = [XDG, TMPDIR]
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        Self { _lock: lock, saved }
    }

    /// Point `key` at `value` for the rest of the test.
    fn set(&self, key: &str, value: &str) {
        // SAFETY: only this guard mutates the env (the file-wide lock is
        // held for the test's whole life) and the values only steer path
        // selection inside `runtime_dir`.
        unsafe { std::env::set_var(key, value) };
    }

    /// Unset `key` for the rest of the test.
    fn clear(&self, key: &str) {
        // SAFETY: same invariant as [`EnvGuard::set`].
        unsafe { std::env::remove_var(key) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: restoring the snapshots taken before the test ran, still
        // under the lock this guard holds.
        unsafe {
            for (key, previous) in &self.saved {
                match previous {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// Per-process fixture directory; unique per tag so the tests of this file
/// never share state. Call it before pointing `TMPDIR` into a sandbox: the
/// path comes from the real temp dir.
fn fixture(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("wwrpc-rt-{}-{tag}", std::process::id()))
}

/// Expected fallback path under `base`: `wwrpc-<uid>`, the naming rule that
/// keeps one account from squatting a shared temp dir.
fn expected_fallback(base: &Path) -> PathBuf {
    let uid = std::fs::metadata("/proc/self")
        .map(|meta| std::os::unix::fs::MetadataExt::uid(&meta))
        .expect("our own /proc entry is readable");
    base.join(format!("wwrpc-{uid}"))
}

/// Permission bits (`mode & 0o777`) of `path`, following symlinks as the
/// implementation does.
fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|err| panic!("stat '{}': {err}", path.display()))
        .mode()
        & 0o777
}

/// Empty scratch directory for one copy test. The caller must hold an
/// [`EnvGuard`]: [`fixture`] reads `TMPDIR`, which the other tests of this
/// file mutate while they run.
fn scratch(tag: &str) -> PathBuf {
    let dir = fixture(tag);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch dir is created");
    dir
}

/// Pins that an absolute, valid `XDG_RUNTIME_DIR` yields
/// `<base>/wwrpc` as an owner-only (0700) directory.
#[test]
fn test_runtime_dir_creates_private_dir() {
    let env = EnvGuard::new();
    let base = fixture("valid");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    env.set(XDG, base.to_str().expect("temp dir is utf-8"));

    let dir = runtime_dir().expect("runtime_dir succeeds under a valid base");

    assert_eq!(
        dir,
        base.join("wwrpc"),
        "the runtime dir must sit directly under the configured base"
    );
    assert_eq!(mode_of(&dir), 0o700, "the runtime dir must be owner-only");
    let _ = std::fs::remove_dir_all(&base);
}

/// Pins that empty and relative `XDG_RUNTIME_DIR` values are ignored: the
/// result is the shared temp-dir fallback path, and nothing is created
/// relative to the working directory.
#[test]
fn test_runtime_dir_rejects_empty_and_relative() {
    let env = EnvGuard::new();
    // Sandbox the fallback: the test must never touch the real `/tmp`.
    let sandbox = fixture("relative");
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(&sandbox).unwrap();
    env.set(TMPDIR, sandbox.to_str().expect("temp dir is utf-8"));

    env.set(XDG, "");
    let dir = runtime_dir().expect("an empty base must fall back, not fail");
    assert!(
        dir.is_absolute(),
        "an empty base must not be used: '{}'",
        dir.display()
    );
    assert_eq!(
        dir,
        expected_fallback(&sandbox),
        "an empty base must use the per-uid fallback"
    );

    env.set(XDG, "relative/path");
    let dir = runtime_dir().expect("a relative base must fall back, not fail");
    assert!(
        dir.is_absolute(),
        "a relative base must not be used: '{}'",
        dir.display()
    );
    assert_eq!(
        dir,
        expected_fallback(&sandbox),
        "a relative base must use the per-uid fallback"
    );
    assert!(
        !Path::new("wwrpc").exists(),
        "a relative base must not create ./wwrpc in the working directory"
    );

    let _ = std::fs::remove_dir_all(&sandbox);
}

/// Pins that with no `XDG_RUNTIME_DIR` at all the module returns the shared
/// system temp dir fallback, named per uid (`wwrpc-<uid>`) so one account
/// cannot squat the plain name. `TMPDIR` sandboxes the fallback, so nothing
/// outside the fixture is created or removed.
#[test]
fn test_runtime_dir_falls_back_without_env() {
    let env = EnvGuard::new();
    let sandbox = fixture("fallback");
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(&sandbox).unwrap();
    env.clear(XDG);
    env.set(TMPDIR, sandbox.to_str().expect("temp dir is utf-8"));

    let dir = runtime_dir().expect("a missing base falls back to temp_dir");

    assert_eq!(
        dir,
        expected_fallback(&sandbox),
        "the shared fallback must be named after the uid"
    );
    assert_eq!(mode_of(&dir), 0o700, "the fallback dir is owner-only");

    let _ = std::fs::remove_dir_all(&sandbox);
}

/// Pins that a held [`PrivateTempDir`] exists, is 0700 and carries the
/// `wwrpc-<tag>-` name shape, and that dropping it removes the directory.
#[test]
fn test_private_temp_dir_is_private_and_cleaned() {
    let env = EnvGuard::new();
    let base = fixture("private");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    env.set(XDG, base.to_str().expect("temp dir is utf-8"));

    let temp = PrivateTempDir::new("t").expect("temp dir is created");
    let path = temp.path().to_path_buf();

    assert!(path.exists(), "the temp dir must exist while held");
    assert_eq!(mode_of(&path), 0o700, "the temp dir must be owner-only");
    let name = path
        .file_name()
        .expect("temp dir has a final component")
        .to_string_lossy()
        .into_owned();
    assert!(
        name.contains("wwrpc-t-"),
        "unexpected temp dir name: '{name}'"
    );

    drop(temp);
    assert!(!path.exists(), "dropping the guard must clean the dir up");
    let _ = std::fs::remove_dir_all(&base);
}

/// Pins that two live [`PrivateTempDir`] instances never share a path, so
/// concurrent runs cannot scribble into each other's snapshot.
#[test]
fn test_private_temp_dir_new_is_unique() {
    let env = EnvGuard::new();
    let base = fixture("unique");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    env.set(XDG, base.to_str().expect("temp dir is utf-8"));

    let first = PrivateTempDir::new("u").expect("first temp dir");
    let second = PrivateTempDir::new("u").expect("second temp dir");

    assert_ne!(
        first.path(),
        second.path(),
        "each temp dir must get its own path"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Pins that [`create_private_file`] writes a 0600 file and refuses any
/// destination that already exists — a regular file as much as a
/// pre-planted symlink, whose target must stay untouched. The env guard is
/// held because [`fixture`] reads `TMPDIR`.
#[test]
fn test_create_private_file_mode_and_symlink_refusal() {
    let _env = EnvGuard::new();
    let dir = fixture("file");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let path = dir.join("snapshot.db");
    create_private_file(&path, b"payload").expect("the file is created");
    assert_eq!(std::fs::read(&path).unwrap(), b"payload");
    assert_eq!(mode_of(&path), 0o600, "private files must be owner-only");

    let existing = dir.join("existing.db");
    std::fs::write(&existing, b"keep").unwrap();
    assert!(
        create_private_file(&existing, b"clobber").is_err(),
        "an existing regular file must never be overwritten"
    );
    assert_eq!(
        std::fs::read(&existing).unwrap(),
        b"keep",
        "the refused write must leave the original bytes in place"
    );

    let target = dir.join("secret.db");
    std::fs::write(&target, b"secret").unwrap();
    let link = dir.join("link.db");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(
        create_private_file(&link, b"clobber").is_err(),
        "a pre-existing symlink must be refused instead of followed"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"secret",
        "the symlink target must stay untouched"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Pins the hostile-shared-temp-dir contract: a symlink at the runtime path
/// is refused instead of followed into another user's directory.
#[test]
fn test_runtime_dir_refuses_symlinked_runtime_dir() {
    let env = EnvGuard::new();
    let base = fixture("linked");
    let _ = std::fs::remove_dir_all(&base);
    let target = base.join("target");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, base.join("wwrpc")).unwrap();
    env.set(XDG, base.to_str().expect("temp dir is utf-8"));

    let err = runtime_dir().expect_err("a symlinked runtime dir must be refused");

    assert_eq!(
        err.to_string(),
        format!(
            "runtime dir '{}' is a symlink; refusing it",
            base.join("wwrpc").display()
        )
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Pins that a runtime dir the caller owns is repaired to `0700` instead of
/// being rejected, so a drifted mode (one bad startup, an old version) can
/// never disable the service.
#[test]
fn test_runtime_dir_repairs_mode_drift() {
    let env = EnvGuard::new();
    let base = fixture("loose");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("wwrpc")).unwrap();
    std::fs::set_permissions(base.join("wwrpc"), std::fs::Permissions::from_mode(0o777)).unwrap();
    env.set(XDG, base.to_str().expect("temp dir is utf-8"));

    let dir = runtime_dir().expect("our own loose dir is repaired, not refused");

    assert_eq!(
        dir,
        base.join("wwrpc"),
        "the runtime dir must sit directly under the configured base"
    );
    assert_eq!(mode_of(&dir), 0o700, "the mode drifts back to owner-only");
    let _ = std::fs::remove_dir_all(&base);
}

/// Pins that a missing base — a `XDG_RUNTIME_DIR` left over from a login
/// manager that is gone — is rebuilt instead of becoming a hard failure,
/// and that every missing level is rebuilt, not just the last one.
#[test]
fn test_runtime_dir_rebuilds_missing_base() {
    let env = EnvGuard::new();
    let root = fixture("stale");
    let base = root.join("missing").join("root");
    let _ = std::fs::remove_dir_all(&root);
    env.set(XDG, base.to_str().expect("temp dir is utf-8"));

    let dir = runtime_dir().expect("a missing base is rebuilt");

    assert_eq!(dir, base.join("wwrpc"));
    assert!(
        base.is_dir(),
        "every missing level of the base must be rebuilt"
    );
    assert_eq!(mode_of(&dir), 0o700, "the rebuilt dir is owner-only");
    let _ = std::fs::remove_dir_all(&root);
}

/// Pins that a valid `XDG_RUNTIME_DIR` keeps the plain `wwrpc` entry name
/// even when it points at the shared temp dir: honoring the operator's
/// value is the spec, so only the fallback — a base we picked ourselves —
/// carries the `wwrpc-<uid>` suffix that keeps accounts apart.
#[test]
fn test_runtime_dir_xdg_base_keeps_plain_name_at_temp_dir() {
    let env = EnvGuard::new();
    // Sandbox the fallback: the test must never touch the real `/tmp`.
    let sandbox = fixture("xdg-temp");
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(&sandbox).unwrap();
    env.set(TMPDIR, sandbox.to_str().expect("temp dir is utf-8"));
    // Point XDG at exactly what the fallback would have used, so the two
    // paths differ only in how they were reached.
    let base = std::env::temp_dir();
    env.set(XDG, base.to_str().expect("temp dir is utf-8"));

    let dir = runtime_dir().expect("a valid XDG base is honored, wherever it points");

    assert_eq!(
        dir,
        base.join("wwrpc"),
        "a configured base must keep the plain name whatever directory it is"
    );
    assert_eq!(
        dir.file_name().and_then(|name| name.to_str()),
        Some("wwrpc"),
        "the uid suffix belongs to the fallback alone, never to a configured base"
    );

    let _ = std::fs::remove_dir_all(&sandbox);
}

/// Pins that a source over the caller's byte limit is refused before the
/// destination is ever created: an oversized file must cost nothing, and
/// leave nothing behind.
#[test]
fn test_bounded_copy_rejects_oversized() {
    let _env = EnvGuard::new();
    let dir = scratch("oversized");
    let src = dir.join("big.db");
    let dst = dir.join("snap.db");
    std::fs::write(&src, b"0123456789abcdef").unwrap();

    let err = bounded_copy(&src, &dst, 8, DestMode::Fresh)
        .expect_err("16 bytes over an 8-byte limit must be refused");

    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("over the 8-byte limit"),
        "the pre-check must name the limit it refused: {err}"
    );
    assert!(
        !dst.exists(),
        "an oversized source must not even create the destination"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pins that a source of exactly `limit` bytes is copied: the cap refuses
/// one byte more, never the boundary itself.
#[test]
fn test_bounded_copy_accepts_exactly_at_limit() {
    let _env = EnvGuard::new();
    let dir = scratch("exact");
    let src = dir.join("exact.db");
    let dst = dir.join("snap.db");
    std::fs::write(&src, b"0123456789abcdef").unwrap();

    bounded_copy(&src, &dst, 16, DestMode::Fresh).expect("16 bytes fit a 16-byte limit");

    assert_eq!(std::fs::read(&dst).unwrap(), b"0123456789abcdef");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pins the happy path: the bytes land verbatim and the destination is
/// private (`0600`), whatever the umask did to the requested mode.
#[test]
fn test_bounded_copy_happy_path() {
    let _env = EnvGuard::new();
    let dir = scratch("happy");
    let src = dir.join("src.db");
    let dst = dir.join("snap.db");
    let payload: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    std::fs::write(&src, &payload).unwrap();

    bounded_copy(&src, &dst, 1 << 20, DestMode::Fresh).expect("a small source is copied");

    assert_eq!(
        std::fs::read(&dst).unwrap(),
        payload,
        "the bytes must land verbatim"
    );
    assert_eq!(mode_of(&dst), 0o600, "a snapshot must be owner-only");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pins that [`DestMode::Fresh`] refuses an existing destination instead of
/// clobbering it, and that the refused copy leaves the old bytes alone.
#[test]
fn test_bounded_copy_fresh_refuses_existing_dst() {
    let _env = EnvGuard::new();
    let dir = scratch("fresh");
    let src = dir.join("src.db");
    let dst = dir.join("snap.db");
    std::fs::write(&src, b"new").unwrap();
    std::fs::write(&dst, b"keep").unwrap();

    let err = bounded_copy(&src, &dst, 1 << 20, DestMode::Fresh)
        .expect_err("an existing destination must be refused");

    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(&dst).unwrap(),
        b"keep",
        "the refused copy must leave the previous bytes in place"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pins that [`DestMode::Fresh`] refuses a pre-planted symlink instead of
/// writing through it.
#[test]
fn test_bounded_copy_fresh_refuses_symlink() {
    let _env = EnvGuard::new();
    let dir = scratch("fresh-link");
    let src = dir.join("src.db");
    let target = dir.join("secret.db");
    let dst = dir.join("snap.db");
    std::fs::write(&src, b"payload").unwrap();
    std::fs::write(&target, b"secret").unwrap();
    std::os::unix::fs::symlink(&target, &dst).unwrap();

    let err = bounded_copy(&src, &dst, 1 << 20, DestMode::Fresh)
        .expect_err("a planted symlink must be refused");

    assert_eq!(
        err.kind(),
        io::ErrorKind::AlreadyExists,
        "the exclusive create is what refuses the link"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"secret",
        "the symlink target must stay untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pins that [`DestMode::Overwrite`] still refuses a pre-planted symlink:
/// truncating through a link would redirect the whole snapshot elsewhere.
#[test]
fn test_bounded_copy_overwrite_refuses_symlink() {
    let _env = EnvGuard::new();
    let dir = scratch("overwrite-link");
    let src = dir.join("src.db");
    let target = dir.join("secret.db");
    let dst = dir.join("snap.db");
    std::fs::write(&src, b"payload").unwrap();
    std::fs::write(&target, b"secret").unwrap();
    std::os::unix::fs::symlink(&target, &dst).unwrap();

    bounded_copy(&src, &dst, 1 << 20, DestMode::Overwrite)
        .expect_err("overwrite must refuse a symlink too");

    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"secret",
        "the symlink target must stay untouched"
    );
    assert!(
        dst.symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink()),
        "the refused destination must still be the link that was planted"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pins that [`DestMode::Overwrite`] replaces the destination completely: a
/// shorter new copy leaves no stale tail, a missing destination is created,
/// and the result is private like any other snapshot.
#[test]
fn test_bounded_copy_overwrite_replaces_and_truncates() {
    let _env = EnvGuard::new();
    let dir = scratch("overwrite");
    let src = dir.join("src.db");
    let dst = dir.join("snap.db");
    std::fs::write(&src, b"new").unwrap();
    std::fs::write(&dst, b"a much longer previous snapshot").unwrap();
    std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o644)).unwrap();

    bounded_copy(&src, &dst, 1 << 20, DestMode::Overwrite)
        .expect("an existing snapshot is replaced");

    assert_eq!(
        std::fs::read(&dst).unwrap(),
        b"new",
        "the destination must be truncated to exactly the new bytes"
    );
    assert_eq!(
        mode_of(&dst),
        0o600,
        "a replaced snapshot must be private again"
    );

    let absent = dir.join("absent.db");
    bounded_copy(&src, &absent, 1 << 20, DestMode::Overwrite)
        .expect("overwrite creates a destination that is not there yet");
    assert_eq!(std::fs::read(&absent).unwrap(), b"new");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Pins that a source that is not a regular file is refused before it is
/// opened — a fifo would otherwise block in the open until some other
/// process decided to write to it, which is a hang, not an error — and that
/// the refusal costs nothing: no destination is ever created. The refusals
/// that land *after* the destination exists are covered by the crate's unit
/// tests, which can change a source mid-copy.
#[test]
fn test_bounded_copy_rejects_non_regular_source() {
    let _env = EnvGuard::new();
    let dir = scratch("non-regular");
    // A directory is the stand-in for a fifo here: both are refused by the
    // same check, and safe Rust cannot create a fifo to show the hang.
    let src = dir.join("a-directory.db");
    let dst = dir.join("snap.db");
    std::fs::create_dir(&src).unwrap();

    let err = bounded_copy(&src, &dst, 1 << 20, DestMode::Fresh)
        .expect_err("a source that is not a regular file must be refused");

    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("not a regular file"),
        "the refusal must name what is wrong: {err}"
    );
    assert!(
        !dst.exists(),
        "a refused source must not even create the destination"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Create `path` as a fifo, whose read-only open waits for some other process
/// to write to it instead of returning. `mkfifo(2)` is the only way to make
/// one: safe Rust has none, and `libc` is already a dependency of this crate.
fn make_fifo(path: &Path) {
    let name = CString::new(path.as_os_str().as_bytes())
        .expect("a path this file builds carries no interior nul");
    // SAFETY: `name` is nul-terminated and outlives the call; `mkfifo` only
    // creates the entry, and the mode keeps it as private as every other file
    // this crate creates.
    let rc = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(
        rc,
        0,
        "the fifo '{}' is created: {}",
        path.display(),
        io::Error::last_os_error()
    );
}

/// Pins that a source that is a fifo is refused rather than waited on: opening
/// a fifo read-only blocks until some other process opens it for writing, and
/// a copy that waits for such a writer hangs instead of failing.
///
/// The call happens on its own thread and the answer is awaited with a bounded
/// timeout, so a copy that blocks fails this test instead of the suite. The
/// blocked thread is detached rather than joined: it would never come back.
#[test]
fn test_bounded_copy_refuses_a_fifo_source() {
    let _env = EnvGuard::new();
    let dir = scratch("fifo");
    let src = dir.join("game.db");
    let dst = dir.join("snap.db");
    make_fifo(&src);

    let (sender, receiver) = mpsc::channel();
    let call_src = src.clone();
    let call_dst = dst.clone();
    let blocked = std::thread::spawn(move || {
        let _ = sender.send(bounded_copy(&call_src, &call_dst, 1 << 20, DestMode::Fresh));
    });

    let outcome = receiver.recv_timeout(FIFO_WAIT).unwrap_or_else(|err| {
        panic!(
            "bounded_copy did not answer within {FIFO_WAIT:?} ({err}): it is \
             blocked opening the fifo, waiting for a writer that never comes"
        )
    });

    let err = outcome.expect_err("a fifo source must be refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("not a regular file"),
        "the refusal must name what is wrong: {err}"
    );
    assert!(
        !dst.exists(),
        "a refused source must not even create the destination"
    );
    drop(blocked);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pins that [`signature_unchanged`] sees an idle file as unchanged and a
/// grown one as changed: that verdict is what re-checks the source after
/// the bytes were written.
#[test]
fn test_signature_unchanged_detects_change() {
    let _env = EnvGuard::new();
    let dir = scratch("signature");
    let path = dir.join("game.db");
    std::fs::write(&path, b"first").unwrap();

    let before = std::fs::metadata(&path).unwrap();
    assert!(
        signature_unchanged(&before, &std::fs::metadata(&path).unwrap()),
        "an idle file must compare equal to itself"
    );

    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("the source is appendable")
        .write_all(b"-second")
        .unwrap();
    assert!(
        !signature_unchanged(&before, &std::fs::metadata(&path).unwrap()),
        "an appended file must be seen as changed"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
