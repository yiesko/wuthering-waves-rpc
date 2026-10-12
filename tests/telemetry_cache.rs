// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Tests for the telemetry lookup cache.
//!
//! The launcher's telemetry database is where wwrpc looks for the player's
//! display name, in two places: the live rows it writes at login, and the
//! remnants of the rows it deleted after uploading them. Both are read from
//! a private copy of that file, so a lookup that finds nothing costs a copy
//! like any other — and a lookup that finds nothing is the ordinary case,
//! because those rows are gone within minutes of the login. This file pins
//! that a "no name here" answer is kept the way a name is: read once per
//! version of the file, not once per tick.
//!
//! A "copy did not happen" is not directly observable — every copy is
//! deleted right after it is read — so the reader-level tests below plant a
//! directory where the copy would land. No `remove_file` can unlink it and
//! no write can open it, so it refuses every copy for as long as it
//! stands: the first tick reads nothing, and a later tick that finds a name
//! anyway can only have read the file again.

use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use rusqlite::Connection;

use wwrpc::db::{DbReader, Refresh};
use wwrpc::telemetry_cache::{FileSignature, Lookup, TelemetryCache, file_signature};

/// Prefix of every fixture directory this file creates.
const FIXTURE_PREFIX: &str = "wwrpc-telemetry";

/// Uid the live fixture account logs in as.
const LIVE_UID: &str = "111111111";

/// The account a login switch moves to, and the one the level database
/// stores while the live one is missing.
const OTHER_UID: &str = "222222222";

/// Empty fixture directory for one test.
fn fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{FIXTURE_PREFIX}-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the fixture dir is created");
    dir
}

/// The launcher prefix inside a Steam tree: only a path beneath a
/// `steamapps` directory resolves to the telemetry database at all, so the
/// fixture has to be laid out like a Steam install for the lookup to reach
/// it.
fn telemetry_dir(root: &Path) -> PathBuf {
    root.join("steamapps/compatdata/3513350/pfx/drive_c/users/steamuser/AppData/Roaming/KR_G153")
}

/// Where the launcher keeps its telemetry database.
fn telemetry_of(root: &Path) -> PathBuf {
    telemetry_dir(root).join("KDData-data.db")
}

/// The game install inside the fixture tree.
fn game_of(root: &Path) -> PathBuf {
    root.join("steamapps/common/Wuthering Waves")
}

/// The account database inside the fixture tree.
fn storage_of(root: &Path) -> PathBuf {
    game_of(root).join("Client/Saved/LocalStorage")
}

/// A Steam-shaped fixture: an install with an account database under it and
/// a launcher prefix beside it. Returns the tree's root.
fn steam_tree(tag: &str) -> PathBuf {
    let root = fixture(tag);
    fs::create_dir_all(storage_of(&root)).expect("the storage dir is created");
    fs::create_dir_all(telemetry_dir(&root)).expect("the prefix dir is created");
    root
}

/// Write the account database: `live` is the logged-in account, and the
/// level rows hold exactly `stored`. Triage passes only when the login is
/// among them.
fn write_account_db(path: &Path, live: &str, stored: &[&str]) {
    let _ = fs::remove_file(path);
    let conn = Connection::open(path).expect("the fixture database is created");
    conn.execute_batch(
        "CREATE TABLE LocalStorage(key text primary key not null, value text not null)",
    )
    .expect("the fixture schema is written");
    conn.execute(
        "INSERT INTO LocalStorage(key, value) VALUES('RecentlyLoginUID', ?1)",
        [live],
    )
    .expect("the login row is written");
    let content: Vec<String> = stored
        .iter()
        .map(|uid| format!(r#"["{uid}", [{{"Region": "Europe", "Level": 60}}]]"#))
        .collect();
    conn.execute(
        "INSERT INTO LocalStorage(key, value) VALUES('SdkLevelData', ?1)",
        [format!(
            r#"{{"___MetaType___": "___Map___", "Content": [{}]}}"#,
            content.join(",")
        )],
    )
    .expect("the level row is written");
}

/// Write the launcher telemetry database with one live row per account,
/// exactly as the launcher stores them.
fn write_telemetry_db(path: &Path, rows: &[(&str, &str)]) {
    let _ = fs::remove_file(path);
    let conn = Connection::open(path).expect("the telemetry fixture is created");
    conn.execute_batch("CREATE TABLE KDData(id INTEGER PRIMARY KEY AUTOINCREMENT, content TEXT)")
        .expect("the telemetry schema is written");
    for (uid, name) in rows {
        conn.execute(
            "INSERT INTO KDData(content) VALUES(?1)",
            [format!(r#"{{"role_id":"{uid}","role_name":"{name}"}}"#)],
        )
        .expect("the telemetry row is written");
    }
}

/// Bytes of a telemetry database whose only trace of `uid` is a deleted
/// row: the event object the remnant scan dates and matches.
fn remnant_bytes(uid: &str, name: &str, event_ms: u64) -> Vec<u8> {
    let mut bytes = b"a page of freelist".to_vec();
    bytes.extend_from_slice(
        format!(
            r##"{{"#distinct_id":"test-device","#event_name":"login_role","properties":{{"role_id":"{uid}","role_name":"{name}","role_level":"60","server_name":"Europe","event_time_ms":"{event_ms}"}}}}"##
        )
        .as_bytes(),
    );
    bytes
}

/// Move a fixture's modification time forward: what a tick reads as "this
/// changed", so the next one copies and triages it again.
fn touch(path: &Path) {
    let meta = fs::metadata(path).expect("the fixture file is there");
    let later = meta
        .modified()
        .expect("the fixture has a modification time")
        + Duration::from_secs(2);
    File::options()
        .write(true)
        .open(path)
        .expect("the fixture file is writable")
        .set_times(FileTimes::new().set_modified(later))
        .expect("the fixture file is stamped");
}

/// The moment a fixture was last written, for [`restore_time`].
fn written_at(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .expect("the fixture has a modification time")
}

/// Put a fixture's modification time back to `when`, so a rewrite can be
/// made invisible to a reader that sees nothing but length and time. The
/// counterpart of [`touch`], and the only way to tell "did this reader read
/// the file again?" from "did the file change?".
fn restore_time(path: &Path, when: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .expect("the fixture file is writable")
        .set_times(FileTimes::new().set_modified(when))
        .expect("the fixture file is stamped");
}

/// Refuse every copy into `path` until [`allow_copies_into`] says otherwise.
fn refuse_copies_into(path: &Path) {
    let _ = fs::remove_dir_all(path);
    fs::create_dir_all(path).expect("the blocking entry is planted");
}

/// Let copies into `path` again.
fn allow_copies_into(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

/// A reader over a Steam fixture, with its snapshot directory beside it.
fn reader_over(root: &Path, tag: &str) -> DbReader {
    let snap = root.join(format!("snapshots-{tag}"));
    let mut reader =
        DbReader::new(storage_of(root), snap, None).expect("the reader opens its snapshot dir");
    reader.set_game_dir(game_of(root));
    reader
}

/// Milliseconds since the Unix epoch, for stamping a telemetry event.
fn epoch_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("the clock is past the epoch")
            .as_millis(),
    )
    .expect("the epoch offset fits in a u64")
}

// ---------------------------------------------------------------------------
// The cache on its own.
// ---------------------------------------------------------------------------

/// Signature of an observation: the same length and the same modification
/// time are the same version, whatever produced it.
fn signature_at(seconds: u64, len: u64) -> FileSignature {
    FileSignature::new(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds), len)
}

/// Pins the whole point of the cache: one read per version of the file. The
/// loader counts its own invocations, because "the second call did not read
/// anything" is the claim no return value could carry.
#[test]
fn test_cache_calls_loader_once_per_signature() {
    let calls = AtomicUsize::new(0);
    let mut cache: TelemetryCache<String> = TelemetryCache::new();
    let signature = signature_at(100, 4096);

    let loader = || {
        calls.fetch_add(1, Ordering::SeqCst);
        Lookup::Answered(Some("RoverOne".to_string()))
    };
    assert_eq!(
        cache.get_with(signature, loader),
        Some("RoverOne".to_string()),
        "the first lookup on an empty cache reads"
    );
    assert_eq!(
        cache.get_with(signature, loader),
        Some("RoverOne".to_string()),
        "the same signature returns the cached value"
    );

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "an unchanged file must not be read a second time"
    );
}

/// Pins that "no name in there" is a value, not an absence of one. A
/// lookup that found nothing has still read the file, and a file that has
/// not changed cannot answer differently — so the second lookup must not
/// read it again, and must still report nothing.
#[test]
fn test_cache_caches_misses() {
    let calls = AtomicUsize::new(0);
    let mut cache: TelemetryCache<String> = TelemetryCache::new();
    let signature = signature_at(100, 4096);

    let loader = || {
        calls.fetch_add(1, Ordering::SeqCst);
        Lookup::Answered(None)
    };
    assert_eq!(cache.get_with(signature, loader), None);
    assert_eq!(cache.get_with(signature, loader), None);

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a miss is cached like a hit: the file was read once, not twice"
    );
    assert_eq!(
        cache.get_with(signature, loader),
        None,
        "the cached miss is still a miss, not a forgotten answer"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Pins the other half: a file that did change must be read again, on
/// length alone as much as on time. Both are listed, so a table of
/// versions shows exactly which differences invalidate and which do not.
#[test]
fn test_cache_invalidates_on_signature_change() {
    let unchanged = signature_at(100, 4096);
    // (what the second signature differs in, that signature, and whether it
    // still describes the same version of the file)
    let versions = [
        ("same length, written later", signature_at(200, 4096), false),
        ("same second, longer file", signature_at(100, 8192), false),
        ("a different file entirely", signature_at(300, 2048), false),
        ("nothing at all", unchanged, true),
    ];

    for (label, second, same_version) in versions {
        let calls = AtomicUsize::new(0);
        let mut cache: TelemetryCache<String> = TelemetryCache::new();
        cache.get_with(unchanged, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Lookup::Answered(Some("first".to_string()))
        });
        cache.get_with(second, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Lookup::Answered(Some("second".to_string()))
        });

        assert_eq!(
            calls.load(Ordering::SeqCst),
            usize::from(!same_version) + 1,
            "{label}: a second read is due only when the file itself changed"
        );
    }

    let calls = AtomicUsize::new(0);
    let mut cache: TelemetryCache<String> = TelemetryCache::new();
    cache.get_with(unchanged, || {
        calls.fetch_add(1, Ordering::SeqCst);
        Lookup::Answered(None)
    });
    cache.get_with(signature_at(200, 4096), || {
        calls.fetch_add(1, Ordering::SeqCst);
        Lookup::Answered(Some("late arrival".to_string()))
    });
    assert_eq!(
        cache.get_with(signature_at(200, 4096), || {
            calls.fetch_add(1, Ordering::SeqCst);
            Lookup::Answered(Some("too late".to_string()))
        }),
        Some("late arrival".to_string()),
        "a name that appears after the file changed is kept"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// Pins the difference between a read that happened and one that did not,
/// which is the whole of what keeps a failure from being frozen.
///
/// A signature describes a file's bytes. It says nothing about a disk that
/// is full, a destination that is busy or permissions that changed — all of
/// which leave the file exactly as it was. So `Unread` is retried on the
/// next tick whatever the signature says, while `Answered(None)` — a file
/// that was read and held nothing — is an answer and is kept. Both return
/// `None` to the caller; only the cache can tell them apart, and it must.
#[test]
fn test_a_failed_read_is_retried_and_a_real_miss_is_not() {
    let calls = AtomicUsize::new(0);
    let mut cache: TelemetryCache<String> = TelemetryCache::new();
    let signature = signature_at(100, 4096);
    let unread = || {
        calls.fetch_add(1, Ordering::SeqCst);
        Lookup::<String>::Unread
    };

    assert_eq!(cache.get_with(signature, unread), None);
    assert_eq!(
        cache.get_with(signature, unread),
        None,
        "a failed read has no answer to hand back"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a read that never completed must be tried again on the next tick, \
         signature or no signature: a refused copy says nothing about the file"
    );

    // The cache is left empty, not poisoned: a real read that follows is kept.
    let answered = || {
        calls.fetch_add(1, Ordering::SeqCst);
        Lookup::Answered(None)
    };
    assert_eq!(cache.get_with(signature, answered), None);
    assert_eq!(cache.get_with(signature, answered), None);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "a genuine miss is an answer, and it is cached like a hit"
    );

    // And a failed read under a changed signature must not pin the cache to
    // the new one either.
    let moved = signature_at(200, 4096);
    cache.get_with(moved, unread);
    cache.get_with(moved, unread);
    assert_eq!(
        cache.get_with(moved, answered),
        None,
        "the read that followed was a miss, and it is kept"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        6,
        "both failed reads were retried under the changed signature, and only \
         the answered one was stored"
    );
}

/// Pins what a caller has to do when the question changes but the file does
/// not. The signature says nothing about who the answer was for, so a new
/// login has to drop the cache by hand — otherwise a telemetry file nobody
/// wrote since would keep answering with the previous role's name.
#[test]
fn test_clear_forces_the_next_lookup_to_run() {
    let calls = AtomicUsize::new(0);
    let mut cache: TelemetryCache<String> = TelemetryCache::new();
    let signature = signature_at(100, 4096);
    let loader = || {
        calls.fetch_add(1, Ordering::SeqCst);
        Lookup::Answered(Some("first".to_string()))
    };

    cache.get_with(signature, loader);
    cache.clear();

    assert_eq!(
        cache.get_with(signature, loader),
        Some("first".to_string()),
        "the same signature reads again after a reset"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// Pins that the signature of a real file follows the file: the same path
/// measured twice is one version, and a rewritten one is another.
#[test]
fn test_signature_follows_the_file_it_measures() {
    let root = fixture("signature");
    let path = root.join("telemetry.db");
    fs::write(&path, b"first version").expect("the fixture file is written");

    let before = file_signature(&path).expect("the fixture file is measurable");
    assert_eq!(
        file_signature(&path).expect("the fixture file is measurable"),
        before,
        "measuring an unchanged file twice describes one version"
    );

    fs::write(&path, b"second version, longer").expect("the fixture file is rewritten");
    assert_ne!(
        file_signature(&path).expect("the rewritten file is measurable"),
        before,
        "a longer rewrite is a new version"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins the one case where there is no version to speak of: a file that is
/// not there cannot be keyed, and says so instead of handing back a
/// signature that would match every other missing file forever.
#[test]
fn test_signature_of_a_missing_file_is_an_error() {
    let root = fixture("signature-missing");
    assert!(
        file_signature(&root.join("absent.db")).is_err(),
        "a file that is not there has no signature to cache against"
    );

    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// The cache inside the reader.
// ---------------------------------------------------------------------------

/// Pins that a tick which finds no display name does not read the telemetry
/// database again on a later tick.
///
/// The miss has to be a real one, not a read that failed: the first version
/// of the file holds a row for another account and none for the live login,
/// so the name lookup ran and came back empty-handed — which is the answer
/// worth keeping. The second version holds the live login's name instead,
/// same bytes in the same file with the timestamp put back, so a reader
/// that looks again would find it. The signature cannot tell the two apart,
/// and the name must stay missing.
///
/// The second tick is one that re-reads the account database, which is what
/// makes the tick look twice: once on the way in and once after triage,
/// against the login that triage confirmed. Neither may reach the telemetry
/// file, and a name appearing here would catch either one of them.
#[test]
fn test_a_telemetry_miss_survives_an_unchanged_file() {
    let root = steam_tree("miss");
    let account = storage_of(&root).join("LocalStorage.db");
    write_account_db(&account, LIVE_UID, &[LIVE_UID]);
    let telemetry = telemetry_of(&root);
    // Another account's row: the live login has no name in here.
    write_telemetry_db(&telemetry, &[(OTHER_UID, "RoverOne")]);
    let before = file_signature(&telemetry).expect("the telemetry file is measurable");
    let before_time = written_at(&telemetry);

    let mut reader = reader_over(&root, "miss");
    assert_eq!(reader.refresh(), Refresh::Updated);
    assert_eq!(
        reader.display_name(),
        None,
        "the file holds no row for the live login, so there is no name"
    );

    // The same file, with the row now naming the live login: same length,
    // same timestamp, a different answer if anyone bothers to look.
    write_telemetry_db(&telemetry, &[(LIVE_UID, "RoverOne")]);
    restore_time(&telemetry, before_time);
    assert_eq!(
        file_signature(&telemetry).expect("the telemetry file is measurable"),
        before,
        "the two versions must be the same length and time, or this fixture \
         says nothing: the reader is allowed to re-read a file that changed"
    );
    touch(&account);

    assert_eq!(
        reader.refresh(),
        Refresh::Updated,
        "the account database moved on, so the tick still triages"
    );
    assert_eq!(
        reader.display_name(),
        None,
        "a name here would mean the unchanged file was copied and read again"
    );
    assert!(
        reader
            .last_info()
            .expect("the account is triaged")
            .display_name
            .is_none(),
        "the published data carries no name either"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins that a read which never completed is tried again, while the file
/// never changed. A signature describes a file's bytes, so it says nothing
/// about a disk that is full, a destination that is busy or permissions
/// that changed: a refused copy leaves the source exactly as it was, and
/// caching "no name" there would freeze the miss until the launcher next
/// wrote that file. The name is in the file the whole time here, and the
/// copy is refused only for the first tick, so a name that shows up on the
/// second one is exactly the retry this is about.
#[test]
fn test_a_failed_telemetry_read_is_retried() {
    let root = steam_tree("retry");
    let account = storage_of(&root).join("LocalStorage.db");
    write_account_db(&account, LIVE_UID, &[LIVE_UID]);
    write_telemetry_db(&telemetry_of(&root), &[(LIVE_UID, "RoverOne")]);

    let snap = root.join("snapshots-retry");
    let mut reader = reader_over(&root, "retry");
    refuse_copies_into(&snap.join("kdnames.db"));

    assert_eq!(reader.refresh(), Refresh::Updated);
    assert_eq!(
        reader.display_name(),
        None,
        "the copy was refused, so nothing was read and there is no name"
    );

    allow_copies_into(&snap.join("kdnames.db"));
    touch(&account);

    assert_eq!(
        reader.refresh(),
        Refresh::Updated,
        "the account database moved on, so the tick still triages"
    );
    assert_eq!(
        reader.display_name(),
        Some("RoverOne"),
        "the telemetry file never changed, but the read did fail: nothing was \
         learned, so the next tick must try the file again"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins the other half of the rule: a telemetry file that DID change is read
/// again, so a rename still shows up. Without this, a cache keyed on the
/// file would freeze the name for the rest of the session.
///
/// A signature is a file's length and its time. Whatever else the rewrite
/// below changes, this test does not leave the time to chance: it moves that
/// timestamp itself, and then asserts that the signature really did change.
/// The assertion is what the reader is being asked to take on trust — without
/// it, the two writes would have had to land on different recorded times by
/// luck, and on a filesystem with coarser ones they need not, so a reader
/// seeing one unchanged signature there would be right to go on serving the
/// first name.
#[test]
fn test_a_rewritten_telemetry_file_is_read_again() {
    let root = steam_tree("rewrite");
    let account = storage_of(&root).join("LocalStorage.db");
    write_account_db(&account, LIVE_UID, &[LIVE_UID]);
    let telemetry = telemetry_of(&root);
    write_telemetry_db(&telemetry, &[(LIVE_UID, "RoverOne")]);
    let first = file_signature(&telemetry).expect("the telemetry file is measurable");

    let mut reader = reader_over(&root, "rewrite");
    assert_eq!(reader.refresh(), Refresh::Updated);
    assert_eq!(
        reader.display_name(),
        Some("RoverOne"),
        "the first tick resolves the name the file holds"
    );

    write_telemetry_db(&telemetry, &[(LIVE_UID, "RoverTwo")]);
    // Pinned, not inherited: whatever granularity this filesystem keeps,
    // the version the reader sees next is one it can actually tell apart.
    touch(&telemetry);
    assert_ne!(
        file_signature(&telemetry).expect("the telemetry file is measurable"),
        first,
        "the rewrite must be a different version of the file"
    );
    touch(&account);

    assert_eq!(
        reader.refresh(),
        Refresh::Updated,
        "the account database moved on, so the tick triages again"
    );
    assert_eq!(
        reader.display_name(),
        Some("RoverTwo"),
        "a rewritten telemetry file is a new version and must be read"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins that a new login never inherits the previous one's name. The
/// telemetry file is written once and never touched again, so the cache
/// alone would answer every later login with the first role's name: only a
/// login switch can tell the two apart, and it must clear the cache with it.
#[test]
fn test_a_new_login_does_not_inherit_the_previous_name() {
    let root = steam_tree("switch");
    let account = storage_of(&root).join("LocalStorage.db");
    write_account_db(&account, LIVE_UID, &[LIVE_UID, OTHER_UID]);
    write_telemetry_db(
        &telemetry_of(&root),
        &[(LIVE_UID, "Alpha"), (OTHER_UID, "Beta")],
    );

    let mut reader = reader_over(&root, "switch");
    assert_eq!(reader.refresh(), Refresh::Updated);
    assert_eq!(
        reader.display_name(),
        Some("Alpha"),
        "the first login resolves the name its own rows hold"
    );

    write_account_db(&account, OTHER_UID, &[LIVE_UID, OTHER_UID]);
    touch(&account);

    assert_eq!(reader.refresh(), Refresh::Updated);
    assert_eq!(
        reader.display_name(),
        Some("Beta"),
        "the new login's own name, not the name cached for the previous one"
    );
    assert_eq!(reader.live_uid(), Some(OTHER_UID));

    let _ = fs::remove_dir_all(&root);
}

/// Pins the same rule for the other telemetry read: the scan for a deleted
/// row also copies the file, and "no fresh event in there" is its ordinary
/// answer. As in the live-name test, that answer has to be a real one rather
/// than a read that failed — so the first version of the file carries a
/// remnant for another role, which the scan reads and rejects. The second
/// carries the live login's instead, the same length with the timestamp put
/// back, so only a scan that ran again could recover the account.
#[test]
fn test_the_remnant_scan_is_not_redone_for_an_unchanged_file() {
    let root = steam_tree("remnant");
    let account = storage_of(&root).join("LocalStorage.db");
    // The live login has no level row yet: exactly the shape the telemetry
    // scan exists to recover.
    write_account_db(&account, LIVE_UID, &[OTHER_UID]);
    let telemetry = telemetry_of(&root);
    // The live rows are unreadable bytes on purpose — what this fixture
    // plants is a remnant, which is what the scan is for — and the first one
    // belongs to another role, so the scan finds nothing in it.
    let event_ms = epoch_millis() - 60_000;
    fs::write(&telemetry, remnant_bytes(OTHER_UID, "Recovered", event_ms))
        .expect("the remnant fixture is written");
    let before = file_signature(&telemetry).expect("the telemetry file is measurable");
    let before_time = written_at(&telemetry);

    let mut reader = reader_over(&root, "remnant");
    assert_eq!(
        reader.refresh(),
        Refresh::Unavailable,
        "no remnant matches the live login, so the account stays unresolved"
    );
    assert!(reader.last_info().is_none());

    fs::write(&telemetry, remnant_bytes(LIVE_UID, "Recovered", event_ms))
        .expect("the second remnant is written");
    restore_time(&telemetry, before_time);
    assert_eq!(
        file_signature(&telemetry).expect("the telemetry file is measurable"),
        before,
        "the two versions must be the same length and time, or this fixture \
         says nothing: the reader is allowed to re-scan a file that changed"
    );

    assert_eq!(
        reader.refresh(),
        Refresh::Unavailable,
        "the telemetry file has not changed, so the scan must not run again: \
         a recovered account here would prove it copied the file for nothing"
    );
    assert!(reader.last_info().is_none());

    touch(&telemetry);

    assert_eq!(
        reader.refresh(),
        Refresh::Updated,
        "a telemetry file that changed is scanned again"
    );
    assert_eq!(
        reader
            .last_info()
            .expect("the account is recovered from the remnant")
            .display_name
            .as_deref(),
        Some("Recovered")
    );

    let _ = fs::remove_dir_all(&root);
}
