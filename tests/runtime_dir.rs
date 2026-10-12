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

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use std::os::unix::fs::{MetadataExt, PermissionsExt};

use wwrpc::runtime_dir::{PrivateTempDir, create_private_file, runtime_dir};

/// Serializes the whole file: each test reads or writes
/// `XDG_RUNTIME_DIR`, directly or through [`runtime_dir`].
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Holds [`ENV_LOCK`] and restores `XDG_RUNTIME_DIR` to its previous value
/// on drop, so a test may set, blank or unset it freely.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    previous: Option<OsString>,
}

impl EnvGuard {
    /// Acquire the file-wide env lock and snapshot `XDG_RUNTIME_DIR`.
    fn new() -> Self {
        // A panicking test must not wedge every later test in this file.
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self {
            _lock: lock,
            previous: std::env::var_os("XDG_RUNTIME_DIR"),
        }
    }

    /// Point `XDG_RUNTIME_DIR` at `value` for the rest of the test.
    fn set(&self, value: &str) {
        // SAFETY: only this guard mutates the env (the file-wide lock is
        // held for the test's whole life) and the value only steers path
        // selection inside `runtime_dir`.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", value) };
    }

    /// Unset `XDG_RUNTIME_DIR` for the rest of the test.
    fn clear(&self) {
        // SAFETY: same invariant as [`EnvGuard::set`].
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: restoring the snapshot taken before the test ran, still
        // under the lock this guard holds.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }
}

/// Per-process fixture directory; unique per tag so the tests of this file
/// never share state.
fn fixture(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("wwrpc-rt-{}-{tag}", std::process::id()))
}

/// Permission bits (`mode & 0o777`) of `path`, following symlinks as the
/// implementation does.
fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|err| panic!("stat '{}': {err}", path.display()))
        .mode()
        & 0o777
}

/// Pins that an absolute, valid `XDG_RUNTIME_DIR` yields
/// `<base>/wwrpc` as an owner-only (0700) directory.
#[test]
fn test_runtime_dir_creates_private_dir() {
    let env = EnvGuard::new();
    let base = fixture("valid");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    env.set(base.to_str().expect("temp dir is utf-8"));

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
/// result is an absolute fallback path, and nothing is created relative to
/// the working directory.
#[test]
fn test_runtime_dir_rejects_empty_and_relative() {
    let env = EnvGuard::new();

    env.set("");
    let dir = runtime_dir().expect("an empty base must fall back, not fail");
    assert!(
        dir.is_absolute(),
        "an empty base must not be used: '{}'",
        dir.display()
    );

    env.set("relative/path");
    let dir = runtime_dir().expect("a relative base must fall back, not fail");
    assert!(
        dir.is_absolute(),
        "a relative base must not be used: '{}'",
        dir.display()
    );
    assert!(
        !Path::new("wwrpc").exists(),
        "a relative base must not create ./wwrpc in the working directory"
    );
}

/// Pins that with no `XDG_RUNTIME_DIR` at all the module still returns the
/// system temp dir fallback, absolute and under `temp_dir()`.
#[test]
fn test_runtime_dir_falls_back_without_env() {
    let env = EnvGuard::new();
    env.clear();

    let dir = runtime_dir().expect("a missing base falls back to temp_dir");

    assert_eq!(dir, std::env::temp_dir().join("wwrpc"));
    assert!(
        dir.is_absolute(),
        "the fallback must be absolute: '{}'",
        dir.display()
    );
}

/// Pins that a held [`PrivateTempDir`] exists, is 0700 and carries the
/// `wwrpc-<tag>-` name shape, and that dropping it removes the directory.
#[test]
fn test_private_temp_dir_is_private_and_cleaned() {
    let env = EnvGuard::new();
    let base = fixture("private");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    env.set(base.to_str().expect("temp dir is utf-8"));

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
    env.set(base.to_str().expect("temp dir is utf-8"));

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
/// pre-planted symlink, whose target must stay untouched.
#[test]
fn test_create_private_file_mode_and_symlink_refusal() {
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
    env.set(base.to_str().expect("temp dir is utf-8"));

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
/// being rejected, and that a missing base (a stale-but-set
/// `XDG_RUNTIME_DIR`) is rebuilt rather than becoming a hard failure.
#[test]
fn test_runtime_dir_repairs_mode_and_missing_base() {
    let env = EnvGuard::new();

    let loose = fixture("loose");
    let _ = std::fs::remove_dir_all(&loose);
    std::fs::create_dir_all(loose.join("wwrpc")).unwrap();
    std::fs::set_permissions(loose.join("wwrpc"), std::fs::Permissions::from_mode(0o777)).unwrap();
    env.set(loose.to_str().expect("temp dir is utf-8"));
    let dir = runtime_dir().expect("our own loose dir is repaired, not refused");
    assert_eq!(mode_of(&dir), 0o700, "the mode drifts back to owner-only");

    let stale = fixture("stale/missing/root");
    let _ = std::fs::remove_dir_all(fixture("stale"));
    env.set(stale.to_str().expect("temp dir is utf-8"));
    let dir = runtime_dir().expect("a missing base is rebuilt");
    assert_eq!(dir, stale.join("wwrpc"));
    assert_eq!(mode_of(&dir), 0o700);

    let _ = std::fs::remove_dir_all(fixture("stale"));
}
