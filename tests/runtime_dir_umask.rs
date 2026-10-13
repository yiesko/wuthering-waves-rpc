// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Integration test for the one thing the process umask decides about
//! [`runtime_dir`]: a strict mask must not keep it from starting.
//!
//! It is alone in this file by design. `umask` belongs to the process and
//! `cargo test` runs the tests of one binary on many threads, so a `0777`
//! set next to other tests would strip the permissions of whatever they
//! create while it was in effect. Each `tests/*.rs` file is a binary of
//! its own, so here the strict mask spans exactly one call.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use wwrpc::runtime_dir::runtime_dir;

/// Environment variable this test points at its sandbox base.
const XDG: &str = "XDG_RUNTIME_DIR";

/// A mask strict enough that `mkdir` leaves a directory with no search bit
/// at all. Only the `chmod` the module does after creating a path itself
/// can make such a directory usable again.
const STRICT_UMASK: libc::mode_t = 0o777;

/// Mode the fixture root is planted with. It is a value the runtime
/// directory has no business touching, so finding it unchanged afterwards
/// is the proof that an already-existing component was left alone.
const ROOT_MODE: u32 = 0o755;

/// The umask this process had, restored when the guard is dropped.
struct UmaskGuard(libc::mode_t);

impl UmaskGuard {
    /// Make `mask` the process umask until the returned guard drops.
    fn set(mask: libc::mode_t) -> Self {
        // SAFETY: `umask` reads and writes one bit mask and nothing else;
        // the guard puts back the value it was handed on every path out,
        // unwinding included.
        Self(unsafe { libc::umask(mask) })
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: restoring a value `umask` itself reported.
        unsafe { libc::umask(self.0) };
    }
}

/// Points `XDG_RUNTIME_DIR` at a path and puts the previous value back when
/// the guard is dropped.
struct XdgGuard(Option<OsString>);

impl XdgGuard {
    /// Point `XDG_RUNTIME_DIR` at `value`.
    fn set(value: &Path) -> Self {
        let previous = std::env::var_os(XDG);
        // SAFETY: one test runs in this binary, so no other thread reads or
        // writes the environment while the guard is alive.
        unsafe { std::env::set_var(XDG, value) };
        Self(previous)
    }
}

impl Drop for XdgGuard {
    fn drop(&mut self) {
        // SAFETY: same invariant as [`XdgGuard::set`].
        unsafe {
            match &self.0 {
                Some(value) => std::env::set_var(XDG, value),
                None => std::env::remove_var(XDG),
            }
        }
    }
}

/// Permission bits (`mode & 0o777`) of `path`.
fn mode_of(path: &Path) -> u32 {
    fs::metadata(path)
        .unwrap_or_else(|err| panic!("stat '{}': {err}", path.display()))
        .mode()
        & 0o777
}

/// Permission bits of `path`, or `absent`: a failure message has to be
/// able to explain a tree that came out half-built.
fn mode_report(path: &Path) -> String {
    match fs::symlink_metadata(path) {
        Ok(meta) => format!("{:04o}", meta.mode() & 0o777),
        Err(_) => String::from("absent"),
    }
}

/// The permission bits of every path in `chain`, one per line.
fn mode_chain(chain: &[&Path]) -> String {
    chain
        .iter()
        .map(|path| format!("  {} = {}", path.display(), mode_report(path)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Remove `path` and everything under it, reopening a directory that was
/// left unsearchable first. A symlink is removed, never descended into.
///
/// `remove_dir_all` alone cannot clear the tree a failing run leaves
/// behind: a level left at `0000` cannot be descended into by anyone, its
/// owner included, and only the owner can hand search back.
fn remove_tree(path: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    // `symlink_metadata` reports a link as not a directory whatever it
    // points at, so this one test covers both cases.
    if !meta.is_dir() {
        return fs::remove_file(path);
    }
    // Owner read/write/execute is what the walk below needs: search to read
    // the entries, write to remove them.
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    for entry in fs::read_dir(path)? {
        remove_tree(&entry?.path())?;
    }
    fs::remove_dir(path)
}

/// The fixture tree, cleared when it is built and removed again when the
/// guard drops — after a failing assertion as much as after a passing one.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// Build an empty fixture root carrying [`ROOT_MODE`], named after this
    /// process.
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("wwrpc-umask-{}", std::process::id()));
        // Whatever an earlier run of this pid left behind goes first, and
        // loudly: starting on top of one would turn a stale fixture into a
        // permission error this test could not explain.
        remove_tree(&root)
            .unwrap_or_else(|err| panic!("cannot clear the fixture '{}': {err}", root.display()));
        fs::create_dir(&root).expect("the fixture root is created");
        fs::set_permissions(&root, fs::Permissions::from_mode(ROOT_MODE))
            .expect("the fixture root carries a mode of its own");
        Self { root }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // `drop` cannot report a failure as a test failure, so a cleanup
        // that still does not go through says so here instead of leaving a
        // tree behind without a word.
        if let Err(err) = remove_tree(&self.root) {
            eprintln!("cannot remove the fixture '{}': {err}", self.root.display());
        }
    }
}

/// Pins that a missing runtime base is rebuilt under a strict umask, and
/// that rebuilding it stays the module's own business: every component it
/// creates ends at `0700`, and every component that already existed keeps
/// the mode it came with.
#[test]
fn test_runtime_dir_rebuilds_base_under_strict_umask() {
    let temp = std::env::temp_dir();
    let temp_mode = mode_of(&temp);
    let fixture = Fixture::new();
    let root = fixture.root.clone();
    // Two levels below the fixture, with the fixture itself already there.
    // Under a strict mask the failure is in the level above, not the one
    // below: a freshly created directory comes out without its search bit,
    // so it is the create beneath it that fails. The shell's `mkdir -p`
    // adds `u+wx` to what it creates and recovers from that; a
    // per-component `mkdir` does not.
    let base = root.join("missing").join("root");
    let dir = base.join("wwrpc");
    let _xdg = XdgGuard::set(&base);

    let outcome = {
        let _umask = UmaskGuard::set(STRICT_UMASK);
        runtime_dir()
    };

    let reported = mode_chain(&[
        root.as_path(),
        base.parent().expect("a nested base has a parent"),
        base.as_path(),
        dir.as_path(),
    ]);
    let created = outcome.unwrap_or_else(|err| {
        panic!("a strict umask must not keep the runtime dir from starting: {err}\n{reported}")
    });
    assert_eq!(
        created, dir,
        "the runtime dir must sit directly under the configured base\n{reported}"
    );
    for path in [
        base.parent().expect("a nested base has a parent"),
        base.as_path(),
        created.as_path(),
    ] {
        assert_eq!(
            mode_of(path),
            0o700,
            "every component this call created must be owner-only\n{reported}"
        );
    }
    assert_eq!(
        mode_of(&root),
        ROOT_MODE,
        "a component that already existed must be left exactly as it was\n{reported}"
    );
    assert_eq!(
        mode_of(&temp),
        temp_mode,
        "a shared ancestor the code did not create must never be chmod'ed"
    );
}
