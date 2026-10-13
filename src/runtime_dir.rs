// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Private runtime directory: the rules for the temporary paths wwrpc
//! writes — the snapshot of the game database and its sidecars, and the
//! instance lock.
//!
//! [`runtime_dir`] applies them, and a path is covered only when its
//! directory comes from there:
//!
//! - Directories are `0700` and files `0600`, set after creation because a
//!   umask can only clear bits, and a strict one would otherwise leave the
//!   new path unusable. A base that has to be rebuilt gets the same
//!   treatment, one level at a time: every level of the chain that walk
//!   creates is pinned, and every level of it that was already there is
//!   left as it is.
//! - `$XDG_RUNTIME_DIR` is honored only when it is absolute and non-empty;
//!   anything else falls back to the system temp dir with a warning, under
//!   the name `wwrpc-<uid>`.
//! - An existing runtime dir is audited: a symlink, a non-directory or a
//!   directory owned by another user is refused, while a directory we own
//!   has its `0700` mode repaired rather than rejected.
//! - A file that must inherit nothing is created exclusively, and no file
//!   is ever written through a symlink; a file that must be replaced is
//!   truncated in place instead, and [`DestMode`] is the whole of that
//!   decision.
//! - [`PrivateTempDir`] is RAII: the directory and its contents are removed
//!   when the guard is dropped.
//! - [`bounded_copy`] is the only way a snapshot reaches those files: it
//!   refuses a source over the caller's byte limit, writes at most that
//!   many bytes, proves the source did not change under it, and takes the
//!   destination away again whenever it cannot finish.
//!
//! Every path the binary writes into its runtime state comes through here:
//! the daemon's snapshots and its instance lock share one [`runtime_dir`],
//! and the `--list-accounts` and `--print-activity` scratch directories are
//! [`PrivateTempDir`]s inside it. A caller must not build a snapshot or
//! scratch path by hand. The lock file keeps two rules of its own, from
//! [`SingleInstance::acquire`](crate::lock::SingleInstance::acquire): it is
//! reused rather than refused when it already exists, and it is left behind
//! when its holder goes.
//!
//! One write is deliberately outside those rules: the detection cache under
//! `$XDG_CACHE_HOME/wwrpc` (or `~/.cache/wwrpc`) belongs to
//! [`crate::detect`] and answers to none of them. It is a cache, refilled
//! from the online database whenever it is missing or stale, so nothing in
//! it has to be private.

use std::borrow::Cow;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
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

/// Bytes per read while a snapshot is copied: a page multiple that keeps a
/// 64 MiB database at about a thousand syscalls.
const COPY_BUFFER: usize = 64 * 1024;

/// The wwrpc runtime directory: `<XDG_RUNTIME_DIR>/wwrpc`, or
/// `<temp_dir>/wwrpc-<uid>` when that variable is unusable.
///
/// A new entry is created exclusively, and an existing one is audited: a
/// symlink, a non-directory or a directory owned by another user is
/// refused, while a directory we own is repaired to mode `0700` when the
/// mode drifted.
///
/// A missing base — a `XDG_RUNTIME_DIR` left behind by a login manager that
/// is gone — is rebuilt one level at a time, each new level pinned to
/// `0700` before the next is created under it, so a strict umask cannot
/// leave one of them unusable. Levels of that base that already exist are
/// not part of the repair: their permissions belong to whoever set them and
/// stay untouched.
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
            create_private_dir_chain(base).map_err(|err| {
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
        // Best effort: the directory is scratch either way, and a cleanup
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
/// is refused rather than followed or overwritten.
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

/// What [`bounded_copy`] does with a destination that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestMode {
    /// Refuse the copy unless the destination is absent, and create it
    /// exclusively (`O_EXCL`): a snapshot that must inherit nothing, a
    /// planted file or link included.
    Fresh,
    /// Replace the destination — truncate it, creating it when absent —
    /// while still refusing a symlink instead of writing through it.
    // Truncating in place is safe only because the runtime directory is never
    // emptied, so a destination this mode is handed is re-copied before the
    // next reader looks at it. That is a property of the caller that picks
    // the mode rather than of this code, and it is stated where it is kept
    // true: the `db` module docs.
    Overwrite,
}

/// Copy `src` to `dst`, writing at most `limit` bytes and refusing anything
/// that does not belong in a snapshot.
///
/// A game database is a file another process keeps writing while we read it,
/// so the copy is bounded in three places. The source is opened with
/// `O_NONBLOCK`, so a fifo standing there hands back a handle instead of
/// waiting for a writer, and everything said about the source afterwards —
/// what kind of file it is, how big it is, whether it changes — describes
/// that handle, never the path, so a source renamed or replaced mid-copy
/// cannot make the verdict a verdict about another file.
///
/// - The source is judged by that handle: it must be a regular file, and
///   over `limit` bytes is refused before the destination is created.
/// - The bytes are then read through the same handle, in chunks that never
///   read past `limit`. A source with one byte too many is refused rather
///   than silently truncated into a corrupt snapshot.
/// - It is measured again afterwards, and [`signature_unchanged`] must
///   still hold or the copy is thrown away: an old head mixed with a new
///   tail is not a snapshot.
///
/// A destination this call created — or, under [`DestMode::Overwrite`],
/// truncated — is removed on every failure; one that already existed and
/// was never opened is left alone. That removal unlinks by path, since safe
/// Rust has no unlink-by-handle, so `dst` must name nothing outside the
/// private [`runtime_dir`] this module keeps at `0700`. On success the copy
/// is `0600` whatever the umask did to it.
///
/// `src` and `dst` must be different files: under [`DestMode::Overwrite`]
/// the destination is truncated before the first byte is read, so a file
/// copied onto itself would destroy the source.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] when the source that was opened is not a
/// regular file; [`io::ErrorKind::InvalidData`] when it is over `limit` or
/// changed while it was copied; [`io::ErrorKind::AlreadyExists`] when
/// [`DestMode::Fresh`] finds a destination; plus filesystem errors from
/// measuring, opening, reading or writing either side.
pub fn bounded_copy(src: &Path, dst: &Path, limit: u64, mode: DestMode) -> io::Result<()> {
    // Both seams are no-ops for every real caller. The crate's own tests pass
    // hooks that change the source underneath a live copy at either moment:
    // the only way to reach the refusals below — and to show the one case
    // where a changing name must not matter — with no second process and no
    // sleep between two writes.
    bounded_copy_inner(src, dst, limit, mode, no_source_change, no_source_change)
}

/// The hook [`bounded_copy`] passes to both of its seams: change the source
/// at the named moment, or notice nothing.
fn no_source_change(_src: &Path) {}

/// The body of [`bounded_copy`], carrying the test seams.
///
/// `before_open` runs once, before the source is opened; `after_measure` runs
/// once, after it has been measured and before its first byte is read. A test
/// passes hooks that put something else where the source is at either named
/// moment: the only way to pin what a copy in flight does with a source that
/// stops being the one it started from, with no second process and no sleep
/// between two writes. Everything else is [`bounded_copy`].
pub(crate) fn bounded_copy_inner(
    src: &Path,
    dst: &Path,
    limit: u64,
    mode: DestMode,
    before_open: impl FnOnce(&Path),
    after_measure: impl FnOnce(&Path),
) -> io::Result<()> {
    before_open(src);
    // A fifo opened read-only blocks until some other process opens it for
    // writing, which in a long-running daemon is a hang rather than an error.
    // `O_NONBLOCK` takes that wait out of the open and hands back a handle
    // that reports its own kind, so the refusal below can be asked of the
    // handle instead of of a name the source can be replaced behind. The flag
    // then stays on the handle for good: it means nothing to a regular file.
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(src)
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot open '{}': {err}", src.display()),
            )
        })?;
    // Measured through the handle, not through the path: every verdict below
    // then describes the file the bytes actually come from, whatever happens
    // to the name while the copy runs.
    let before = source.metadata().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot inspect the open '{}': {err}", src.display()),
        )
    })?;
    if !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("'{}' is not a regular file", src.display()),
        ));
    }
    if before.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "'{}' is {} bytes, over the {limit}-byte limit",
                src.display(),
                before.len()
            ),
        ));
    }
    let mut sink = open_destination(dst, mode)?;
    // From this line on the destination is one this call created or
    // truncated, so every failure has to take it away again: a half-written
    // snapshot is worse than none, because the next reader cannot tell the
    // two apart.
    let outcome = seal_destination(&sink, dst)
        .and_then(|()| {
            after_measure(src);
            copy_capped(&mut source, &mut sink, src, dst, limit)
        })
        .and_then(|()| verify_unchanged(&source, src, &before));
    if let Err(err) = outcome {
        // The handle goes first: safe Rust has no unlink-by-handle, and the
        // path below is only safe to unlink once nothing holds the file.
        drop(sink);
        remove_destination(dst);
        return Err(err);
    }
    Ok(())
}

/// Whether two observations of the same file agree on what a copy cares
/// about: its length and its modification time. The time is read whole —
/// seconds plus the nanoseconds the filesystem records — so a rewrite inside
/// the same second is still seen wherever the filesystem keeps that much
/// precision. [`bounded_copy`] throws the copy away when this returns
/// `false`.
#[must_use]
pub fn signature_unchanged(a: &Metadata, b: &Metadata) -> bool {
    a.len() == b.len() && a.mtime() == b.mtime() && a.mtime_nsec() == b.mtime_nsec()
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
/// directory owned by another user. A directory the caller owns with a
/// drifted mode is accepted — [`runtime_dir`] repairs it.
fn audit_existing(dir: &Path) -> io::Result<()> {
    // One lookup answers the whole audit, so a directory that is swapped
    // between a type check and an owner check cannot slip through. A
    // symlink is never adopted, even when it points at a directory that
    // looks perfectly fine.
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
    let uid = current_uid();
    let owner = meta.uid();
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

/// Create every component of `base` that is missing, each as a private
/// (`0700`) directory through the exclusive `mkdir` of
/// [`create_private_dir`], and skip every component that already exists.
///
/// What `Ok(())` promises is only that every level of `base` was either
/// there already or was created here — not that `base` is now a usable
/// directory. A level that is a file, or a symlink to nowhere, is left
/// exactly as it is, so it can pass this walk and still refuse the `wwrpc`
/// entry [`runtime_dir`] creates next. That entry is what proves the base
/// is usable, not this call.
fn create_private_dir_chain(base: &Path) -> io::Result<()> {
    // Each level is pinned before the walk descends: `mkdir` takes its mode
    // through the umask, so a strict one leaves the level just created
    // without its search bit, and the next `mkdir` under it fails with
    // `EACCES` long before that level could be repaired.
    //
    // `components` yields the root first for an absolute path and nothing
    // at all for an empty one, so `prefix` is a real, resolvable path at
    // every step of the descent.
    let mut prefix = PathBuf::new();
    for component in base.components() {
        prefix.push(component);
        match create_private_dir(&prefix) {
            Ok(()) => {}
            // `mkdir` refused an entry that was already there: this code
            // created nothing here and has no say over its permissions.
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Open the destination for writing without ever following a symlink.
///
/// [`DestMode::Fresh`] creates it exclusively, so anything already there —
/// a file, a link, a directory — is refused by the kernel itself.
/// [`DestMode::Overwrite`] truncates, creating it when absent, with
/// `O_NOFOLLOW`: a link planted where the snapshot goes must not redirect
/// the whole copy somewhere else, and the check cannot race the way a
/// separate `lstat` would. Both ask for [`FILE_MODE`], which
/// [`seal_destination`] then pins exactly.
fn open_destination(dst: &Path, mode: DestMode) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).mode(FILE_MODE);
    match mode {
        DestMode::Fresh => {
            options.create_new(true);
        }
        DestMode::Overwrite => {
            options.create(true).truncate(true);
            options.custom_flags(libc::O_NOFOLLOW);
        }
    }
    options.open(dst).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot open '{}': {err}", dst.display()),
        )
    })
}

/// Pin the destination to [`FILE_MODE`] through the handle being written,
/// not through the path, so a path swapped after the open cannot redirect
/// the `chmod`. Called before the first byte lands: the umask can only
/// clear bits, so a snapshot is never readable by another account even for
/// the moment the write takes.
fn seal_destination(sink: &File, dst: &Path) -> io::Result<()> {
    sink.set_permissions(fs::Permissions::from_mode(FILE_MODE))
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot set the mode of '{}': {err}", dst.display()),
            )
        })
}

/// Copy `source` into `sink` until the source ends or `limit` bytes have
/// moved, then refuse a source that still has one byte left.
///
/// The cap lives in the reads themselves: no read is ever larger than what
/// `limit` still allows, so an oversized source can neither be buffered nor
/// written, and the extra byte is looked for on the read that follows the
/// cap instead of on a second pass over the file.
fn copy_capped(
    source: &mut File,
    sink: &mut File,
    src: &Path,
    dst: &Path,
    limit: u64,
) -> io::Result<()> {
    let mut buffer = [0u8; COPY_BUFFER];
    let mut copied: u64 = 0;
    while copied < limit {
        // What the cap still allows, at most one buffer. A `limit` wider
        // than `usize` (32-bit target, huge cap) only means "a full buffer".
        let room = usize::try_from(limit - copied)
            .unwrap_or(COPY_BUFFER)
            .min(COPY_BUFFER);
        let read = source
            .read(&mut buffer[..room])
            .map_err(|err| read_error(src, err))?;
        if read == 0 {
            return Ok(());
        }
        copied += read as u64;
        sink.write_all(&buffer[..read])
            .map_err(|err| write_error(dst, err))?;
    }
    // Exactly at the cap. One byte is enough to know the source is over it,
    // and that byte is never written: a snapshot shorter than its source
    // would be a corrupt one.
    let mut probe = [0u8; 1];
    if source
        .read(&mut probe)
        .map_err(|err| read_error(src, err))?
        != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "'{}' grew past the {limit}-byte limit while it was copied",
                src.display()
            ),
        ));
    }
    Ok(())
}

/// Re-measure the file that was just copied, through the handle that read
/// it, and refuse the copy unless it is still what [`bounded_copy`]
/// measured: a database rewritten under us must never be snapshotted as an
/// old head plus a new tail.
///
/// Comparing the handle's own stats — instead of a second `stat` of the
/// path — is what makes that verdict hold: a source renamed or replaced
/// mid-copy can no longer stand in for the file whose bytes were read.
fn verify_unchanged(source: &File, src: &Path, before: &Metadata) -> io::Result<()> {
    let after = source.metadata().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot re-inspect '{}': {err}", src.display()),
        )
    })?;
    if signature_unchanged(before, &after) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("'{}' changed while it was copied", src.display()),
    ))
}

/// Take away a destination [`bounded_copy`] created or truncated. Best
/// effort, like the [`PrivateTempDir`] drop: the copy has already failed,
/// and letting a cleanup failure hide that first error would trade a known
/// problem for an unknown one.
fn remove_destination(dst: &Path) {
    if let Err(err) = fs::remove_file(dst)
        && err.kind() != io::ErrorKind::NotFound
    {
        log::warn(format!(
            "cannot remove the partial copy '{}': {err}",
            dst.display()
        ));
    }
}

/// A read failure, tagged with the source it came from.
fn read_error(src: &Path, err: io::Error) -> io::Error {
    io::Error::new(
        err.kind(),
        format!("cannot read from '{}': {err}", src.display()),
    )
}

/// A write failure, tagged with the destination it went to.
fn write_error(dst: &Path, err: io::Error) -> io::Error {
    io::Error::new(
        err.kind(),
        format!("cannot write '{}': {err}", dst.display()),
    )
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
