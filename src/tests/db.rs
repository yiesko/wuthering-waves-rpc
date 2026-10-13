use std::fs;

use crate::db::{
    DbReader, MAX_REMANT_AGE_MS, Refresh, RemnantStats, TriageError, accepted_message,
    check_integrity, check_schema, discover_game_dir, find_remnant_role, has_account_data,
    is_numbered_db, load_sdk_level, load_sdk_level_by_index, load_uid, missing_level_message,
    parse_account_list, parse_quest_count, parse_sdk_level_data, recovered_message,
    resolve_storage, resolve_storage_dir, scan_census_message,
};

const SDK_BODY: &str = r#"{
  "___MetaType___": "___Map___",
  "Content": [
    ["111111111", [{"Region": "Europe", "Level": 60}]],
    ["222222222", [{"Region": "America", "Level": "30"}]]
  ]
}"#;

#[test]
fn numbered_db_names_match_only_numbered_files() {
    assert!(!is_numbered_db("LocalStorage.db"));
    assert!(!is_numbered_db("LocalStorage.db-journal"));
    assert!(!is_numbered_db("LocalStorage.db-shm"));
    assert!(!is_numbered_db("LocalStorage2.db-journal"));
    assert!(!is_numbered_db("other2.db"));
    assert!(is_numbered_db("LocalStorage2.db"));
    assert!(is_numbered_db("LocalStorage10.db"));
}

#[test]
fn sdk_level_data_parses_numeric_and_string_levels() {
    assert_eq!(
        parse_sdk_level_data(SDK_BODY, "111111111"),
        Some(("Europe".to_string(), "60".to_string()))
    );
    assert_eq!(
        parse_sdk_level_data(SDK_BODY, "222222222"),
        Some(("America".to_string(), "30".to_string()))
    );
}

#[test]
fn sdk_level_data_rejects_unknown_uid_and_garbage() {
    assert_eq!(parse_sdk_level_data(SDK_BODY, "999999999"), None);
    assert_eq!(parse_sdk_level_data("not json", "111111111"), None);
    assert_eq!(parse_sdk_level_data("{}", "111111111"), None);
    assert_eq!(
        parse_sdk_level_data(
            r#"{"Content": [["111111111", [{"Region": "", "Level": 1}]]}"#,
            "111111111"
        ),
        None
    );
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("wwrpc-test-{}-{}", std::process::id(), tag))
}

fn make_storage(tag: &str, uid: &str, region: &str, level: u64) -> std::path::PathBuf {
    let dir = unique_dir(tag);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("LocalStorage.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "CREATE TABLE LocalStorage(key text primary key not null, value text not null)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO LocalStorage(key, value) VALUES (?1, ?2)",
        ["RecentlyLoginUID", uid],
    )
    .unwrap();
    let sdk = format!(
        r#"{{"___MetaType___": "___Map___", "Content": [["{uid}", [{{"Region": "{region}", "Level": {level}}}]]]}}"#
    );
    conn.execute(
        "INSERT INTO LocalStorage(key, value) VALUES (?1, ?2)",
        ["SdkLevelData", &sdk],
    )
    .unwrap();
    drop(conn);
    dir
}

const QUESTS_BODY: &str = r#"{
  "___MetaType___": "___Map___",
  "Content": [
    ["111111111", {"___MetaType___": "___Map___", "Content": [1, 2, 3]}],
    ["222222222", {"___MetaType___": "___Map___", "Content": []}]
  ]
}"#;

#[test]
fn quest_count_counts_entries_per_uid() {
    assert_eq!(parse_quest_count(QUESTS_BODY, "111111111"), Some(3));
    assert_eq!(parse_quest_count(QUESTS_BODY, "222222222"), Some(0));
    assert_eq!(parse_quest_count(QUESTS_BODY, "999999999"), None);
    assert_eq!(parse_quest_count("not json", "111111111"), None);
}

#[test]
fn refresh_reads_copy_and_caches_by_signature() {
    let storage = make_storage("basic", "111111111", "Europe", 60);
    let snap = unique_dir("basic-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();

    assert_eq!(reader.refresh(), Refresh::Updated);
    assert_eq!(reader.last_info().unwrap().union_level, "60".to_string());
    assert_eq!(reader.last_info().unwrap().region, "Europe".to_string());
    // No snapshot leftovers: the private copy is deleted after each read.
    assert!(!snap.join("snapshot.db").exists());

    // Nothing the snapshot is built from was written: no re-read.
    assert_eq!(reader.refresh(), Refresh::Unchanged);

    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn refresh_honors_uid_override_and_sentinel() {
    let storage = make_storage("override", "111111111", "Europe", 60);
    let snap = unique_dir("override-snap");
    let mut reader =
        DbReader::new(storage.clone(), snap.clone(), Some("999999999".to_string())).unwrap();
    // Unknown override uid: no data, but no crash either.
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert!(reader.last_info().is_none());

    // A numbered database trips the fail-safe permanently.
    fs::write(storage.join("LocalStorage2.db"), b"junk").unwrap();
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert!(reader.is_degraded());
    fs::remove_file(storage.join("LocalStorage2.db")).unwrap();
    // Still degraded: game files are left alone for the rest of the session.
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert!(reader.is_degraded());

    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

/// Build a storage dir whose `LocalStorage.db` is created from raw SQL,
/// for triage tests that need broken schemas.
fn storage_with_sql(tag: &str, setup: &[&str]) -> std::path::PathBuf {
    let dir = unique_dir(tag);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let conn = rusqlite::Connection::open(dir.join("LocalStorage.db")).unwrap();
    for sql in setup {
        conn.execute_batch(sql).unwrap();
    }
    drop(conn);
    dir
}

const VALID_SCHEMA: &str =
    "CREATE TABLE LocalStorage(key text primary key not null, value text not null)";
const UID_ROW: &str =
    "INSERT INTO LocalStorage(key, value) VALUES ('RecentlyLoginUID', '111111111')";
const SDK_ROW: &str = "INSERT INTO LocalStorage(key, value) VALUES ('SdkLevelData', '{\"___MetaType___\": \"___Map___\", \"Content\": [[\"111111111\", [{\"Region\": \"Europe\", \"Level\": 60}]]]}')";

fn mem_db(setup: &[&str]) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for sql in setup {
        conn.execute_batch(sql).unwrap();
    }
    conn
}

#[test]
fn triage_rejects_missing_table() {
    let storage = storage_with_sql("no-table", &["CREATE TABLE Other(x)"]);
    let snap = unique_dir("no-table-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert!(reader.last_info().is_none());
    assert_eq!(
        check_schema(&mem_db(&["CREATE TABLE Other(x)"])),
        Err(TriageError::MissingTable)
    );
    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn triage_rejects_bad_schema() {
    let storage = storage_with_sql("bad-schema", &["CREATE TABLE LocalStorage(a, b)"]);
    let snap = unique_dir("bad-schema-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert!(reader.last_info().is_none());
    assert_eq!(
        check_schema(&mem_db(&["CREATE TABLE LocalStorage(a, b)"])),
        Err(TriageError::BadSchema)
    );
    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn triage_rejects_corrupt_file() {
    let storage = unique_dir("corrupt");
    let _ = fs::remove_dir_all(&storage);
    fs::create_dir_all(&storage).unwrap();
    fs::write(storage.join("LocalStorage.db"), b"definitely not sqlite").unwrap();
    let snap = unique_dir("corrupt-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert!(reader.last_info().is_none());
    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn triage_rejects_missing_sdk_key() {
    let storage = storage_with_sql("no-sdk", &[VALID_SCHEMA, UID_ROW]);
    let snap = unique_dir("no-sdk-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert!(reader.last_info().is_none());
    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn triage_rejects_malformed_sdk_json() {
    let storage = storage_with_sql(
        "bad-sdk",
        &[
            VALID_SCHEMA,
            UID_ROW,
            "INSERT INTO LocalStorage(key, value) VALUES ('SdkLevelData', 'oops')",
        ],
    );
    let snap = unique_dir("bad-sdk-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert!(reader.last_info().is_none());
    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn triage_rejects_missing_uid() {
    // No RecentlyLoginUID and no override: no account can be selected.
    let storage = storage_with_sql("no-uid", &[VALID_SCHEMA, SDK_ROW]);
    let snap = unique_dir("no-uid-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert!(reader.last_info().is_none());
    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn triage_rejects_stale_override_uid() {
    // Override present but absent from SdkLevelData: reject, don't guess.
    let storage = storage_with_sql("stale-uid", &[VALID_SCHEMA, UID_ROW, SDK_ROW]);
    let snap = unique_dir("stale-uid-snap");
    let mut reader =
        DbReader::new(storage.clone(), snap.clone(), Some("000000000".to_string())).unwrap();
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert!(reader.last_info().is_none());
    let conn = mem_db(&[VALID_SCHEMA, UID_ROW, SDK_ROW]);
    assert_eq!(
        load_uid(&conn, Some("000000000")),
        Ok("000000000".to_string())
    );
    assert_eq!(
        load_sdk_level(&conn, "000000000"),
        Err(TriageError::AccountNotInLevelData {
            known_accounts: 1,
            uid_seen_elsewhere: false,
        })
    );
    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn triage_missing_file_is_unavailable_not_fatal() {
    let storage = unique_dir("missing-dir");
    let _ = fs::remove_dir_all(&storage);
    let snap = unique_dir("missing-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert!(reader.last_info().is_none());
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn triage_failure_is_tracked_and_hints_stay_generic() {
    let storage = storage_with_sql("tracked", &[VALID_SCHEMA, UID_ROW, SDK_ROW]);
    let snap = unique_dir("tracked-snap");
    let mut reader =
        DbReader::new(storage.clone(), snap.clone(), Some("000000000".to_string())).unwrap();

    assert_eq!(reader.last_triage(), None);
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert_eq!(
        reader.last_triage(),
        Some(TriageError::AccountNotInLevelData {
            known_accounts: 1,
            uid_seen_elsewhere: false,
        })
    );
    // Persists across ticks (dedup key); success would reset it.
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert_eq!(
        reader.last_triage(),
        Some(TriageError::AccountNotInLevelData {
            known_accounts: 1,
            uid_seen_elsewhere: false,
        })
    );

    // Hints guide to --kuro-uid without echoing account values.
    let err = reader.last_triage().unwrap();
    assert!(reader.hint(err).contains("--kuro-uid"));
    assert!(!reader.hint(err).contains("000000000"));

    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

/// Pins that a sidecar which cannot be measured is LOUD once — not once per
/// tick, and not once ever.
///
/// A tick that cannot take its signature keeps its data and carries on,
/// which is right: nothing is refused, and the condition usually clears by
/// itself. But a condition that persists then holds the presence at its
/// last value for the rest of the session, and the warning is the only
/// thing that tells a reader of the log so. So the warning must exist, and
/// it must be deduplicated: one line while the fault is still there, and a
/// fresh one if it clears and returns, because by then the situation has
/// changed and the earlier line is stale.
///
/// What is asserted is both halves of "once". The record (`last_warning`)
/// says which fault the reader is holding, and the count
/// (`signature_warnings`) says how many lines it has actually printed —
/// the count is what a record-only test cannot reach, since the record
/// reads the same whether the first tick printed or the fifth did. The
/// lines themselves go to the process's stderr, which a test cannot read,
/// so the count is kept beside the emission. Lives here rather than in
/// `tests/` because both accessors are crate-private, next to the triage
/// dedup case above it.
#[test]
fn unmeasurable_sidecar_warns_once_and_again_when_it_returns() {
    let storage = storage_with_sql("unmeasurable", &[VALID_SCHEMA, UID_ROW, SDK_ROW]);
    let snap = unique_dir("unmeasurable-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();

    assert_eq!(reader.refresh(), Refresh::Updated);
    assert_eq!(
        reader.last_warning(),
        None,
        "a healthy reader has said nothing"
    );

    // A sidecar that is there and cannot be read through: a symlink loop,
    // which the kernel refuses to resolve every time.
    let wal = storage.join("LocalStorage.db-wal");
    std::os::unix::fs::symlink("LocalStorage.db-wal", &wal).unwrap();
    for tick in 1..=5 {
        assert_eq!(
            reader.refresh(),
            Refresh::Unchanged,
            "tick {tick} is held at the last accepted data"
        );
    }
    assert_eq!(
        reader.last_warning(),
        Some("signature"),
        "a fault that persists is reported, and the record does not change \
         across the ticks that follow, which is what stops them printing again"
    );
    // The count, which is the part a test of the record alone cannot reach:
    // the record reads the same whether the first tick printed a line or
    // the fifth did. Without this, dropping the guard would leave every
    // assertion above passing and the warning would repeat every tick.
    assert_eq!(
        reader.signature_warnings(),
        1,
        "five blocked ticks, one line: the dedup bounds a persistent fault \
         to a single warning rather than one per tick"
    );

    // A healthy tick clears the record, so the fault returning is news.
    fs::remove_file(&wal).unwrap();
    let db = storage.join("LocalStorage.db");
    let later = fs::metadata(&db).unwrap().modified().unwrap() + std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&db)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(later))
        .unwrap();
    assert_eq!(
        reader.refresh(),
        Refresh::Updated,
        "a healthy tick triages again"
    );
    assert_eq!(
        reader.last_warning(),
        None,
        "a tick with nothing to report clears the record, so a fault that \
         returns later is a change and not a repeat of the old one"
    );

    std::os::unix::fs::symlink("LocalStorage.db-wal", &wal).unwrap();
    assert_eq!(reader.refresh(), Refresh::Unchanged);
    assert_eq!(
        reader.last_warning(),
        Some("signature"),
        "and the returning fault is reported again rather than swallowed by \
         the record of the one that was fixed"
    );
    assert_eq!(
        reader.signature_warnings(),
        2,
        "and it is a second line, not a silent repeat: a fault that clears and \
         returns has changed the situation, which is what the record clearing \
         on a successful tick buys"
    );

    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn hint_without_override_points_to_list_accounts() {
    let storage = unique_dir("hint-auto");
    let _ = fs::remove_dir_all(&storage);
    fs::create_dir_all(&storage).unwrap();
    let snap = unique_dir("hint-auto-snap");
    let reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    let hint = reader.hint(TriageError::AccountNotInLevelData {
        known_accounts: 2,
        uid_seen_elsewhere: false,
    });
    assert!(hint.contains("--list-accounts"), "unexpected hint: {hint}");
    assert!(hint.contains("--account-index"), "unexpected hint: {hint}");
    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn triage_known_account_missing_level_reports_seen() {
    // Like the real incident: per-uid keys exist, the level row does not.
    let storage = storage_with_sql(
        "seen-elsewhere",
        &[
            VALID_SCHEMA,
            UID_ROW,
            SDK_ROW,
            "INSERT INTO LocalStorage(key, value) VALUES ('LoginTime_000000000', '123')",
        ],
    );
    let snap = unique_dir("seen-elsewhere-snap");
    let mut reader =
        DbReader::new(storage.clone(), snap.clone(), Some("000000000".to_string())).unwrap();
    assert_eq!(reader.refresh(), Refresh::Unavailable);
    assert_eq!(
        reader.last_triage(),
        Some(TriageError::AccountNotInLevelData {
            known_accounts: 1,
            uid_seen_elsewhere: true,
        })
    );
    assert!(
        reader
            .hint(reader.last_triage().unwrap())
            .contains("--list-accounts, --account-index N")
    );

    let conn = mem_db(&[
        VALID_SCHEMA,
        UID_ROW,
        SDK_ROW,
        "INSERT INTO LocalStorage(key, value) VALUES ('LoginTime_000000000', '123')",
    ]);
    assert!(has_account_data(&conn, "000000000"));
    assert!(!has_account_data(&conn, "999999999"));

    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn triage_missing_quests_still_updates() {
    // Quests are best-effort: identity data alone is enough for presence.
    let storage = storage_with_sql("no-quests", &[VALID_SCHEMA, UID_ROW, SDK_ROW]);
    let snap = unique_dir("no-quests-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    assert_eq!(reader.refresh(), Refresh::Updated);
    let info = reader.last_info().unwrap();
    assert_eq!(info.union_level, "60".to_string());
    assert_eq!(info.region, "Europe".to_string());
    assert_eq!(info.quests_done, None);
    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn triage_valid_snapshot_passes_integrity() {
    let conn = mem_db(&[VALID_SCHEMA, UID_ROW, SDK_ROW]);
    assert_eq!(check_schema(&conn), Ok(()));
    assert_eq!(check_integrity(&conn), Ok(()));
    assert_eq!(load_uid(&conn, None), Ok("111111111".to_string()));
    // Empty or blank override falls back to auto-detection.
    assert_eq!(load_uid(&conn, Some("")), Ok("111111111".to_string()));
    assert_eq!(load_uid(&conn, Some("   ")), Ok("111111111".to_string()));
    assert_eq!(
        load_sdk_level(&conn, "111111111"),
        Ok(("Europe".to_string(), "60".to_string()))
    );
}

#[test]
fn triage_reads_device_version_best_effort() {
    // Layout: <root>/Saved/{LocalStorage, DeviceSaved/DeviceStorage.db}.
    let root = unique_dir("device");
    let _ = fs::remove_dir_all(&root);
    let storage = root.join("Saved/LocalStorage");
    fs::create_dir_all(&storage).unwrap();
    let conn = rusqlite::Connection::open(storage.join("LocalStorage.db")).unwrap();
    for sql in [VALID_SCHEMA, UID_ROW, SDK_ROW] {
        conn.execute_batch(sql).unwrap();
    }
    drop(conn);
    let snap = unique_dir("device-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();

    // No device dir: still Updated, version None.
    assert_eq!(reader.refresh(), Refresh::Updated);
    assert_eq!(reader.last_info().unwrap().game_version, None);

    // With device db: version picked up, no leftovers in the snapshot dir.
    let mut reader2 = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    let dev_dir = root.join("Saved/DeviceSaved");
    fs::create_dir_all(&dev_dir).unwrap();
    let dev = rusqlite::Connection::open(dev_dir.join("DeviceStorage.db")).unwrap();
    dev.execute_batch(
        "CREATE TABLE LocalStorage(key text primary key not null, value text not null)",
    )
    .unwrap();
    dev.execute(
        "INSERT INTO LocalStorage(key, value) VALUES ('PatchVersion', '\"9.9.9\"')",
        [],
    )
    .unwrap();
    assert_eq!(reader2.refresh(), Refresh::Updated);
    assert_eq!(
        reader2.last_info().unwrap().game_version.as_deref(),
        Some("9.9.9")
    );
    assert!(!snap.join("device.db").exists());

    // Non-string version: ignored, presence still updates.
    dev.execute(
        "UPDATE LocalStorage SET value = '123' WHERE key = 'PatchVersion'",
        [],
    )
    .unwrap();
    drop(dev);
    let mut reader3 = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    assert_eq!(reader3.refresh(), Refresh::Updated);
    assert_eq!(reader3.last_info().unwrap().game_version, None);

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn resolve_storage_prefers_explicit_dir() {
    let storage = storage_layout("resolve-explicit", true);
    // Explicit nested dir resolves without discovery.
    let (game, resolved) =
        resolve_storage(Some(storage.as_path()), &[]).expect("explicit dir resolves");
    assert_eq!(game, storage);
    assert_eq!(
        resolved,
        storage.join("Wuthering Waves Game/Client/Saved/LocalStorage")
    );
    // Explicit bogus dir errors with a helpful message.
    let bogus = unique_dir("resolve-bogus");
    let _ = fs::remove_dir_all(&bogus);
    fs::create_dir_all(&bogus).unwrap();
    let err = resolve_storage(Some(bogus.as_path()), &[]).unwrap_err();
    assert!(
        err.to_string().contains("no Client/Saved/LocalStorage"),
        "unexpected error: {err}"
    );
    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&bogus);
}

const SDK_TWO: &str = "INSERT INTO LocalStorage(key, value) VALUES ('SdkLevelData', '{\"___MetaType___\": \"___Map___\", \"Content\": [[\"111111111\", [{\"Region\": \"Europe\", \"Level\": 60}]], [\"222222222\", [{\"Region\": \"America\", \"Level\": 30}]]]}')";
const QUESTS_TWO: &str = "INSERT INTO LocalStorage(key, value) VALUES ('UserFinishedQuests', '{\"___MetaType___\": \"___Map___\", \"Content\": [[\"111111111\", {\"Content\": [1]}], [\"222222222\", {\"Content\": [1, 2, 3]}]]}')";

#[test]
fn load_by_index_selects_position() {
    let conn = mem_db(&[VALID_SCHEMA, SDK_TWO]);
    let first = load_sdk_level_by_index(&conn, 1).expect("first entry");
    assert_eq!(first.uid, "111111111".to_string());
    assert_eq!(first.region, "Europe".to_string());
    let second = load_sdk_level_by_index(&conn, 2).expect("second entry");
    assert_eq!(second.uid, "222222222".to_string());
    assert_eq!(second.level, "30".to_string());
    assert_eq!(
        load_sdk_level_by_index(&conn, 0),
        Err(TriageError::UnknownUid)
    );
    assert_eq!(
        load_sdk_level_by_index(&conn, 3),
        Err(TriageError::UnknownUid)
    );
}

#[test]
fn refresh_uses_account_index_over_uid_resolution() {
    let storage = storage_with_sql("by-index", &[VALID_SCHEMA, UID_ROW, SDK_TWO, QUESTS_TWO]);
    let snap = unique_dir("by-index-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    // Auto path resolves the RecentlyLoginUID account...
    assert_eq!(reader.refresh(), Refresh::Updated);
    assert_eq!(reader.last_info().unwrap().uid, "111111111".to_string());

    // ...while an explicit index pins the other one, quests included.
    let mut pinned = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    pinned.set_account_index(2);
    assert_eq!(pinned.refresh(), Refresh::Updated);
    let info = pinned.last_info().unwrap();
    assert_eq!(info.uid, "222222222".to_string());
    assert_eq!(info.union_level, "30".to_string());
    assert_eq!(info.region, "America".to_string());
    assert_eq!(info.quests_done, Some(3));

    // Out of range degrades gracefully instead of failing.
    let mut bad = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    bad.set_account_index(9);
    assert_eq!(bad.refresh(), Refresh::Unavailable);
    assert!(bad.last_info().is_none());

    let _ = fs::remove_dir_all(&storage);
    let _ = fs::remove_dir_all(&snap);
}

#[test]
fn list_accounts_reads_without_uids() {
    let storage = storage_with_sql("list", &[VALID_SCHEMA, UID_ROW, SDK_TWO]);
    // No launcher cache around: nicknames stay absent, listing still works.
    let listed = DbReader::list_accounts(&storage, &storage).expect("listing works");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].index, 1);
    assert_eq!(listed[0].region, "Europe".to_string());
    assert_eq!(listed[0].level, "60".to_string());
    assert_eq!(listed[0].nickname, None);
    assert_eq!(listed[1].nickname, None);
    let missing = unique_dir("list-missing");
    let _ = fs::remove_dir_all(&missing);
    assert!(DbReader::list_accounts(&missing, &missing).is_err());
    let _ = fs::remove_dir_all(&storage);
}

#[test]
fn list_accounts_attaches_login_nicknames() {
    // Fake Steam tree: <root>/steamapps/{common/<game>/Client/..., compatdata/...}.
    let root = unique_dir("nicknames");
    let _ = fs::remove_dir_all(&root);
    let game = root.join("steamapps/common/Wuthering Waves");
    fs::create_dir_all(game.join("Client/Saved/LocalStorage")).unwrap();
    let conn =
        rusqlite::Connection::open(game.join("Client/Saved/LocalStorage/LocalStorage.db")).unwrap();
    for sql in [VALID_SCHEMA, SDK_TWO] {
        conn.execute_batch(sql).unwrap();
    }
    drop(conn);
    let cache_dir = root.join(
        "steamapps/compatdata/3513350/pfx/drive_c/users/steamuser/AppData/Roaming/KR_G153/A1834",
    );
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(
        cache_dir.join("KRSDKUserCache.json"),
        r#"{"last_login_cuid": "222222222", "account_list": [
      {"cuid": "111111111", "thirdNickName": "RoverOne", "email": "x", "token": "y"},
      {"cuid": "222222222", "thirdNickName": "", "email": "x", "token": "y"},
      {"cuid": "333333333"}
    ]}"#,
    )
    .unwrap();

    let storage = game.join("Client/Saved/LocalStorage");
    let listed = DbReader::list_accounts(&storage, &game).expect("listing works");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].nickname.as_deref(), Some("RoverOne"));
    // Empty nicknames are skipped, unknown cuids stay absent.
    assert_eq!(listed[1].nickname, None);
    // Last-login marker follows cuid equality only — never displayed uids.
    assert!(!listed[0].last_login);
    assert!(listed[1].last_login);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn launcher_cache_path_resolves_steam_layout_only() {
    let root = unique_dir("prefix");
    let _ = fs::remove_dir_all(&root);
    let game = root.join("steamapps/common/Wuthering Waves");
    fs::create_dir_all(&game).unwrap();
    // No cache file yet: None (best-effort, never an error).
    assert_eq!(crate::db::launcher_cache_path(&game), None);
    let cache_dir = root.join(
        "steamapps/compatdata/3513350/pfx/drive_c/users/steamuser/AppData/Roaming/KR_G153/A1834",
    );
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(cache_dir.join("KRSDKUserCache.json"), "{}").unwrap();
    assert!(crate::db::launcher_cache_path(&game).is_some());
    // Outside any steamapps tree: None.
    assert_eq!(crate::db::launcher_cache_path(&unique_dir("nope")), None);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn triage_errors_never_echo_account_values() {
    // Privacy invariant: key names are fine in logs, values/uids are not.
    let probe = "111111111";
    let errors = [
        TriageError::Unopenable,
        TriageError::MissingTable,
        TriageError::BadSchema,
        TriageError::Corrupt,
        TriageError::MissingKey("SdkLevelData"),
        TriageError::Malformed("SdkLevelData"),
        TriageError::UnknownUid,
        TriageError::AccountNotInLevelData {
            known_accounts: 2,
            uid_seen_elsewhere: false,
        },
        TriageError::AccountNotInLevelData {
            known_accounts: 2,
            uid_seen_elsewhere: true,
        },
    ];
    for err in errors {
        assert!(
            !format!("{err}").contains(probe),
            "triage message must not echo account values: {err}"
        );
    }
}

fn storage_layout(tag: &str, nested: bool) -> std::path::PathBuf {
    let root = unique_dir(tag);
    let _ = fs::remove_dir_all(&root);
    let base = if nested {
        root.join("Wuthering Waves Game")
    } else {
        root.clone()
    };
    fs::create_dir_all(base.join("Client/Saved/LocalStorage")).unwrap();
    root
}

#[test]
fn resolve_storage_dir_accepts_both_layouts() {
    // Steam layout: Client/ directly under the game dir.
    let steam = storage_layout("layout-steam", false);
    assert_eq!(
        resolve_storage_dir(&steam),
        Some(steam.join("Client/Saved/LocalStorage"))
    );

    // Standalone launchers (Twintail/Heroic/...): nested game folder.
    let standalone = storage_layout("layout-standalone", true);
    assert_eq!(
        resolve_storage_dir(&standalone),
        Some(standalone.join("Wuthering Waves Game/Client/Saved/LocalStorage"))
    );

    // Anything else: explicit --game-dir (or an install) is required.
    let empty = unique_dir("layout-empty");
    let _ = fs::remove_dir_all(&empty);
    fs::create_dir_all(&empty).unwrap();
    assert_eq!(resolve_storage_dir(&empty), None);

    let _ = fs::remove_dir_all(&steam);
    let _ = fs::remove_dir_all(&standalone);
    let _ = fs::remove_dir_all(&empty);
}

#[test]
fn discover_game_dir_searches_roots_and_one_level() {
    let roots = unique_dir("discover-roots");
    let _ = fs::remove_dir_all(&roots);
    // A nested Twintail-style folder inside a root.
    let nested = roots.join("Some Folder").join("Wuthering Waves");
    fs::create_dir_all(nested.join("Wuthering Waves Game/Client/Saved/LocalStorage")).unwrap();

    let found = discover_game_dir(std::slice::from_ref(&roots)).expect("finds nested install");
    assert_eq!(found, roots.join("Some Folder").join("Wuthering Waves"));

    // A root that is itself a game dir wins without descending.
    let direct = storage_layout("discover-direct", false);
    assert_eq!(
        discover_game_dir(std::slice::from_ref(&direct)),
        Some(direct.clone())
    );

    assert_eq!(discover_game_dir(&[unique_dir("discover-absent")]), None);

    let _ = fs::remove_dir_all(&roots);
    let _ = fs::remove_dir_all(&direct);
}

#[test]
fn garbage_sibling_rows_never_hide_a_valid_match() {
    // The exact dedup hazard: a malformed row for ANOTHER account must not
    // prevent resolving a valid one (nor listing it).
    let body = r#"{"Content": [
    ["9", [{"Region": "X", "Level": true}]],
    "not-an-entry",
    ["111111111", [{"Region": "Europe", "Level": 60}]],
    ["222222222", [{"Region": "", "Level": 1}]]
  ]}"#;
    assert_eq!(
        parse_sdk_level_data(body, "111111111"),
        Some(("Europe".to_string(), "60".to_string()))
    );
    assert_eq!(
        parse_account_list(body),
        Some(vec![("Europe".to_string(), "60".to_string())])
    );
    assert_eq!(parse_sdk_level_data(body, "000000000"), None);
}

#[test]
fn query_kd_names_matches_exact_live_rows_only() {
    fn fixture(tag: &str, rows: &[&str]) -> std::path::PathBuf {
        let dir = unique_dir(tag);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kd.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE KDData (id INTEGER PRIMARY KEY AUTOINCREMENT, content TEXT)",
        )
        .unwrap();
        for row in rows {
            conn.execute("INSERT INTO KDData(content) VALUES (?1)", [row])
                .unwrap();
        }
        drop(conn);
        path
    }

    let path = fixture(
        "kd-exact",
        &[
            r#"{"role_id":"111111111","role_name":"First"}"#,
            r#"{"role_id":"222222222","role_name":"Al"}"#,
            r#"{"role_id":"111111111","role_name":"Second"}"#,
            r#"{"role_id":"333333333"}"#,
            r#"{"role_id":"444444444","role_name":""}"#,
            r#"{"role_id":555555555,"role_name":"Numeric"}"#,
            "not json at all",
        ],
    );
    // First exact match wins; other uids, malformed rows and garbage pass by.
    assert_eq!(
        crate::db::query_kd_names(&path, "111111111"),
        crate::telemetry_cache::Lookup::Answered(Some("First".to_string()))
    );
    assert_eq!(
        crate::db::query_kd_names(&path, "222222222"),
        crate::telemetry_cache::Lookup::Answered(Some("Al".to_string()))
    );
    assert_eq!(
        crate::db::query_kd_names(&path, "555555555"),
        crate::telemetry_cache::Lookup::Answered(Some("Numeric".to_string()))
    );
    assert_eq!(
        crate::db::query_kd_names(&path, "999999999"),
        crate::telemetry_cache::Lookup::Answered(None)
    );
    let _ = fs::remove_dir_all(path.parent().unwrap());

    // A search that ran answers, even when it found nothing; a file that
    // could not be read does not.
    let empty_dir = unique_dir("kd-empty");
    let _ = fs::remove_dir_all(&empty_dir);
    fs::create_dir_all(&empty_dir).unwrap();
    let empty = empty_dir.join("kd.db");
    rusqlite::Connection::open(&empty)
        .unwrap()
        .execute_batch("CREATE TABLE KDData (id INTEGER PRIMARY KEY)")
        .unwrap();
    assert_eq!(
        crate::db::query_kd_names(&empty, "111111111"),
        crate::telemetry_cache::Lookup::Unread
    );
    assert_eq!(
        crate::db::query_kd_names(&empty_dir.join("missing.db"), "111111111"),
        crate::telemetry_cache::Lookup::Unread
    );
    let garbage = empty_dir.join("garbage.db");
    fs::write(&garbage, b"not a database at all").unwrap();
    assert_eq!(
        crate::db::query_kd_names(&garbage, "111111111"),
        crate::telemetry_cache::Lookup::Unread
    );
    let _ = fs::remove_dir_all(&empty_dir);
}

#[test]
fn refresh_attaches_live_display_name() {
    // Steam-like tree so kddata_path resolves: <root>/steamapps/{common,compatdata}.
    let root = unique_dir("kdname");
    let _ = fs::remove_dir_all(&root);
    let game = root.join("steamapps/common/Wuthering Waves");
    fs::create_dir_all(game.join("Client/Saved/LocalStorage")).unwrap();
    let storage = game.join("Client/Saved/LocalStorage");
    let conn = rusqlite::Connection::open(storage.join("LocalStorage.db")).unwrap();
    for sql in [VALID_SCHEMA, UID_ROW, SDK_ROW] {
        conn.execute_batch(sql).unwrap();
    }
    drop(conn);
    let kd_dir = root
        .join("steamapps/compatdata/3513350/pfx/drive_c/users/steamuser/AppData/Roaming/KR_G153");
    fs::create_dir_all(&kd_dir).unwrap();
    let kd = rusqlite::Connection::open(kd_dir.join("KDData-data.db")).unwrap();
    kd.execute_batch("CREATE TABLE KDData (id INTEGER PRIMARY KEY AUTOINCREMENT, content TEXT)")
        .unwrap();
    kd.execute(
    "INSERT INTO KDData(content) VALUES ('{\"role_id\":\"111111111\",\"role_name\":\"TestRover\"}')",
    [],
  )
  .unwrap();
    drop(kd);

    let snap = unique_dir("kdname-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    reader.set_game_dir(game.clone());
    assert_eq!(reader.refresh(), Refresh::Updated);
    assert_eq!(
        reader.last_info().unwrap().display_name.as_deref(),
        Some("TestRover")
    );
    // No leftover copies next to the snapshot.
    assert!(!snap.join("kdnames.db").exists());

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&snap);
}

fn remnant_event(role: &str, name: &str, level: &str, region: &str, event_ms: u64) -> Vec<u8> {
    // Freelist context around the event: prefix/suffix noise like a real
    // telemetry copy, with fully synthetic values only.
    let mut bytes = b"\x00\x11s{\"#stale\":true}garbage".to_vec();
    bytes.extend_from_slice(
        format!(
            // `event_time_ms` is a string on the wire, like the real telemetry.
            "s{{\"#distinct_id\":\"test-device\",\"#event_name\":\"login_role\",\
       \"properties\":{{\"role_id\":\"{role}\",\"role_name\":\"{name}\",\
       \"role_level\":\"{level}\",\"server_name\":\"{region}\",\
       \"event_time_ms\":\"{event_ms}\"}}}}trailer"
        )
        .as_bytes(),
    );
    bytes
}

fn test_now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

#[test]
fn remnant_role_recovers_fresh_match() {
    let now = test_now_ms();
    let bytes = remnant_event("111111111", "TestRover", "64", "America", now - 3_600_000);
    let (role, stats) = find_remnant_role(&bytes, "111111111", now);
    let role = role.unwrap();
    assert_eq!(role.name, "TestRover");
    assert_eq!(role.level, 64);
    assert_eq!(role.region, "America");
    // Census lines up: one candidate, one login event, one fresh event.
    assert_eq!(stats.candidates, 1);
    assert_eq!(stats.role_hits, 1);
    assert_eq!(stats.fresh_hits, 1);
}

#[test]
fn remnant_role_rejects_stale_wrong_role_and_garbage() {
    let now = test_now_ms();
    // Stale: older than the freshness gate.
    let stale = remnant_event(
        "111111111",
        "TestRover",
        "64",
        "America",
        now - MAX_REMANT_AGE_MS - 1,
    );
    assert_eq!(find_remnant_role(&stale, "111111111", now).0, None);
    // Wrong role.
    let fresh = remnant_event("111111111", "TestRover", "64", "America", now - 1_000);
    assert_eq!(find_remnant_role(&fresh, "999999999", now).0, None);
    // Garbage bytes and truncated objects.
    assert_eq!(
        find_remnant_role(b"\x00\xffnot json", "111111111", now).0,
        None
    );
    assert_eq!(
        find_remnant_role(b"s{\"#distinct_id\":\"x\",\"role_id\"", "111111111", now).0,
        None
    );
}

#[test]
fn remnant_role_accepts_numeric_fields_and_prefers_newest() {
    let now = test_now_ms();
    let mut bytes = remnant_event("111111111", "OldName", "60", "Europe", now - 5_000);
    bytes.extend_from_slice(b"filler");
    bytes.extend_from_slice(
        format!(
            "s{{\"#distinct_id\":\"test-device\",\"#event_name\":\"login_role\",\
       \"properties\":{{\"role_id\":111111111,\"role_name\":\"NewName\",\
       \"role_level\":64,\"server_name\":\"America\",\
       \"event_time_ms\":\"{}\"}}}}end",
            now - 1_000
        )
        .as_bytes(),
    );
    let (role, stats) = find_remnant_role(&bytes, "111111111", now);
    let role = role.unwrap();
    assert_eq!(role.name, "NewName");
    assert_eq!(role.level, 64);
    assert_eq!(stats.candidates, 2);
    assert_eq!(stats.role_hits, 2);
    assert_eq!(stats.fresh_hits, 2);
}

#[test]
fn remnant_role_rejects_future_skew_and_bad_names() {
    let now = test_now_ms();
    // Beyond the clock-skew allowance.
    let future = remnant_event(
        "111111111",
        "TestRover",
        "64",
        "America",
        now + 60 * 60 * 1000,
    );
    assert_eq!(find_remnant_role(&future, "111111111", now).0, None);
    // Control characters in the name.
    let control = remnant_event("111111111", "Bad\nName", "64", "America", now - 1_000);
    assert_eq!(find_remnant_role(&control, "111111111", now).0, None);
    // Missing level.
    let mut bytes = b"s".to_vec();
    bytes.extend_from_slice(
        format!(
            "{{\"#distinct_id\":\"d\",\"properties\":{{\"role_id\":\"111111111\",\
       \"role_name\":\"TestRover\",\"server_name\":\"America\",\
       \"event_time_ms\":\"{}\"}}}}",
            now - 1_000
        )
        .as_bytes(),
    );
    assert_eq!(find_remnant_role(&bytes, "111111111", now).0, None);
}

#[test]
fn refresh_synthesizes_rich_data_from_remnants() {
    // Steam-like tree so kddata_path resolves; SdkLevelData WITHOUT the
    // live login — the exact production shape — plus remnant bytes (not
    // SQLite rows) carrying the fresh live identity.
    let root = unique_dir("remnant");
    let _ = fs::remove_dir_all(&root);
    let game = root.join("steamapps/common/Wuthering Waves");
    fs::create_dir_all(game.join("Client/Saved/LocalStorage")).unwrap();
    let storage = game.join("Client/Saved/LocalStorage");
    let conn = rusqlite::Connection::open(storage.join("LocalStorage.db")).unwrap();
    for sql in [
        VALID_SCHEMA,
        UID_ROW,
        "INSERT INTO LocalStorage(key, value) VALUES ('SdkLevelData', '{\"___MetaType___\": \"___Map___\", \"Content\": [[\"222222222\", [{\"Region\": \"Europe\", \"Level\": 60}]]]}')",
    ] {
        conn.execute_batch(sql).unwrap();
    }
    drop(conn);
    let kd_dir = root
        .join("steamapps/compatdata/3513350/pfx/drive_c/users/steamuser/AppData/Roaming/KR_G153");
    fs::create_dir_all(&kd_dir).unwrap();
    let event_ms = test_now_ms() - 60_000;
    fs::write(
        kd_dir.join("KDData-data.db"),
        remnant_event("111111111", "TestRover", "64", "America", event_ms),
    )
    .unwrap();

    let snap = unique_dir("remnant-snap");
    let mut reader = DbReader::new(storage.clone(), snap.clone(), None).unwrap();
    reader.set_game_dir(game.clone());
    assert_eq!(reader.refresh(), Refresh::Updated);
    let info = reader.last_info().unwrap();
    assert_eq!(info.uid, "111111111");
    assert_eq!(info.union_level, "64");
    assert_eq!(info.region, "America");
    assert_eq!(info.display_name.as_deref(), Some("TestRover"));
    // No leftover copies next to the snapshot.
    assert!(!snap.join("kdremnant.db").exists());

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&snap);
}

/// Pins the per-tick log lines byte for byte.
///
/// The builders under test hold the whole of each sentence a tick can
/// print, so an exact string here is that exact sentence in the log: any
/// edit to a message breaks this test on purpose, and the fix is to
/// decide the new text, not to loosen the assertion.
///
/// What this does not cover is when a line prints: level gating lives in
/// the log module and is pinned by its own tests, and the lines themselves
/// go to the process's stderr, which no test can read.
#[test]
fn tick_log_messages_keep_exact_text() {
    assert_eq!(
        accepted_message(true, false),
        "snapshot accepted; quests data: true, version data: false"
    );
    assert_eq!(
        accepted_message(false, true),
        "snapshot accepted; quests data: false, version data: true"
    );
    assert_eq!(
        missing_level_message(3, true),
        "level row missing for live login (expected 1 row, found 3 other row(s), \
         login seen elsewhere: true); trying telemetry remnants"
    );
    assert_eq!(
        scan_census_message(RemnantStats {
            candidates: 2,
            role_hits: 2,
            fresh_hits: 1,
        }),
        "telemetry scan: 2 candidate(s), 2 login event(s), 1 fresh event(s)"
    );
    assert_eq!(
        recovered_message(90, true),
        "role identity recovered from launcher telemetry (event age 90s): \
         complemented name+level+region; quests unavailable (no quest row for this login); \
         version attached: true"
    );
}

/// Pins the remnant scan's search contract: every `"role_id"` hit is
/// visited in order, and each hit resolves against the marker nearest
/// BEHIND it rather than the first one in the window.
///
/// Two events for the same login sit inside one 4 KiB marker window: a
/// stale one first, a fresh one second. The fresh hit's window therefore
/// holds both markers, and only the nearest-behind choice parses the
/// fresh event — a first-in-window search would re-read the stale one,
/// skip it, and recover nothing. The census pins the walk itself: two
/// candidates found, both for the login, one fresh.
#[test]
fn remnant_scan_resolves_each_hit_against_its_nearest_marker() {
    let now = test_now_ms();
    let stale = format!(
        "s{{\"#distinct_id\":\"d1\",\"properties\":{{\"role_id\":\"111111111\",\
         \"role_name\":\"Old\",\"role_level\":\"60\",\"server_name\":\"Europe\",\
         \"event_time_ms\":\"{}\"}}}}",
        now - MAX_REMANT_AGE_MS - 60_000
    );
    let fresh = format!(
        "s{{\"#distinct_id\":\"d2\",\"properties\":{{\"role_id\":\"111111111\",\
         \"role_name\":\"New\",\"role_level\":\"64\",\"server_name\":\"America\",\
         \"event_time_ms\":\"{}\"}}}}",
        now - 1_000
    );
    let mut bytes = b"prefix-noise".to_vec();
    bytes.extend_from_slice(stale.as_bytes());
    bytes.extend_from_slice(b"filler");
    bytes.extend_from_slice(fresh.as_bytes());
    bytes.extend_from_slice(b"trailer");

    let (role, stats) = find_remnant_role(&bytes, "111111111", now);
    let role = role.unwrap();
    assert_eq!(role.name, "New");
    assert_eq!(role.level, 64);
    assert_eq!(role.region, "America");
    assert_eq!(
        stats,
        RemnantStats {
            candidates: 2,
            role_hits: 2,
            fresh_hits: 1,
        }
    );
}
