// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Integration tests for [`wwrpc::lock`]: who holds `wwrpc.lock`.
//!
//! Exclusion is never decided from the file's contents, and nothing here
//! pretends otherwise: the pid inside the lock file is diagnostics, and the
//! only assertions that read it pin that a holder records its own. What the
//! tests do pin is:
//!
//! - a live holder excludes every other holder, including one racing it;
//! - releasing the lock is the descriptor closing, so a leftover file is
//!   not a claim and a killed holder is not a claim;
//! - the lock file itself is private, refuses a planted symlink, and stays
//!   behind on drop.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::os::unix::fs::{MetadataExt, PermissionsExt};

use wwrpc::lock::SingleInstance;

/// Name of the lock file, as the holder and a stray reader both see it.
const LOCK_FILE: &str = "wwrpc.lock";

/// Mode of the scratch directories below: owner only, like every directory
/// this crate creates.
const SCRATCH_MODE: u32 = 0o700;

/// How many starters race for the same lock at once.
const RACERS: usize = 8;

/// Environment variable that turns a re-executed copy of this test binary
/// into the child process that holds the lock until it is killed.
const HOLDER_DIR: &str = "WWRPC_TEST_LOCK_HOLDER_DIR";

/// File the child touches once it owns the lock, so the parent knows the
/// lock is held without racing it.
const HOLDER_READY: &str = "holder-ready";

/// Exact test name the parent asks the child binary to run, and nothing else.
const HOLDER_TEST: &str = "test_lock_is_released_when_the_holder_process_dies";

/// How long a test waits for the child process to report that it holds the
/// lock before failing instead of hanging.
const CHILD_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs this file's tests one at a time.
///
/// They all take and release locks from one process, and one of them forks a
/// child that has to be excluded by a lock the parent already holds. That
/// fork is precisely what makes a `flock` answer `EWOULDBLOCK` for a file
/// whose holder closed microseconds earlier: a lock belongs to the open file
/// description, `fork` duplicates the descriptor, and `O_CLOEXEC` only closes
/// that copy at the child's `exec` — so the child keeps the lock alive until
/// it execs, and a fresh descriptor in that window loses to it. Measured on
/// this `$TMPDIR` (ext2 on zram) with no fork beside the locking: thousands of
/// lock cycles, never a refusal, so the filesystem is not the variable.
/// Serialising keeps the fork from running beside the other tests; nothing
/// here depends on the order.
static SERIAL: Mutex<()> = Mutex::new(());

/// Take [`SERIAL`], ignoring the poison a panicking test leaves behind: one
/// failed run must not wedge every later one.
fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A private (`0700`) directory that removes itself when dropped.
///
/// It sits under the system temp dir rather than under the runtime directory
/// the daemon really uses: a test must never create, repair or audit that
/// one, only the directories it is handed.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Create a fresh scratch directory for one test. The name carries the
    /// pid and a nanosecond stamp, so parallel test runs never share one.
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        let path =
            std::env::temp_dir().join(format!("wwrpc-lock-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).expect("the scratch dir is created");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(SCRATCH_MODE))
            .expect("the scratch dir is owner-only");
        Self { path }
    }

    /// Path handed to the lock, valid until the guard is dropped.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Path of the lock file inside `dir`.
fn lock_path(dir: &Path) -> PathBuf {
    dir.join(LOCK_FILE)
}

/// Permission bits (`mode & 0o777`) of `path`, following symlinks as the
/// implementation does.
fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|err| panic!("stat '{}': {err}", path.display()))
        .mode()
        & 0o777
}

/// Wait until `path` shows up. A child that never arrives fails the test
/// through this, instead of hanging it; it is taken down first so a failed
/// run leaves no orphan behind.
fn wait_for(path: &Path, child: &mut Child) -> bool {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Pins that a live holder excludes a second one, that the pid in the lock
/// file is our own, and that dropping the holder releases the lock while
/// leaving the file behind.
#[test]
fn test_flock_excludes_second_holder() {
    let _serial = serial();
    let dir = Scratch::new("held");
    let path = lock_path(dir.path());
    let first = SingleInstance::acquire(dir.path()).expect("the first holder wins");

    let refused =
        SingleInstance::acquire(dir.path()).expect_err("a live holder excludes a second one");
    assert_eq!(
        refused.to_string(),
        "another wwrpc instance is running",
        "a refused start must say why, in the words the daemon prints"
    );
    assert_eq!(
        first.pid(),
        Some(std::process::id()),
        "the holder records its own pid for diagnostics"
    );
    assert_eq!(mode_of(&path), 0o600, "the lock file must be owner-only");

    drop(first);
    assert!(
        path.exists(),
        "dropping a holder must leave the lock file in place: unlinking it would hand a \
         fresh, unlocked file to the next starter while this one still holds the old inode"
    );
    let second = SingleInstance::acquire(dir.path())
        .expect("a leftover lock file must not block a new holder");
    assert_eq!(
        second.pid(),
        Some(std::process::id()),
        "the new holder rewrites the pid, replacing the previous one"
    );
}

/// Pins that the contents of the lock file are diagnostics, never a claim:
/// unparsable text, a dead pid and even our own live pid all leave the lock
/// free, because nothing holds the descriptor.
#[test]
fn test_stale_lock_file_does_not_block() {
    let _serial = serial();
    let dir = Scratch::new("stale");
    let path = lock_path(dir.path());

    std::fs::write(&path, "not-a-pid").expect("the unparsable leftover is written");
    let holder =
        SingleInstance::acquire(dir.path()).expect("unparsable leftovers must not block a start");
    assert_eq!(
        holder.pid(),
        Some(std::process::id()),
        "the new holder overwrites the leftovers"
    );
    drop(holder);

    std::fs::write(&path, "2147483647").expect("the dead-pid leftover is written");
    let holder = SingleInstance::acquire(dir.path()).expect("a dead pid must not block a start");
    // Longer than the line this process writes: a holder that overwrote only
    // the leading bytes would leave the tail behind and read back as no pid.
    assert_eq!(
        holder.pid(),
        Some(std::process::id()),
        "the new holder replaces the leftover line"
    );
    drop(holder);

    std::fs::write(&path, std::process::id().to_string())
        .expect("the live-pid leftover is written");
    let holder =
        SingleInstance::acquire(dir.path()).expect("a live pid in the file is not a claim");
    assert_eq!(
        holder.pid(),
        Some(std::process::id()),
        "the pid we just wrote is the pid we read"
    );
}

/// Pins that a symlink planted where the lock file belongs is refused rather
/// than followed, and that whatever it pointed at is left exactly as it was:
/// the lock file is `0600` and rewritten in place, so following a link would
/// put both on a file the lock never asked for.
#[test]
fn test_symlinked_lock_file_is_refused() {
    let _serial = serial();
    let dir = Scratch::new("symlink");
    let decoy = dir.path().join("decoy");
    std::fs::write(&decoy, "not a lock\n").expect("the decoy is written");
    let decoy_mode = mode_of(&decoy);
    std::os::unix::fs::symlink(&decoy, lock_path(dir.path())).expect("the link is planted");

    let err = SingleInstance::acquire(dir.path())
        .expect_err("a symlinked lock file must be refused instead of followed");

    assert!(
        err.to_string().contains("cannot open lock file"),
        "the refusal must name the lock file, in the words the daemon prints: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(&decoy).expect("the decoy is readable"),
        "not a lock\n",
        "the lock must not write through the planted link"
    );
    assert_eq!(
        mode_of(&decoy),
        decoy_mode,
        "the lock must not re-mode the planted link's target"
    );
}

/// Pins the guarantee itself: starters that begin at the same instant end
/// with exactly one holder. Every winner keeps its guard until every racer
/// has finished, so a lone result cannot come from a lock released early.
#[test]
fn test_concurrent_acquires_leave_exactly_one_holder() {
    let _serial = serial();
    let dir = Scratch::new("race");
    let path = dir.path();
    let gate = Arc::new(Barrier::new(RACERS));

    let outcomes: Vec<Result<SingleInstance, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..RACERS)
            .map(|_| {
                let gate = Arc::clone(&gate);
                scope.spawn(move || {
                    // Released together, so the opens and the lock attempts
                    // overlap instead of running one after the other.
                    gate.wait();
                    SingleInstance::acquire(path).map_err(|err| err.to_string())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("no racer panicked"))
            .collect()
    });

    let winners = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    assert_eq!(
        winners, 1,
        "a check-then-write guard lets concurrent starters through together; the lock must \
         be granted to exactly one of them"
    );
}

/// Pins that the lock dies with the process that took it: the child holds the
/// lock until it is killed, and afterwards the file is still there while the
/// lock is gone. That is why this guard needs no stale-file recovery at all.
#[test]
fn test_lock_is_released_when_the_holder_process_dies() {
    // Re-executed as the child: hold the lock and wait to be killed.
    if let Some(dir) = std::env::var_os(HOLDER_DIR) {
        hold_lock_until_killed(Path::new(&dir));
    }

    let _serial = serial();
    let dir = Scratch::new("killed");
    let ready = dir.path().join(HOLDER_READY);
    let exe = std::env::current_exe().expect("this test binary has a path");
    let mut child = Command::new(exe)
        .args(["--exact", HOLDER_TEST, "--nocapture"])
        .env(HOLDER_DIR, dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the holder child starts");

    assert!(
        wait_for(&ready, &mut child),
        "the child never reported holding the lock on '{}'",
        dir.path().display()
    );
    let refused = SingleInstance::acquire(dir.path()).is_err();
    // Killed before the verdict is checked, so a failed assertion cannot
    // leave the child running.
    child.kill().expect("the holder child is killed");
    child.wait().expect("the holder child is reaped");

    assert!(refused, "a holder in another process must exclude us");
    assert!(
        lock_path(dir.path()).exists(),
        "a killed holder leaves the lock file behind, and that is harmless"
    );
    let _holder = SingleInstance::acquire(dir.path())
        .expect("a killed holder releases the lock: the kernel closes its descriptor");
}

/// Child half: take the lock, say so, and stay alive until the parent kills
/// this process. Only then does the kernel release the lock.
fn hold_lock_until_killed(dir: &Path) -> ! {
    let _holder = SingleInstance::acquire(dir).expect("the child takes the lock");
    std::fs::write(dir.join(HOLDER_READY), b"ready")
        .expect("the child announces that it holds the lock");
    // Bounded so an orphaned child cannot outlive the run it belongs to.
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::process::exit(0);
}
