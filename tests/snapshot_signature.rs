// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Tests for the snapshot change signature.
//!
//! A tick decides whether to copy the account database at all by asking
//! whether anything a snapshot is built from was written since the last
//! one it accepted. That is a question about the database AND the SQLite
//! sidecars beside it: a game running in WAL mode commits its rows into
//! `-wal` and leaves the main file's timestamp standing still, so a
//! signature built from the main file alone would call that unchanged and
//! go on serving an older snapshot.
//!
//! What the signature cannot do is pinned next to what it can, because
//! both are properties of the fold: an absent sidecar must never look like
//! a present one (a `-shm` of no bytes has to be one version of the set,
//! not two), and a file that is not there must never look like a failure,
//! since a game in rollback-journal mode keeps no sidecars at all.

use std::fs::{self, File, FileTimes};
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use wwrpc::db::{DbReader, Refresh, snapshot_signature};
use wwrpc::telemetry_cache::file_signature;

/// Prefix of every fixture directory this file creates.
const FIXTURE_PREFIX: &str = "wwrpc-signature";

/// Uid the fixture account is stored under. Never a real account.
const LIVE_UID: &str = "111111111";

/// Bytes of a rollback journal, so a planted one is a file with contents
/// rather than an empty fixture.
const JOURNAL_BYTES: &[u8] = b"a rollback journal no reader will replay here";

/// Empty fixture directory for one test.
fn fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{FIXTURE_PREFIX}-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the fixture dir is created");
    dir
}

/// The account database directory inside a fixture tree.
fn storage_of(root: &Path) -> PathBuf {
    root.join("Client/Saved/LocalStorage")
}

/// The level row for `level`, for the fixture account.
fn level_row(level: u64) -> String {
    format!(r#"{{"Content": [["{LIVE_UID}", [{{"Region": "Europe", "Level": {level}}}]]]}}"#)
}

/// Write a readable `LocalStorage.db` holding one account at a known
/// level. Nothing else lives beside it: a rollback-journal database with
/// no transaction in flight keeps no sidecar, which is the state the
/// signature has to treat as ordinary.
fn write_account_db(path: &Path, level: u64) {
    let _ = fs::remove_file(path);
    let conn = Connection::open(path).expect("the fixture database is created");
    conn.execute_batch(
        "CREATE TABLE LocalStorage(key text primary key not null, value text not null)",
    )
    .expect("the fixture schema is written");
    conn.execute(
        "INSERT INTO LocalStorage(key, value) VALUES('RecentlyLoginUID', ?1)",
        [LIVE_UID],
    )
    .expect("the login row is written");
    conn.execute(
        "INSERT INTO LocalStorage(key, value) VALUES('SdkLevelData', ?1)",
        [level_row(level)],
    )
    .expect("the level row is written");
}

/// The moment a fixture file was last written.
fn written_at(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .expect("the fixture file has a modification time")
}

/// Move a fixture file's modification time forward by `seconds`, well clear
/// of where it was, and return the time it was given.
///
/// The fixture filesystem keeps timestamps no finer than a millisecond or
/// two, so two writes a few nanoseconds apart can land on the same recorded
/// time. A test about *when* a file changed cannot leave that to chance: it
/// sets the time itself, which is also the only way to tell "the reader
/// looked at this file again" from "this file changed".
fn stamp_later(path: &Path, seconds: u64) -> SystemTime {
    let later = written_at(path) + Duration::from_secs(seconds);
    File::options()
        .write(true)
        .open(path)
        .expect("the fixture file is writable")
        .set_times(FileTimes::new().set_modified(later))
        .expect("the fixture file is stamped");
    later
}

/// Give a fixture file the modification time `when`, exactly.
///
/// The counterpart of [`stamp_later`] for a time the clock would never
/// reach on its own. A Linux filesystem records timestamps before the
/// epoch, and a `stat` on one reads back a negative count of seconds, so
/// the case is reachable on any Linux box and needs no mocking to reach.
fn stamp_at(path: &Path, when: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .expect("the fixture file is writable")
        .set_times(FileTimes::new().set_modified(when))
        .expect("the fixture file is stamped");
    assert_eq!(
        written_at(path),
        when,
        "the filesystem must have kept the time it was given, or the test \
         below is not exercising what it claims to"
    );
}

/// Fixture tree holding one readable account database and no sidecar at
/// all: its root, and the directory the database is in.
fn bare_storage(tag: &str, level: u64) -> (PathBuf, PathBuf) {
    let root = fixture(tag);
    let storage = storage_of(&root);
    fs::create_dir_all(&storage).expect("the storage dir is created");
    write_account_db(&storage.join("LocalStorage.db"), level);
    (root, storage)
}

// ---------------------------------------------------------------------------
// The signature on its own.
// ---------------------------------------------------------------------------

/// Pins the point of the whole fold: a sidecar written on its own is a new
/// version of the snapshot, even when the main database has not been
/// touched. This is the case that matters in WAL mode, where a commit lands
/// in `-wal` and the main file keeps its timestamp until a checkpoint.
///
/// The fixture asserts that the main file really did stand still — a
/// reader that saw it move would be entitled to copy it again anyway, and
/// the test would be pinning nothing.
#[test]
fn test_signature_changes_when_sidecar_mtime_changes() {
    let (root, storage) = bare_storage("sidecar-mtime", 60);
    let journal = storage.join("LocalStorage.db-journal");
    fs::write(&journal, JOURNAL_BYTES).expect("the journal fixture is written");

    let before = snapshot_signature(&storage).expect("the storage dir is measurable");
    let database_before =
        file_signature(&storage.join("LocalStorage.db")).expect("the main database is measurable");

    let stamped = stamp_later(&journal, 60);
    assert_eq!(
        written_at(&journal),
        stamped,
        "the fixture must really move the journal's recorded time, or the \
         signatures could match for a reason that says nothing"
    );

    let after = snapshot_signature(&storage).expect("the storage dir is measurable");
    assert_eq!(
        file_signature(&storage.join("LocalStorage.db")).expect("the main database is measurable"),
        database_before,
        "the fixture must leave the main database alone, or a reader could \
         be re-copying it for that reason alone"
    );
    assert_ne!(
        after, before,
        "a journal written at a new time is a new version of the snapshot, \
         and the signature is what the tick's skip decision is made from"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins the other half, which is what makes the skip worth having: nothing
/// written means nothing copied, so two calls over the same untouched
/// directory are one version.
///
/// A sidecar is planted here on purpose. A database with no sidecars beside
/// it is the easy case, and it would stay the easy case if the fold ever
/// started inventing a version for a file that is not there.
#[test]
fn test_signature_stable_when_nothing_changes() {
    let (root, storage) = bare_storage("stable", 60);
    fs::write(
        storage.join("LocalStorage.db-wal"),
        b"a write-ahead log nobody is writing to",
    )
    .expect("the wal fixture is written");

    let first = snapshot_signature(&storage).expect("the storage dir is measurable");
    assert_eq!(
        snapshot_signature(&storage).expect("the storage dir is measurable"),
        first,
        "measuring an untouched directory twice describes one version, which \
         is what lets a tick skip the copy entirely"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins that sidecars are optional. A game in rollback-journal mode keeps
/// no `-wal` and no `-shm` at all, and that is the ordinary state of a
/// database nobody has run a transaction in, not a fault: the signature is
/// still produced, and it is still one version measured twice.
///
/// The second half is the one that keeps the sentinel honest. Three absent
/// sidecars and three present ones fold the same way only if presence is
/// worth nothing at all, which would make a journal appearing invisible —
/// the very thing this signature exists to catch.
#[test]
fn test_signature_tolerates_missing_sidecars() {
    let (root, storage) = bare_storage("no-sidecars", 60);

    let bare = snapshot_signature(&storage).expect("a database with no sidecars is measurable");
    assert_eq!(
        snapshot_signature(&storage).expect("a database with no sidecars is measurable"),
        bare,
        "nothing was written between the two measurements"
    );

    fs::write(storage.join("LocalStorage.db-journal"), JOURNAL_BYTES)
        .expect("the journal fixture is written");
    assert_ne!(
        snapshot_signature(&storage).expect("the storage dir is measurable"),
        bare,
        "a sidecar that appears is a new version of the set; a fold that \
         treated it as absent would skip the copy that recovers it"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins that appearing and vanishing are two changes, not one. A `-wal`
/// that comes and goes is what SQLite does around a checkpoint, and a
/// reader that folded both into the same value would either copy on every
/// checkpoint or miss the commit that came with it.
#[test]
fn test_a_sidecar_that_appears_or_vanishes_moves_the_signature() {
    let (root, storage) = bare_storage("appear-vanish", 60);
    let wal = storage.join("LocalStorage.db-wal");

    let without = snapshot_signature(&storage).expect("the storage dir is measurable");
    fs::write(&wal, b"a write-ahead log").expect("the wal fixture is written");
    let with = snapshot_signature(&storage).expect("the storage dir is measurable");
    assert_ne!(with, without, "a sidecar that appears is a new version");

    fs::remove_file(&wal).expect("the wal fixture is removed");
    assert_eq!(
        snapshot_signature(&storage).expect("the storage dir is measurable"),
        without,
        "and the same sidecar vanishing is a return to the version before \
         it, not a third one"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins the one collision that is worth spending a test on: an empty sidecar
/// against no sidecar at all.
///
/// Every present file contributes one more than its length, so a file of no
/// bytes can never contribute the same as a file that is not there. SQLite
/// never leaves an empty `-wal` or `-journal` lying around — it deletes the
/// journal instead — but the filesystem is not SQLite's to guarantee, and a
/// zero-length file is exactly the value a naive sum would lose.
#[test]
fn test_an_empty_sidecar_is_not_the_same_as_no_sidecar() {
    let (root, storage) = bare_storage("empty-sidecar", 60);
    let shm = storage.join("LocalStorage.db-shm");

    let without = snapshot_signature(&storage).expect("the storage dir is measurable");
    fs::write(&shm, b"").expect("the empty sidecar is written");
    assert_ne!(
        snapshot_signature(&storage).expect("the storage dir is measurable"),
        without,
        "a sidecar of no bytes is still a file that is there, and the tick \
         must not read that as the file being absent"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins the other half of the fold on its own. The time total catches a
/// rewrite at a new moment; the length total catches one that lands on the
/// moment the file already had, which is what a filesystem with coarse
/// timestamps does without anyone noticing.
///
/// The timestamp is put back on purpose: without that, the growing file
/// would also have moved its recorded time and the test would pass for a
/// reason that has nothing to do with what it claims to pin.
#[test]
fn test_a_sidecar_that_grows_at_the_same_time_is_a_new_version() {
    let (root, storage) = bare_storage("grow", 60);
    let journal = storage.join("LocalStorage.db-journal");
    fs::write(&journal, JOURNAL_BYTES).expect("the journal fixture is written");
    let journal_time = written_at(&journal);

    let before = snapshot_signature(&storage).expect("the storage dir is measurable");
    fs::write(&journal, vec![b'x'; 4096]).expect("the journal fixture grows");
    File::options()
        .write(true)
        .open(&journal)
        .expect("the fixture file is writable")
        .set_times(FileTimes::new().set_modified(journal_time))
        .expect("the fixture file is stamped back");
    assert_eq!(
        written_at(&journal),
        journal_time,
        "the fixture must hand the journal back the time it already had"
    );

    assert_ne!(
        snapshot_signature(&storage).expect("the storage dir is measurable"),
        before,
        "a sidecar that grows is a new version even with its recorded time \
         unchanged; the length total is what sees this one"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins that a timestamp from before 1970 is an ordinary version.
///
/// A Linux filesystem records modification times before the epoch — a file
/// stamped there reads back as a negative count of seconds — so the fold's
/// subtraction of the epoch runs backwards on it, and three things have to
/// hold for such a file to be usable: measuring it must not fail, two
/// different pre-epoch instants must not collapse into one version, and a
/// pre-epoch instant must not collide with the epoch itself.
///
/// What is deliberately not asserted is any particular value inside the
/// signature, which is opaque on purpose. A re-encoding that stays
/// injective passes this file even though it is not the signed
/// arithmetic the implementation uses, because a caller cannot tell the
/// two apart and should not have to. What must fail is the collapse: the
/// mutations checked here were a pre-epoch time clamped to zero (caught)
/// and a pre-epoch time that panicked instead of returning (caught by the
/// test not completing).
#[test]
fn test_a_timestamp_before_the_epoch_is_an_ordinary_version() {
    let (root, storage) = bare_storage("pre-1970", 60);
    let database = storage.join("LocalStorage.db");

    // Two seconds before the epoch, and the epoch itself: the two states a
    // signed fold must keep apart, and the pair a fold that dropped the
    // sign would collapse into one value.
    stamp_at(&database, UNIX_EPOCH - Duration::from_secs(2));
    let before = snapshot_signature(&storage).expect("a pre-epoch database is measurable");
    assert_eq!(
        snapshot_signature(&storage).expect("a pre-epoch database is measurable"),
        before,
        "measuring an untouched pre-epoch database twice describes one version"
    );

    stamp_at(&database, UNIX_EPOCH);
    let at_epoch = snapshot_signature(&storage).expect("an epoch database is measurable");
    assert_ne!(
        at_epoch, before,
        "a database stamped two seconds earlier is a different version, and \
         a fold that clamped every pre-epoch time to zero would call these \
         two the same"
    );

    // A decade further back, which is the case a clamp to zero cannot pass:
    // it is a different pre-epoch instant and must still read as one.
    stamp_at(
        &database,
        UNIX_EPOCH - Duration::from_secs(10 * 365 * 24 * 3600),
    );
    assert_ne!(
        snapshot_signature(&storage).expect("a pre-epoch database is measurable"),
        before,
        "a decade before the epoch is a different version from two seconds \
         before it, and an implementation that dropped the sign or panicked \
         cannot pass all three of these assertions at once"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins the error that keeps the sentinel from becoming a blind spot. A
/// directory with no account database has no version to describe — the
/// signature is asked about the set a snapshot is built from, and the main
/// file is in every one of them — while the sidecars stay optional. A caller
/// that cannot measure the main file has to hear about it.
///
/// The path is one this test creates and knows to be empty, rather than a
/// guess that some install is missing: this machine has one.
#[test]
fn test_the_signature_still_needs_the_main_database() {
    let root = fixture("no-database");
    assert!(
        snapshot_signature(&root).is_err(),
        "a storage dir with no account database has no version to fold"
    );

    write_account_db(&root.join("LocalStorage.db"), 60);
    assert!(
        snapshot_signature(&root).is_ok(),
        "and the same dir with the database in it is measurable again"
    );

    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// The signature inside the reader.
// ---------------------------------------------------------------------------

/// Pins the whole reason this signature exists, end to end: a commit that
/// only reaches a sidecar is a new version of the snapshot, and the tick
/// publishes it.
///
/// The fixture is a game holding its database open in WAL mode, which is
/// what a running client does: rows go into `-wal`, and the main database's
/// length and timestamp stay exactly where they were until a checkpoint.
/// The test asserts that of the main file before it asserts anything of
/// the reader — without it the tick would be free to re-copy for the plain
/// reason that the database changed, and this would pin nothing.
#[test]
fn test_a_write_that_only_reaches_the_sidecar_is_still_read() {
    let root = fixture("wal");
    let storage = storage_of(&root);
    fs::create_dir_all(&storage).expect("the storage dir is created");
    let db = storage.join("LocalStorage.db");

    // The game keeps this handle for the whole test: dropping it would
    // checkpoint the sidecar away and leave the fixture proving nothing.
    let game = Connection::open(&db).expect("the fixture database is created");
    game.pragma_update(None, "journal_mode", "wal")
        .expect("the fixture database runs in WAL mode");
    game.execute_batch(
        "CREATE TABLE LocalStorage(key text primary key not null, value text not null)",
    )
    .expect("the fixture schema is written");
    game.execute(
        "INSERT INTO LocalStorage(key, value) VALUES('RecentlyLoginUID', ?1)",
        [LIVE_UID],
    )
    .expect("the login row is written");
    game.execute(
        "INSERT INTO LocalStorage(key, value) VALUES('SdkLevelData', ?1)",
        [level_row(60)],
    )
    .expect("the level row is written");
    assert!(
        storage.join("LocalStorage.db-wal").exists(),
        "a WAL database must keep a sidecar, or this fixture pins nothing"
    );

    let mut reader = DbReader::new(storage.clone(), root.join("snapshots"), None)
        .expect("the reader opens its snapshot dir");
    assert_eq!(reader.refresh(), Refresh::Updated, "the first tick reads");
    assert_eq!(
        reader
            .last_info()
            .expect("the first tick triages the account")
            .union_level,
        "60"
    );
    let before = file_signature(&db).expect("the main database is measurable");

    // The game levels up: the same key, one row different, committed into
    // the sidecar.
    game.execute(
        "UPDATE LocalStorage SET value = ?1 WHERE key = 'SdkLevelData'",
        [level_row(61)],
    )
    .expect("the level row is raised");
    assert_eq!(
        file_signature(&db).expect("the main database is measurable"),
        before,
        "a WAL commit must leave the main database untouched, or this \
         fixture pins nothing: the reader would be entitled to copy it again"
    );

    assert_eq!(
        reader.refresh(),
        Refresh::Updated,
        "a commit that only reached the sidecar is a new version of the snapshot"
    );
    assert_eq!(
        reader
            .last_info()
            .expect("the second tick triages the account")
            .union_level,
        "61",
        "the tick must publish the level the sidecar is carrying"
    );

    drop(game);
    let _ = fs::remove_dir_all(&root);
}

/// Pins that folding four files into one pair did not cost the skip. A
/// database with a sidecar beside it and nothing written to either must
/// still be one version, or every poll of a session would re-copy a
/// snapshot that had not moved.
#[test]
fn test_an_untouched_database_is_not_read_again() {
    let (root, storage) = bare_storage("untouched", 60);
    fs::write(storage.join("LocalStorage.db-journal"), JOURNAL_BYTES)
        .expect("the journal fixture is written");

    let mut reader = DbReader::new(storage.clone(), root.join("snapshots"), None)
        .expect("the reader opens its snapshot dir");

    assert_eq!(
        reader.refresh(),
        Refresh::Updated,
        "the first tick copies and triages"
    );
    assert_eq!(
        reader.refresh(),
        Refresh::Unchanged,
        "nothing was written to the database or to its sidecar, so the tick \
         has nothing new to copy and the published data still stands"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins the strict side of the fold and what it costs a tick.
///
/// A sidecar that exists and cannot be measured is not the same observation
/// as one that is not there, so the fold refuses rather than folds it into
/// the sentinel — and refusing reaches `refresh` as an error, which holds
/// the tick at its last accepted data exactly as an unreadable main
/// database would. The reader's sidecar copy loop is lenient with the same
/// file, and the difference is deliberate, so this is worth pinning on both
/// sides of it.
///
/// The fixture is a symlink that points at itself, which the kernel refuses
/// to resolve: `ELOOP`, every time, with no permission fiddling and no
/// timing. `-wal` is the file planted, because a `-shm` beside a
/// rollback-journal snapshot is ignored by SQLite and would make the
/// verdict ambiguous.
///
/// The signature assertion is the one that pins the branch: relax the
/// strict rule and a sentinel for an unmeasurable file returns `Ok`
/// instead, and the tick below goes on as if nothing were wrong. The
/// refreshes are what pin the consequence, and they cannot tell the two
/// apart on their own — a reader that folded the bad file in would also
/// answer `Unchanged` on the second tick, for the wrong reason. They are
/// here because that consequence is a contract a reader of the source is
/// owed: the tick is held, and it comes back on its own.
#[test]
fn test_a_sidecar_that_cannot_be_measured_holds_the_tick() {
    let (root, storage) = bare_storage("unmeasurable", 60);
    let mut reader = DbReader::new(storage.clone(), root.join("snapshots"), None)
        .expect("the reader opens its snapshot dir");
    assert_eq!(
        reader.refresh(),
        Refresh::Updated,
        "the first tick copies and triages an ordinary database"
    );

    // What the accepted snapshot was read at, for the fixture control below.
    let signature_at_tick_one =
        snapshot_signature(&storage).expect("the storage dir is measurable");

    // A file that is there and cannot be read through.
    let wal = storage.join("LocalStorage.db-wal");
    symlink("LocalStorage.db-wal", &wal).expect("the unresolvable sidecar is planted");
    let err = snapshot_signature(&storage)
        .expect_err("a sidecar that cannot be measured is not an absent one");
    // The error must NOT read as absence — that is the branch the sentinel
    // serves, and mistaking one for the other is the whole hazard. The
    // dedicated `FilesystemLoop` kind is unstable on this toolchain
    // (`io_error_more`), so the message is what the assertion reads, and
    // the errno is checked to still be inside the text the wrap preserved.
    assert_ne!(
        err.kind(),
        io::ErrorKind::NotFound,
        "a file that cannot be resolved is not a file that is not there: {err}"
    );
    let message = err.to_string();
    assert!(
        message.contains("LocalStorage.db-wal"),
        "the error must name the file it is about, or an operator reading the \
         warning has three sidecars and no way to tell which one: {message}"
    );
    // The errno is asserted against the tail of the message, past the path.
    // Searching the whole message would not discriminate: the fixture's
    // temporary directory carries this process's id, so the digits of a pid
    // ending "40" would satisfy the check on their own and the assertion
    // would pass on a wrap that had thrown the errno away. Stripping what
    // the wrap added is what makes this about the errno.
    let diagnosis = message
        .rsplit_once(": ")
        .map_or(message.as_str(), |(_, rest)| rest);
    assert!(
        diagnosis.contains(&format!("(os error {})", libc::ELOOP)),
        "and it must still carry the OS error underneath the path, or the \
         wrap has swallowed the diagnosis: {message}"
    );

    assert_eq!(
        reader.refresh(),
        Refresh::Unchanged,
        "a signature that cannot be taken holds the tick at its last data: \
         neither a copy on a guess nor a claim that nothing changed"
    );
    assert_eq!(
        reader
            .last_info()
            .expect("the first tick's data is still standing")
            .union_level,
        "60",
        "and the published data is the one the last accepted snapshot read"
    );

    fs::remove_file(&wal).expect("the unresolvable sidecar is gone");
    let database = storage.join("LocalStorage.db");
    write_account_db(&database, 61);
    // Level 60 and level 61 are the same number of bytes, so the timestamp
    // is the only thing that can tell the two databases apart — and this
    // filesystem keeps it no finer than a millisecond or two. The test
    // stamps the time itself rather than leaving the reader to notice a
    // rewrite that may have landed on the instant it already had.
    stamp_later(&database, 60);
    assert_ne!(
        snapshot_signature(&storage).expect("the storage dir is measurable"),
        signature_at_tick_one,
        "the fixture must actually change the set, or the recovery below \
         proves nothing"
    );

    assert_eq!(
        reader.refresh(),
        Refresh::Updated,
        "with the obstruction gone the next tick copies and triages again: a \
         reader that had folded the bad sidecar into the sentinel would see \
         the same signature as before and sit on the old data forever"
    );
    assert_eq!(
        reader
            .last_info()
            .expect("the recovered tick triages the account")
            .union_level,
        "61"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins that a sidecar symlink with no target is a file that is there, not
/// a file that is absent: the fold reports it instead of folding it into
/// the sentinel.
///
/// `fs::metadata` follows the link and fails with `NotFound`, which is the
/// kind the fold reads as "the game keeps no such file". A dangling link
/// has an entry of its own — `symlink_metadata` reports it — so folding it
/// in would hide a file that exists behind the ordinary rollback-journal
/// case, which keeps no sidecar at all. Lifting the link returns the set to
/// that ordinary case, which must stay silent.
#[test]
fn test_a_dangling_sidecar_symlink_is_reported_not_folded() {
    let (root, storage) = bare_storage("dangling-symlink", 60);
    let bare = snapshot_signature(&storage).expect("a database with no sidecars is measurable");

    let wal = storage.join("LocalStorage.db-wal");
    symlink("LocalStorage.db-missing-target", &wal).expect("the dangling sidecar is planted");
    assert!(
        wal.symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink()),
        "the fixture must really plant a link, or the error below proves nothing"
    );
    let err = snapshot_signature(&storage)
        .expect_err("a sidecar entry that cannot be measured is not an absent one");
    assert_ne!(
        err.kind(),
        io::ErrorKind::NotFound,
        "a link with no target is still an entry that is there: {err}"
    );
    assert_eq!(
        err.kind(),
        io::ErrorKind::Other,
        "a file that is there but cannot be resolved is neither absent nor \
         forbidden: a permission kind would misdirect the operator, and the \
         sentinel kind would hide the file: {err}"
    );
    let message = err.to_string();
    assert!(
        message.contains("LocalStorage.db-wal"),
        "the error must name the file it is about, or an operator reading \
         the warning has three sidecars and no way to tell which one: {message}"
    );
    assert!(
        message.contains("(os error 2)"),
        "and it must still carry the OS error underneath the path, or the \
         wrap has swallowed the diagnosis: {message}"
    );

    fs::remove_file(&wal).expect("the dangling sidecar is gone");
    assert_eq!(
        snapshot_signature(&storage).expect("a database with no sidecars is measurable"),
        bare,
        "lifting the link returns the set to its ordinary absent-sidecar \
         version, not to a third one"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins that a sidecar symlink resolving to a real file keeps working: it
/// is followed and folded in like the file itself, not reported.
///
/// The counterpart of the dangling case — the fix tells the two apart by
/// whether an entry is present, and a naive version refuses every link. A
/// signature taken through the link must describe the target's bytes, so it
/// differs from the version with no sidecar at all and stays put while
/// nothing is written.
#[test]
fn test_a_live_sidecar_symlink_is_followed_and_folded() {
    let (root, storage) = bare_storage("live-symlink", 60);
    let bare = snapshot_signature(&storage).expect("a database with no sidecars is measurable");

    fs::write(storage.join("wal-target"), b"a write-ahead log")
        .expect("the link target is written");
    let wal = storage.join("LocalStorage.db-wal");
    symlink("wal-target", &wal).expect("the live sidecar link is planted");
    assert!(
        wal.symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink()),
        "the fixture must really plant a link, or the measurement below \
         proves nothing"
    );

    let linked = snapshot_signature(&storage).expect("a sidecar link to a real file is measurable");
    assert_ne!(
        linked, bare,
        "a link resolving to a real file is folded in like the file \
         itself; a fold that read it as absent would return the bare version"
    );
    assert_eq!(
        snapshot_signature(&storage).expect("the storage dir is measurable"),
        linked,
        "measuring an untouched directory twice describes one version"
    );

    let _ = fs::remove_dir_all(&root);
}
