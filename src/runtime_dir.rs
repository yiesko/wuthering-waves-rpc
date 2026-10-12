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
//! - [`bounded_copy`] is the only way a snapshot reaches those files: it
//!   refuses a source over the caller's byte limit, writes at most that
//!   many bytes, proves the source did not change under it, and takes the
//!   destination away again whenever it cannot finish.
//!
//! Creating a temp path is this module's job alone: callers must not build
//! private paths on their own.

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

/// What [`bounded_copy`] does with a destination that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestMode {
    /// Refuse the copy unless the destination is absent, and create it
    /// exclusively (`O_EXCL`): a snapshot that must inherit nothing, a
    /// planted file or link included.
    Fresh,
    /// Replace the destination — truncate it, creating it when absent —
    /// while still refusing a symlink instead of writing through it.
    Overwrite,
}

/// Copy `src` to `dst`, writing at most `limit` bytes and refusing anything
/// that does not belong in a snapshot.
///
/// The copy is bounded in three places, because a game database is a file
/// another process keeps writing while we read it:
///
/// - The source is measured first, through the handle the bytes will come
///   from: over `limit` bytes is refused before the destination is created,
///   so a runaway file costs nothing.
/// - The bytes are read through that same handle, in chunks that never read
///   past `limit` — never with an unbounded `fs::copy`. A source with one
///   byte too many is refused rather than silently truncated into a corrupt
///   snapshot.
/// - It is measured again afterwards: [`signature_unchanged`] must still
///   hold, or the snapshot mixes an old head with a new tail and is thrown
///   away.
///
/// Both measurements come from the handle, never from the path: a path read
/// before the open describes a file that may already be a different one. The
/// source's *kind* is the one thing checked from the path, and that check runs
/// before the open, because opening a fifo blocks until a writer appears — in
/// a long-running daemon, a hang rather than an error.
///
/// A destination that this call created — or, under [`DestMode::Overwrite`],
/// truncated — is removed on every failure: a half-written snapshot is
/// worse than none, because the next reader cannot tell the two apart. The
/// removal unlinks by path, right after the write handle is dropped, since
/// safe Rust has no unlink-by-handle; callers of this module write into the
/// [`runtime_dir`] it keeps at `0700`, so that path names nothing outside
/// our own snapshot. A destination that already existed and was never
/// opened is left alone. On success the copy is private (`0600`) whatever
/// the umask did to it.
///
/// `src` and `dst` must be different files: under [`DestMode::Overwrite`]
/// the destination is truncated before the first byte is read, so a file
/// copied onto itself would destroy the source.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] when the source is not a regular file;
/// [`io::ErrorKind::InvalidData`] when it is over `limit` or changed while
/// it was copied; [`io::ErrorKind::AlreadyExists`] when [`DestMode::Fresh`]
/// finds a destination; plus filesystem errors from measuring, opening,
/// reading or writing either side.
pub fn bounded_copy(src: &Path, dst: &Path, limit: u64, mode: DestMode) -> io::Result<()> {
    // `after_measure` is the no-op below for every real caller. The crate's
    // own tests pass one that changes the source underneath a live copy:
    // the only way to reach the two refusals below — and to show the one
    // case where a changing name must not matter — with no second process
    // and no sleep between two writes.
    bounded_copy_inner(src, dst, limit, mode, no_source_change)
}

/// The hook [`bounded_copy`] passes: measure, copy, notice nothing.
fn no_source_change(_src: &Path) {}

/// The body of [`bounded_copy`], carrying the test seam.
///
/// `after_measure` runs once, after the source has been measured and before
/// its first byte is read, so a test can change the source underneath a copy
/// in flight and pin what happens next. Everything else is [`bounded_copy`].
pub(crate) fn bounded_copy_inner(
    src: &Path,
    dst: &Path,
    limit: u64,
    mode: DestMode,
    after_measure: impl FnOnce(&Path),
) -> io::Result<()> {
    check_regular_file(src)?;
    let mut source = File::open(src).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot open '{}': {err}", src.display()),
        )
    })?;
    // Measured through the handle, not through the path: the verdict below
    // then describes the file the bytes actually come from, whatever happens
    // to the name while the copy runs.
    let before = source.metadata().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot inspect the open '{}': {err}", src.display()),
        )
    })?;
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
    // truncated, so every failure has to take it away again.
    let outcome = seal_destination(&sink, dst)
        .and_then(|()| {
            after_measure(src);
            copy_capped(&mut source, &mut sink, src, dst, limit)
        })
        .and_then(|()| verify_unchanged(&source, src, &before));
    if let Err(err) = outcome {
        drop(sink);
        remove_destination(dst);
        return Err(err);
    }
    Ok(())
}

/// Refuse a source that is not a plain file, before the source is opened:
/// opening a fifo blocks until a writer appears, which in a long-running
/// daemon is a hang rather than an error.
///
/// How big the source is, and whether it changes, are deliberately not
/// decided here — those are questions about the file the bytes are read from,
/// and [`bounded_copy`] asks them of the handle it holds.
fn check_regular_file(src: &Path) -> io::Result<()> {
    let stat = fs::metadata(src).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot inspect '{}': {err}", src.display()),
        )
    })?;
    if stat.is_file() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("'{}' is not a regular file", src.display()),
    ))
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
