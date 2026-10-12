// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Private runtime directory: the rules for every temporary path wwrpc
//! writes — the snapshot of the game database, its sidecars, the instance
//! lock.
//!
//! All of them belong in one guarded per-user runtime directory
//! (`$XDG_RUNTIME_DIR`, tmpfs by spec) that no other account can read or
//! pre-empt:
//!
//! - Directories are `0700` and files `0600`, enforced after creation so
//!   even a strange umask cannot widen them.
//! - `$XDG_RUNTIME_DIR` counts only when it is absolute; empty or relative
//!   values fall back to the system temp dir with a warning, because shared
//!   `/tmp` is neither private nor tmpfs. That fallback names the directory
//!   after the uid (`wwrpc-<uid>`), so one account cannot squat the name
//!   and lock every other user out of it.
//! - An existing runtime dir is audited: a symlink, a non-directory or a
//!   directory owned by another user is refused (anyone can pre-create an
//!   entry in shared `/tmp`), and a directory we own has its `0700` mode
//!   repaired when it drifted.
//! - Every path is created exclusively (`O_EXCL` / `create_new`) and a
//!   pre-existing symlink is refused instead of followed, so a hostile
//!   shared temp dir cannot redirect a snapshot elsewhere.
//! - [`PrivateTempDir`] is RAII: the directory and its contents are removed
//!   when the guard is dropped.
//!
//! Creating a temp path is this module's job alone: callers must not build
//! private paths on their own.

use std::borrow::Cow;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::log;

/// Mode of every directory this module creates: owner read/write/execute.
const DIR_MODE: u32 = 0o700;

/// Mode of every file this module creates: owner read/write.
const FILE_MODE: u32 = 0o600;

/// Name of the wwrpc directory inside the runtime base.
const RUNTIME_SUBDIR: &str = "wwrpc";

/// Environment variable holding the per-user tmpfs runtime directory.
const XDG_RUNTIME_DIR: &str = "XDG_RUNTIME_DIR";

/// Candidate names [`PrivateTempDir::new`] tries before giving up. Real
/// collisions end on the first try; the budget only bounds a pathological
/// (or hostile) runtime dir.
const NAME_ATTEMPTS: u32 = 64;

/// The wwrpc runtime directory: `<XDG_RUNTIME_DIR>/wwrpc`, or
/// `<temp_dir>/wwrpc-<uid>` when that variable is unusable.
///
/// The entry is created exclusively and then audited: a symlink, a
/// non-directory or a directory owned by another user is refused, which is
/// what makes the shared `/tmp` fallback safe (anyone can pre-create an
/// entry there). A directory the caller owns is repaired to mode `0700`
/// when the mode drifted.
///
/// # Errors
///
/// Filesystem errors while creating or inspecting the entry, plus a
/// runtime path that is a symlink, not a directory, or owned by another
/// user.
pub fn runtime_dir() -> io::Result<PathBuf> {
    let base = runtime_base();
    let dir = base.path().join(base.subdir());
    match create_private_dir(&dir) {
        // Fresh create: mkdir's mode plus the set_mode inside
        // `create_private_dir` already leave it exactly `0700`.
        Ok(()) => return Ok(dir),
        // A set-but-stale base (login manager gone) is rebuilt once, then
        // the exclusive create is retried.
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let base = base.path();
            fs::create_dir_all(base).map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("cannot create runtime base '{}': {err}", base.display()),
                )
            })?;
            match create_private_dir(&dir) {
                Ok(()) => return Ok(dir),
                // Another run of ours got there first: repair below.
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
        }
        // Already there — ours or hostile: the audit below decides.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    // The entry already existed. The audit refuses anything that is not a
    // directory we own; once it has passed, a drifted mode is repaired
    // rather than rejected, so one bad startup cannot disable the service.
    audit_existing(&dir)?;
    set_mode(&dir, DIR_MODE)?;
    Ok(dir)
}

/// A private (`0700`) directory that removes itself on drop.
///
/// The name mixes pid, an attempt counter and a nanosecond timestamp, and
/// the create is exclusive, so concurrent runs never share one directory.
pub struct PrivateTempDir {
    path: PathBuf,
}

impl PrivateTempDir {
    /// Create a fresh private directory inside [`runtime_dir`], with `tag`
    /// embedded in its name for diagnostics.
    ///
    /// # Errors
    ///
    /// Filesystem errors from the runtime directory or the exclusive create;
    /// [`io::ErrorKind::AlreadyExists`] after every candidate name was taken
    /// (the runtime dir would have to be full of `wwrpc-{tag}-{pid}-` rows).
    pub fn new(tag: &str) -> io::Result<Self> {
        let base = runtime_dir()?;
        let pid = std::process::id();
        let stamp = nanos();
        for attempt in 0..NAME_ATTEMPTS {
            let path = base.join(format!("wwrpc-{tag}-{pid}-{attempt}-{stamp}"));
            match create_private_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                // Taken by a concurrent run, or a stale entry from a crashed
                // one: the next candidate differs and nothing is reused.
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("no free runtime dir name for tag '{tag}' in {NAME_ATTEMPTS} tries"),
        ))
    }

    /// Path of the private directory, valid until this guard is dropped.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateTempDir {
    fn drop(&mut self) {
        // Best effort: a reboot already discarded the tmpfs, and a cleanup
        // failure must never turn a successful run into a panic.
        if let Err(err) = fs::remove_dir_all(&self.path)
            && err.kind() != io::ErrorKind::NotFound
        {
            log::warn(format!(
                "cannot remove temp dir '{}': {err}",
                self.path.display()
            ));
        }
    }
}

/// Write `contents` to `path` as a private (`0600`) file, created
/// exclusively.
///
/// A destination that already exists — a plain file as much as a symlink —
/// is refused instead of followed or overwritten, so nobody can pre-plant a
/// link that turns a snapshot write into a write elsewhere.
///
/// # Errors
///
/// [`io::ErrorKind::AlreadyExists`] when `path` exists, plus filesystem
/// errors from creating or writing it.
pub fn create_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    // Purely diagnostic: the `create_new` below opens with `O_EXCL`, which
    // is the real guard and already refuses any pre-existing destination.
    // This check only buys the clearer "refusing to write through it"
    // message for a planted symlink.
    if fs::symlink_metadata(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "'{}' already exists; refusing to write through it",
                path.display()
            ),
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot create '{}': {err}", path.display()),
            )
        })?;
    // Tighten the mode before the contents land on disk: the mode argument
    // above is masked by the umask, and a snapshot is never public, not
    // even for the moment the write takes.
    file.set_permissions(fs::Permissions::from_mode(FILE_MODE))
        .and_then(|()| file.write_all(contents))
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot write '{}': {err}", path.display()),
            )
        })
}

/// Where the runtime directory lives, and how much of that base is trusted.
enum RuntimeBase {
    /// `$XDG_RUNTIME_DIR`, absolute and non-empty: the spec's per-user
    /// tmpfs, honored verbatim.
    Xdg(PathBuf),
    /// `std::env::temp_dir()`: shared, so neither private nor tmpfs — the
    /// reason [`runtime_base`] warns about it.
    Fallback(PathBuf),
}

impl RuntimeBase {
    /// The base directory itself.
    fn path(&self) -> &Path {
        match self {
            Self::Xdg(path) | Self::Fallback(path) => path,
        }
    }

    /// Directory name inside [`RuntimeBase::path`]. A valid XDG base keeps
    /// `wwrpc` verbatim — honoring the operator's value is the spec, and a
    /// foreign-owned entry under it is the XDG contract failing closed. The
    /// shared fallback is per-user instead: any local account can create an
    /// entry in a shared temp dir, so a single name would let the first one
    /// there lock every other user out of it entirely.
    fn subdir(&self) -> Cow<'_, Path> {
        match self {
            Self::Xdg(_) => Cow::Borrowed(Path::new(RUNTIME_SUBDIR)),
            Self::Fallback(_) => {
                Cow::Owned(PathBuf::from(format!("{RUNTIME_SUBDIR}-{}", current_uid())))
            }
        }
    }
}

/// Resolve the runtime base: `$XDG_RUNTIME_DIR` when it is absolute and
/// non-empty, otherwise the system temp dir with a warning.
fn runtime_base() -> RuntimeBase {
    if let Some(raw) = std::env::var_os(XDG_RUNTIME_DIR)
        && !raw.is_empty()
        && Path::new(&raw).is_absolute()
    {
        return RuntimeBase::Xdg(PathBuf::from(raw));
    }
    let fallback = std::env::temp_dir();
    log::warn(format!(
        "{XDG_RUNTIME_DIR} is unset or unusable; using '{}' instead: \
         a shared temp dir is neither private nor tmpfs",
        fallback.display()
    ));
    RuntimeBase::Fallback(fallback)
}

/// Refuse a runtime directory that is not safe to use: a symlink (the
/// pre-planted entry of a hostile shared `/tmp`), a non-directory, or a
/// directory owned by another user (`stat` through [`owner_uid`], so a
/// link cannot hide a foreign owner). A directory the caller owns with a
/// drifted mode is accepted — [`runtime_dir`] repairs it.
fn audit_existing(dir: &Path) -> io::Result<()> {
    // lstat first: a symlink is never adopted, even when it points at a
    // directory that looks perfectly fine.
    let meta = fs::symlink_metadata(dir).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot inspect runtime dir '{}': {err}", dir.display()),
        )
    })?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("runtime dir '{}' is a symlink; refusing it", dir.display()),
        ));
    }
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("runtime dir path '{}' is not a directory", dir.display()),
        ));
    }
    let owner = owner_uid(dir).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cannot tell who owns runtime dir '{}'", dir.display()),
        )
    })?;
    let uid = current_uid();
    if owner != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "runtime dir '{}' is owned by uid {owner}, not uid {uid}",
                dir.display()
            ),
        ));
    }
    let mode = meta.mode() & 0o777;
    if mode != DIR_MODE {
        log::warn(format!(
            "runtime dir '{}' has mode {mode:04o}, expected {DIR_MODE:04o}: repairing it",
            dir.display()
        ));
    }
    Ok(())
}

/// Create one directory with mode `0700`, exclusively. The create is a plain
/// `mkdir(2)`, which refuses to adopt an entry that already exists — a
/// symlink included — so a hostile runtime dir can never be followed here.
fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .mode(DIR_MODE)
        .create(path)
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot create '{}': {err}", path.display()),
            )
        })?;
    // mkdir's mode is masked by the umask; enforce the exact value.
    set_mode(path, DIR_MODE)
}

/// Owner uid of `path` as `libc::stat` reports it (following symlinks, so
/// the value describes the effective file). `None` when the path cannot be
/// expressed as a C string or the call fails.
fn owner_uid(path: &Path) -> Option<u32> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `libc::stat` is a plain-old-data struct of integer fields, so
    // an all-zero bit pattern is valid, and the slot is a local this thread
    // owns. `libc::stat` overwrites every field it fills before returning,
    // and `st_uid` is only read when the call reported success (`rc == 0`),
    // so no uninitialized field is ever observed.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a NUL-terminated buffer that stays alive for the
    // whole call, and `stat` is a stack slot of exactly the type libc
    // writes into; nothing else aliases either.
    let rc = unsafe { libc::stat(c_path.as_ptr(), &mut stat) };
    (rc == 0).then_some(stat.st_uid)
}

/// Uid of the running process.
fn current_uid() -> u32 {
    // SAFETY: getuid takes no arguments and only reports the caller's uid.
    unsafe { libc::getuid() }
}

/// Set the permission bits of `path` (following symlinks). Called right
/// after creation because the umask can only clear bits: a hostile `0777`
/// umask would otherwise leave the new path unusable.
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Nanoseconds since the Unix epoch: a cheap, unpredictable suffix that
/// keeps generated names apart without pulling in an RNG.
fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0)
}
