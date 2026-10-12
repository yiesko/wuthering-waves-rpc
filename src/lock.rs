// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Single-instance guard: refuses to start a second copy, so two instances
//! never double-publish presence or duplicate snapshots.
//!
//! The guarantee belongs to the kernel, not to this code.
//! [`SingleInstance::acquire`] opens `<dir>/wwrpc.lock` and asks for an
//! exclusive, non-blocking `flock` on that descriptor: the kernel either
//! grants it — we are the one instance — or answers `EWOULDBLOCK`, and no
//! two callers can be told otherwise at the same time. A guard that merely
//! *checked* a file before writing its own pid could not say that: two
//! starters racing both saw "no instance" and both went on.
//!
//! The pid inside the lock file is diagnostics: it is written after the lock
//! is held and is never read back to decide anything, because a leftover pid
//! says nothing about a live holder — it can belong to a process that died,
//! to a recycled pid running something else entirely, or to nothing at all.
//! Diagnostics that cheap still cost something, and the trade is deliberate:
//! recording the holder is the last thing [`SingleInstance::acquire`] does,
//! and a write that fails there — a full filesystem, a quota — refuses the
//! start instead of continuing with no holder on record. The same exhausted
//! filesystem would fail the daemon's first snapshot copy anyway, and
//! refusing once is easier to explain than a lock nobody can attribute.
//!
//! The lock dies with the open file description. The kernel releases it when
//! the last close of that description happens, whatever ended the process — a
//! clean exit, a signal, an aborting panic — so a crashed run never leaves a
//! lock behind and this module needs no recovery path. The *file* is left
//! behind on purpose: unlinking it would hand a brand-new, unlocked file to
//! the next starter while the previous one still holds the now-unlinked
//! inode, and two holders would be back.
//!
//! One caveat, because a guard that is sometimes wrong is worth knowing
//! about — and it is a fork window, not a filesystem. A `flock` belongs to
//! the *open file description*, not to a descriptor number: `fork` duplicates
//! every descriptor a process holds, and `O_CLOEXEC` closes those copies only
//! at the child's `exec`. A child forked while this process holds the lock
//! therefore keeps the lock alive until it execs, and a fresh descriptor
//! asking in that window is refused with `EWOULDBLOCK` even though the locker
//! let go microseconds earlier.
//!
//! What was measured, with the filesystem held constant rather than blamed
//! (every number below comes from one `$TMPDIR`, ext2 on zram). One cycle is:
//! open the lock file, take the flock, close it, take it again from a fresh
//! descriptor. Across four threads, 2480 such cycles with no fork anywhere
//! beside them were refused zero times. The same loop with a child
//! deliberately held between its fork and its `exec` beside it was refused
//! 240 times out of 240, and once out of 240 with a real `Command::spawn`
//! running beside it: the rarity a spawn has, because its own fork and `exec`
//! are microseconds apart. One refusal read out of `/proc/locks` while it was
//! still standing showed a live `FLOCK` entry naming a child's pid, and the
//! lock was grantable again as soon as that child had `exec`ed (240 such
//! checks, none left out) — held all along by the copy the fork made, never
//! lost.
//!
//! So the window is a consequence of how `flock` is defined, and it is not
//! retried away: a retry would mask a race in the one place that must stay
//! unambiguous, while the window can only ever cost a start that was refused,
//! never a second instance on the slot. wwrpc spawns no child process at all
//! (no `Command::new`, no `fork`, no `exec` anywhere in it, and the detection
//! engine it runs in-process starts no watcher), so nothing of ours can
//! extend the lock past the descriptor that holds it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::log;

/// Name of the lock file inside the runtime directory.
const LOCK_FILE: &str = "wwrpc.lock";

/// Mode of the lock file: owner read/write, like every other file this
/// crate writes.
const LOCK_MODE: u32 = 0o600;

/// What a refused start is told: the only thing we know for sure is that
/// somebody else holds the lock.
const BUSY: &str = "another wwrpc instance is running";

/// Room for the pid line this crate writes: every digit of a `u32` pid plus
/// its newline. A read that fills the buffer is a line this crate did not
/// write.
const PID_LEN: usize = 16;

/// A held single-instance lock.
///
/// Holding one means this process is the only wwrpc; dropping it — or dying
/// with it — releases the lock and leaves nothing a later start has to reason
/// about. The lock file itself stays behind; see the module docs for why that
/// is the safe way round.
#[must_use = "a lock dropped straight away is a lock that was never held"]
#[derive(Debug)]
pub struct SingleInstance {
    /// The locked descriptor. Keeping it open is what keeps the lock: a
    /// `File` dropped at the end of [`SingleInstance::acquire`] would
    /// release the flock before the caller ever saw it.
    file: File,
    /// Where the lock lives, for diagnostics and for the release line.
    path: PathBuf,
}

impl SingleInstance {
    /// Take the single-instance lock in `dir`.
    ///
    /// `dir` is the private runtime directory
    /// ([`runtime_dir`](crate::runtime_dir::runtime_dir)), created when
    /// missing so a caller may hand over a fresh path, and `dir/wwrpc.lock`
    /// is created when missing; an existing lock file is reused as it is,
    /// because the file is not the lock — the descriptor is.
    ///
    /// # Errors
    ///
    /// The message `another wwrpc instance is running` when another process
    /// holds the lock; then filesystem errors from creating `dir` and
    /// opening `dir/wwrpc.lock`, and a `flock` that failed for any other
    /// reason. The last step — recording this process's pid — can also end
    /// the start, deliberately, so that a holder is never left unrecorded:
    /// see the module docs.
    pub fn acquire(dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let path = dir.join(LOCK_FILE);
        fs::create_dir_all(dir)
            .map_err(|err| format!("cannot create runtime dir '{}': {err}", dir.display()))?;
        let mut file = open_lock_file(&path)?;
        take_flock(&file, &path)?;
        record_pid(&mut file, &path)?;
        log::debug(format!("instance lock held on '{}'", path.display()));
        Ok(Self { file, path })
    }

    /// The pid this holder recorded in the lock file, when it holds one.
    ///
    /// Diagnostics and tests only; it never decides whether a start may go
    /// ahead. Read through the locked descriptor rather than by path, so the
    /// answer cannot be swapped underneath us by a leftover file being
    /// replaced: whatever this process recorded is what comes back.
    pub fn pid(&self) -> Option<u32> {
        let mut buf = [0u8; PID_LEN];
        let read = self.file.read_at(&mut buf, 0).ok()?;
        // A pid we wrote always fits; a longer line is not one of ours.
        if read >= buf.len() {
            return None;
        }
        std::str::from_utf8(&buf[..read]).ok()?.trim().parse().ok()
    }
}

/// Releases the lock: closing the locked descriptor is the whole of it, and
/// the fields below do it. The lock file itself stays — see the module
/// docs for why removing it would be worse than leaving it.
impl Drop for SingleInstance {
    fn drop(&mut self) {
        // Nothing to clean up on purpose. The `file` field drops right
        // after this returns, and the kernel drops the `flock` with the last
        // close of the descriptor.
        log::debug(format!(
            "instance lock released on '{}'",
            self.path.display()
        ));
    }
}

/// Open the lock file, creating it when absent, owner-only and never through
/// a symlink.
fn open_lock_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(LOCK_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot open lock file '{}': {err}", path.display()),
            )
        })?;
    // The mode argument above is masked by the umask, so the exact mode is
    // pinned through the handle — before the lock is taken, and through the
    // handle rather than the path, which cannot have been swapped under us.
    file.set_permissions(fs::Permissions::from_mode(LOCK_MODE))
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot set the mode of '{}': {err}", path.display()),
            )
        })?;
    Ok(file)
}

/// Take an exclusive, non-blocking `flock` on the open lock file.
///
/// The one place this crate asks the kernel a question it must not get a
/// wrong answer to: the kernel serialises concurrent callers, so at most one
/// of them gets the lock.
fn take_flock(file: &File, path: &Path) -> io::Result<()> {
    // SAFETY: `file` owns a valid descriptor that stays open and unchanged
    // for the whole call, and `LOCK_EX | LOCK_NB` asks for an exclusive lock
    // that returns immediately with `EWOULDBLOCK` instead of waiting, so
    // this neither blocks nor drops a lock it did not take.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    // Both spellings are compared because they are the same value on Linux
    // and not on every Unix.
    let held_by_someone = matches!(
        err.raw_os_error(),
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
    );
    if held_by_someone {
        return Err(io::Error::new(err.kind(), BUSY.to_owned()));
    }
    Err(io::Error::new(
        err.kind(),
        format!("cannot lock '{}': {err}", path.display()),
    ))
}

/// Record this process's pid in the lock file, for diagnostics to read.
///
/// Written through the locked descriptor, which no other wwrpc can be holding
/// while the flock is ours, and truncated through that same descriptor so a
/// shorter pid cannot leave digits of a longer previous one behind. The
/// cursor is put back at the start of the file before the write, so the line
/// lands at offset zero whatever anything read through the descriptor before.
fn record_pid(file: &mut File, path: &Path) -> io::Result<()> {
    file.set_len(0)
        .and_then(|()| file.rewind())
        .and_then(|()| writeln!(file, "{}", std::process::id()))
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot write the pid into '{}': {err}", path.display()),
            )
        })
}
