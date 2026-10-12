// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Integration tests for the snapshot byte limits.
//!
//! Every game file wwrpc copies into its private snapshot directory has a
//! ceiling for its kind: a database, or the launcher's JSON cache. A file
//! past that ceiling is refused before the copy starts, and a refused copy
//! leaves nothing behind — a snapshot the SQLite queries then read is never
//! half a database.
//!
//! The fixtures that must be refused are grown with [`File::set_len`]: a
//! sparse extension keeps every SQLite header and page intact, so the file
//! stays a *readable* database. Anything that can still read it must refuse
//! to copy it, which is exactly the runaway-writer case the ceiling exists
//! for.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

use wwrpc::db::{DbReader, MAX_DB_BYTES, MAX_JSON_BYTES, Refresh, TriageError};
use wwrpc::runtime_dir::runtime_dir;

/// Prefix of every fixture directory this file creates.
const FIXTURE_PREFIX: &str = "wwrpc-limits";

/// Serializes the tests that call [`DbReader::list_accounts`]: it copies
/// into a scratch directory named after this process, and one test
/// asserts that no such directory survives.
static LIST_LOCK: Mutex<()> = Mutex::new(());

/// Holds [`LIST_LOCK`], recovering from a panic in an earlier test.
fn list_lock() -> MutexGuard<'static, ()> {
    LIST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Empty fixture directory for one test.
fn fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{FIXTURE_PREFIX}-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the fixture dir is created");
    dir
}

/// One account row set, enough for triage and for a listing to succeed.
const ACCOUNT_ROWS: &str = r#"
CREATE TABLE LocalStorage(key text primary key not null, value text not null);
INSERT INTO LocalStorage(key, value) VALUES('RecentlyLoginUID', '111111111');
INSERT INTO LocalStorage(key, value) VALUES('SdkLevelData',
  '{"Content": [["111111111", [{"Region": "Europe", "Level": 60}]]]}');
"#;

/// Write a readable `LocalStorage.db` holding one account.
fn write_account_db(path: &Path) {
    let conn = Connection::open(path).expect("the fixture database is created");
    conn.execute_batch(ACCOUNT_ROWS)
        .expect("the fixture rows are written");
}

/// Write a readable device database carrying `PatchVersion`.
fn write_patch_version_db(path: &Path) {
    let conn = Connection::open(path).expect("the device fixture is created");
    conn.execute_batch(
        r#"
        CREATE TABLE LocalStorage(key text primary key not null, value text not null);
        INSERT INTO LocalStorage(key, value) VALUES('PatchVersion', '"3.6.13"');
        "#,
    )
    .expect("the patch version row is written");
}

/// Stretch `path` one byte past `limit` without writing the bytes: whatever
/// the file already holds stays readable at its front, and the extension
/// itself costs nothing.
fn grow_past(path: &Path, limit: u64) {
    let file = File::options()
        .write(true)
        .open(path)
        .expect("the fixture database is writable");
    file.set_len(limit + 1)
        .expect("the fixture database is extended");
}

/// Launcher account cache padded past the JSON ceiling: the real file is a
/// few kilobytes, so the padding stands in for a runaway writer.
fn write_oversized_launcher_cache(root: &Path) {
    let dir = root.join(
        "steamapps/compatdata/3513350/pfx/drive_c/users/steamuser/AppData/Roaming/KR_G153/A1834",
    );
    fs::create_dir_all(&dir).expect("the launcher cache dir is created");
    let padding = "a".repeat(usize::try_from(MAX_JSON_BYTES).expect("the cap fits in memory"));
    fs::write(
        dir.join("KRSDKUserCache.json"),
        format!(
            r#"{{"last_login_cuid": "111111111", "account_list": [{{"cuid": "111111111", "thirdNickName": "RoverOne"}}], "pad": "{padding}"}}"#
        ),
    )
    .expect("the launcher cache is written");
}

/// Scratch directories `list_accounts` would have left behind in the
/// private runtime directory it copies into: a leftover there is a guard
/// that never dropped. The name carries this process's pid, so a listing
/// running in another test binary is never mistaken for one of ours.
fn list_dirs() -> Vec<PathBuf> {
    let base = runtime_dir().expect("the private runtime directory is available");
    let prefix = format!("wwrpc-list-{}-", std::process::id());
    fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect()
}

/// Pins the ceiling on the device database: past the cap the file is
/// corruption or the wrong directory, so the version lookup must decline —
/// and must leave no copy of it behind, not even a partial one.
#[test]
fn test_device_version_rejects_oversized() {
    let root = fixture("device");
    let storage = root.join("Client/Saved/LocalStorage");
    fs::create_dir_all(&storage).expect("the storage dir is created");
    write_account_db(&storage.join("LocalStorage.db"));

    // `DeviceSaved` sits next to `LocalStorage` under the game's `Saved` dir.
    let device = storage
        .parent()
        .expect("the storage dir has a parent")
        .join("DeviceSaved/DeviceStorage.db");
    fs::create_dir_all(device.parent().expect("the device dir has a parent"))
        .expect("the device dir is created");
    write_patch_version_db(&device);
    grow_past(&device, MAX_DB_BYTES);

    let snap = root.join("snapshots");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).expect("the reader opens");

    assert_eq!(
        reader.refresh(),
        Refresh::Updated,
        "the account database itself is small and readable"
    );
    assert_eq!(
        reader.cached_version(),
        None,
        "an oversized device database must not yield a version"
    );
    assert!(
        !snap.join("device.db").exists(),
        "a refused copy must leave nothing behind"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins the ceiling on the account listing: an oversized database is refused
/// before it is copied, so the listing fails instead of reading a snapshot
/// nobody bounded — and the scratch directory it created is gone either way.
#[test]
fn test_query_list_rejects_oversized() {
    let _lock = list_lock();
    let root = fixture("list");
    let storage = root.join("Client/Saved/LocalStorage");
    fs::create_dir_all(&storage).expect("the storage dir is created");
    let db = storage.join("LocalStorage.db");
    write_account_db(&db);
    grow_past(&db, MAX_DB_BYTES);

    let err = DbReader::list_accounts(&storage, &storage)
        .expect_err("an oversized database must not be listed");

    assert_eq!(err, TriageError::Unopenable);
    let leftovers = list_dirs();
    assert!(
        leftovers.is_empty(),
        "the listing must not leave its scratch dir behind: {leftovers:?}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins the JSON ceiling on the launcher cache: past a megabyte it is
/// refused, and the listing still works — nicknames are a recognition aid,
/// never a reason to fail.
#[test]
fn test_launcher_cache_copy_oversized_rejected() {
    let _lock = list_lock();
    let root = fixture("launcher");
    let game = root.join("steamapps/common/Wuthering Waves");
    let storage = game.join("Client/Saved/LocalStorage");
    fs::create_dir_all(&storage).expect("the storage dir is created");
    write_account_db(&storage.join("LocalStorage.db"));
    write_oversized_launcher_cache(&root);

    let listed = DbReader::list_accounts(&storage, &game).expect("the listing itself works");

    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].nickname, None,
        "an oversized launcher cache must not be read"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins the per-tick snapshot decision: the same path is re-copied every
/// tick, so whatever a killed run left there must be replaced, never
/// mistaken for a snapshot that may not be replaced.
#[test]
fn test_snapshot_overwrites_a_stale_copy() {
    let root = fixture("stale");
    let storage = root.join("Client/Saved/LocalStorage");
    fs::create_dir_all(&storage).expect("the storage dir is created");
    write_account_db(&storage.join("LocalStorage.db"));
    let snap = root.join("snapshots");
    fs::create_dir_all(&snap).expect("the snapshot dir is created");
    fs::write(snap.join("snapshot.db"), b"whatever a killed run left")
        .expect("the leftover snapshot is written");

    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).expect("the reader opens");

    assert_eq!(
        reader.refresh(),
        Refresh::Updated,
        "a stale snapshot must be replaced, not refused"
    );
    assert!(
        !snap.join("snapshot.db").exists(),
        "the snapshot is removed after the tick that read it"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Pins the mode at a snapshot that lives in the private runtime directory:
/// that directory is never emptied, so a file a killed run left behind must
/// be replaced. Refusing it instead would cost the game version every later
/// run, silently. The leftover is grown sparse so the test costs nothing.
#[test]
fn test_device_version_replaces_a_stale_copy() {
    let root = fixture("stale-device");
    let storage = root.join("Client/Saved/LocalStorage");
    fs::create_dir_all(&storage).expect("the storage dir is created");
    write_account_db(&storage.join("LocalStorage.db"));

    let device = storage
        .parent()
        .expect("the storage dir has a parent")
        .join("DeviceSaved/DeviceStorage.db");
    fs::create_dir_all(device.parent().expect("the device dir has a parent"))
        .expect("the device dir is created");
    write_patch_version_db(&device);

    let snap = root.join("snapshots");
    fs::create_dir_all(&snap).expect("the snapshot dir is created");
    let orphan = snap.join("device.db");
    fs::write(&orphan, b"torn").expect("the leftover snapshot is written");
    grow_past(&orphan, MAX_DB_BYTES);

    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).expect("the reader opens");

    assert_eq!(reader.refresh(), Refresh::Updated);
    assert_eq!(
        reader.cached_version(),
        Some("3.6.13"),
        "a leftover snapshot must be replaced, not obeyed"
    );
    assert!(
        !orphan.exists(),
        "the snapshot is removed after the tick that read it"
    );

    let _ = fs::remove_dir_all(&root);
}
