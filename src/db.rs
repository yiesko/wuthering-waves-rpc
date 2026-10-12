// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! LocalStorage reader with copy-to-RAM semantics.
//!
//! Safety model: the live game files
//! are NEVER opened with SQLite, never written to, never chmodded and
//! never kept open. Each tick we copy `LocalStorage.db` (+ journal) into a
//! tmpfs snapshot dir through [`bounded_copy`], which refuses a source past
//! [`MAX_DB_BYTES`] (past [`MAX_JSON_BYTES`] for the launcher cache), caps
//! the bytes it moves and proves the source did not change mid-copy. Only
//! the private copy is queried, and it is deleted right after. From the
//! game's perspective this is indistinguishable from a backup tool: no
//! SQLite locks are ever taken on the live files.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::log;
use crate::runtime_dir::{DestMode, bounded_copy};

const MAIN_DB: &str = "LocalStorage.db";
/// Steam AppId, locating the Proton prefix (`compatdata/<id>/...`) for
/// Steam installs. Public, stable, documented in the unit file.
const STEAM_APP_ID: &str = "3513350";
/// Rolled-back journal today; `-wal`/`-shm` if a future update switches
/// SQLite modes. Copied alongside so the private snapshot always recovers.
const SIDECAR_SUFFIXES: [&str; 3] = ["-journal", "-wal", "-shm"];
/// Byte ceiling for every game database wwrpc snapshots: a healthy
/// `LocalStorage`, its sidecars and the device and telemetry databases are
/// kilobytes, so anything past this is corruption or a wrong directory.
/// Refused before the copy starts, not truncated into a corrupt snapshot.
pub const MAX_DB_BYTES: u64 = 64 * 1024 * 1024;

/// Byte ceiling for the launcher account cache: a JSON file describing a
/// handful of accounts is a few kilobytes, so a megabyte is already
/// generous.
pub const MAX_JSON_BYTES: u64 = 1024 * 1024;

/// Identity + stats of one account, as shown in presence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerInfo {
    pub uid: String,
    pub union_level: String,
    pub region: String,
    /// Finished-quest entries (`UserFinishedQuests`), if readable.
    pub quests_done: Option<usize>,
    /// Game version (`PatchVersion` from the device database), if readable.
    pub game_version: Option<String>,
    /// Live display name (Rover name from launcher telemetry), if a live row
    /// matches the current login. Loses to explicit `--player-name`.
    pub display_name: Option<String>,
}

/// Outcome of [`DbReader::refresh`]; the freshest data is always available
/// via [`DbReader::last_info`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// Snapshot unchanged since the last tick; `last_info` still holds.
    Unchanged,
    /// New data read and fully triaged; see `last_info`.
    Updated,
    /// Nothing usable right now (missing file, torn copy, failed triage...);
    /// `last_info` keeps the previous value, if any.
    Unavailable,
}

/// Why a snapshot was rejected. Messages name keys, never values: uids,
/// levels and regions must not leak into logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriageError {
    /// Snapshot file cannot be opened as SQLite at all.
    Unopenable,
    /// `LocalStorage` table missing.
    MissingTable,
    /// Table exists but lacks `key`/`value` text columns.
    BadSchema,
    /// Copy is corrupt (`PRAGMA quick_check` failed).
    Corrupt,
    /// Required key absent.
    MissingKey(&'static str),
    /// Required key present but not valid JSON / wrong shape.
    Malformed(&'static str),
    /// No usable uid (no override and no parseable `RecentlyLoginUID`).
    UnknownUid,
    /// The resolved uid has no entry in `SdkLevelData`. Carries only counts,
    /// never uids or values. `uid_seen_elsewhere` tells a known account
    /// whose level row is simply missing yet apart from a fully unknown one.
    AccountNotInLevelData {
        known_accounts: usize,
        uid_seen_elsewhere: bool,
    },
}

impl std::error::Error for TriageError {}

impl std::fmt::Display for TriageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unopenable => write!(f, "snapshot is not a readable SQLite database"),
            Self::MissingTable => write!(f, "LocalStorage table missing"),
            Self::BadSchema => write!(f, "LocalStorage table has unexpected columns"),
            Self::Corrupt => write!(f, "snapshot failed integrity check"),
            Self::MissingKey(key) => write!(f, "required key `{key}` missing"),
            Self::Malformed(key) => write!(f, "key `{key}` has unexpected shape"),
            Self::UnknownUid => write!(f, "no usable account uid found"),
            Self::AccountNotInLevelData {
                known_accounts,
                uid_seen_elsewhere: true,
            } => write!(
                f,
                "account has no level data yet ({known_accounts} other account(s) stored)"
            ),
            Self::AccountNotInLevelData { known_accounts, .. } => write!(
                f,
                "logged-in account has no level data ({known_accounts} account(s) stored)"
            ),
        }
    }
}

/// Polling reader over the game's `LocalStorage`: copy-to-RAM snapshot,
/// ordered triage, and best-effort enrichment (display name, version).
/// One tick is [`DbReader::refresh`]; the freshest accepted data is
/// [`DbReader::last_info`]. Never fails hard: every problem degrades to
/// `Unavailable`/`Unchanged` and is logged once per distinct reason.
#[derive(Debug)]
pub struct DbReader {
    storage_dir: PathBuf,
    src_db: PathBuf,
    snap_db: PathBuf,
    /// `(source, snapshot)` sidecar pairs, precomputed in `new`.
    sidecars: [(PathBuf, PathBuf); 3],
    uid_override: Option<String>,
    /// Explicit 1-based account position (`--account-index`), winning over
    /// uid resolution. Validated softly at startup; per-tick misses reject
    /// like an unknown uid.
    account_index: Option<usize>,
    /// Game dir, for prefix-derived lookups (launcher cache, KDData).
    /// Optional: everything depending on it is best-effort.
    game_dir: Option<PathBuf>,
    /// Live login uid (`RecentlyLoginUID`) as of the latest re-read,
    /// independent of triage outcome. Drives session identity: DATA follows
    /// pinning/validation, but IDENTITY always follows this signal.
    live_uid: Option<String>,
    /// Display name keyed by the role uid it was resolved for, plus the
    /// telemetry mtime it was resolved at. Survives telemetry gaps; dies
    /// on role switch.
    cached_display: Option<(String, String)>,
    kd_mtime: Option<SystemTime>,
    /// Game version from the last successful device read, independent of
    /// triage outcome: every enrichment complements the others, so a
    /// failed level triage must not take the version down with it.
    cached_version: Option<String>,
    last_mtime: Option<SystemTime>,
    last: Option<PlayerInfo>,
    degraded: bool,
    /// Last reported triage failure, to log each distinct reason once
    /// instead of spamming every tick while it persists.
    last_reported: Option<TriageError>,
}

impl DbReader {
    /// Open a reader: `storage_dir` holds the live `LocalStorage.db`,
    /// `snapshot_dir` (tmpfs) holds the private per-tick copies, and
    /// `uid_override` (`--kuro-uid`) wins over auto-detection when set.
    /// Creates the snapshot dir; reads nothing yet (see `refresh`).
    pub fn new(
        storage_dir: PathBuf,
        snapshot_dir: PathBuf,
        uid_override: Option<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        fs::create_dir_all(&snapshot_dir).map_err(|err| {
            format!(
                "cannot create snapshot dir '{}': {err}",
                snapshot_dir.display()
            )
        })?;
        let sidecars = SIDECAR_SUFFIXES.map(|suffix| {
            (
                storage_dir.join(format!("{MAIN_DB}{suffix}")),
                snapshot_dir.join(format!("snapshot.db{suffix}")),
            )
        });
        Ok(Self {
            src_db: storage_dir.join(MAIN_DB),
            snap_db: snapshot_dir.join("snapshot.db"),
            sidecars,
            storage_dir,
            uid_override,
            account_index: None,
            game_dir: None,
            live_uid: None,
            cached_display: None,
            kd_mtime: None,
            cached_version: None,
            last_mtime: None,
            last: None,
            degraded: false,
            last_reported: None,
        })
    }

    /// Pin presence to one stored account by 1-based position (see
    /// `--list-accounts`). Wins over uid resolution, including overrides.
    pub fn set_account_index(&mut self, index: usize) {
        self.account_index = Some(index);
    }

    /// Game dir for prefix-derived lookups (launcher nicknames, KDData
    /// display names). Best-effort throughout: absence only disables them.
    pub fn set_game_dir(&mut self, dir: PathBuf) {
        self.game_dir = Some(dir);
    }

    /// `true` once the numbered-DB sentinel tripped: the reader stops
    /// touching game files for the rest of the session (fail-safe).
    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    /// Freshest accepted snapshot, if any tick has produced one. Drives the
    /// rich tier; `None` means silence/static (see `presence`).
    pub fn last_info(&self) -> Option<&PlayerInfo> {
        self.last.as_ref()
    }

    /// Live login uid as of the latest re-read, if readable. Session
    /// identity signal: values never leave the reader except through here
    /// (never logged).
    pub fn live_uid(&self) -> Option<&str> {
        self.live_uid.as_deref()
    }

    /// Display name for the live login, if currently resolved (telemetry
    /// hit for exactly this login). Values never logged.
    pub fn display_name(&self) -> Option<&str> {
        let live = self.live_uid.as_deref()?;
        self.cached_display
            .as_ref()
            .and_then(|(uid, name)| (uid == live).then_some(name.as_str()))
    }

    /// Game version as of the latest successful device read, if any.
    pub fn cached_version(&self) -> Option<&str> {
        self.cached_version.as_deref()
    }

    /// Where this reader looks for `LocalStorage.db`.
    pub fn storage_dir(&self) -> &Path {
        &self.storage_dir
    }

    /// Last triage failure, if the latest snapshot was rejected. Test-only
    /// observability for the log-dedup state machine (`src/tests/db.rs`).
    #[cfg(test)]
    pub(crate) fn last_triage(&self) -> Option<TriageError> {
        self.last_reported
    }

    /// One poll tick. Never fails hard: every problem degrades to
    /// [`Refresh::Unavailable`] (and the caller falls back to static text),
    /// except the sentinel, which degrades the reader permanently.
    pub fn refresh(&mut self) -> Refresh {
        if self.degraded {
            return if self.last.is_some() {
                Refresh::Unchanged
            } else {
                Refresh::Unavailable
            };
        }

        if has_numbered_db(&self.storage_dir) {
            self.degraded = true;
            log::warn(format!(
                "SAFETY: unexpected numbered database (LocalStorage2.db, ...) \
         appeared in '{}'. Stopping all game-file access for this session; \
         continuing with static presence.",
                self.storage_dir.display()
            ));
            return if self.last.is_some() {
                Refresh::Unchanged
            } else {
                Refresh::Unavailable
            };
        }

        // Display-name tracking runs on every tick, on telemetry's own
        // cadence — independent of the main snapshot cadence below.
        self.refresh_display_name();

        let meta = match fs::metadata(&self.src_db) {
            Ok(meta) if meta.len() <= MAX_DB_BYTES => meta,
            Ok(_) => {
                log::warn("LocalStorage.db exceeds sanity size; ignoring");
                return self.cached_or_unavailable();
            }
            Err(_) => return self.cached_or_unavailable(),
        };
        let mtime = match meta.modified() {
            Ok(mtime) => mtime,
            Err(_) => return self.cached_or_unavailable(),
        };

        if self.last_mtime == Some(mtime) && self.last.is_some() {
            log::debug("snapshot unchanged since last tick; reusing data");
            return Refresh::Unchanged;
        }

        // The same destination is re-copied every tick, so whatever a killed
        // run left there is replaced rather than refused; the ceiling is
        // re-checked on the open source, never taken from the stat above.
        let copy = bounded_copy(
            &self.src_db,
            &self.snap_db,
            MAX_DB_BYTES,
            DestMode::Overwrite,
        );
        if copy.is_err() {
            log::debug("snapshot copy failed; keeping previous data");
            return self.cached_or_unavailable();
        }
        for (src, snap) in &self.sidecars {
            // Best effort: without sidecars a torn copy cannot recover. A
            // sidecar that is missing, oversized or rewritten mid-copy is
            // skipped, and the snapshot it would have filled stays absent.
            let _ = bounded_copy(src, snap, MAX_DB_BYTES, DestMode::Overwrite);
        }

        // Discard torn copies: the game must not have touched the source while
        // we were reading it. The copy itself already rejects a source that
        // changed underneath it; this check still covers the sidecars copied
        // after it.
        let unchanged = fs::metadata(&self.src_db)
            .and_then(|m| m.modified())
            .map(|after| after == mtime)
            .unwrap_or(false);
        if !unchanged {
            log::debug("source changed mid-copy; discarding torn snapshot");
            Self::remove_quiet(&self.snap_db);
            for (_, snap) in &self.sidecars {
                Self::remove_quiet(snap);
            }
            return self.cached_or_unavailable();
        }

        // Live identity, independent of triage: a role switch with unknown
        // new data must not keep showing the previous account, so stale state
        // (level and display alike) is dropped the moment the login moves on.
        // Absent/unreadable markers change nothing: unknown never counts.
        let tick_live = Self::peek_live_uid(&self.snap_db);
        if let (Some(previous), Some(current)) = (self.live_uid.as_deref(), tick_live.as_deref())
            && previous != current
        {
            log::info("login uid changed; dropping previous session data");
            self.last = None;
            self.cached_display = None;
        }
        if tick_live.is_some() {
            self.live_uid = tick_live;
        }

        let outcome = query_snapshot(
            &self.snap_db,
            self.uid_override.as_deref(),
            self.account_index,
        );
        // Device version rides along every re-read, whatever triage says:
        // enrichment sources complement each other instead of sharing fate.
        if let Some(version) = read_device_version(&self.storage_dir, &self.snap_db) {
            self.cached_version = Some(version);
        }
        Self::remove_quiet(&self.snap_db);
        for (_, snap) in &self.sidecars {
            Self::remove_quiet(snap);
        }

        match outcome {
            Ok((mut info, _)) => {
                info.game_version = self.cached_version.clone();
                self.last_mtime = Some(mtime);
                self.last = Some(info);
                self.last_reported = None;
                // Resolve display names against the now-fresh login too: the
                // top-of-tick phase ran before this tick was known.
                self.refresh_display_name();
                log::debug(format!(
                    "snapshot accepted; quests data: {}, version data: {}",
                    self.last
                        .as_ref()
                        .is_some_and(|info| info.quests_done.is_some()),
                    self.last
                        .as_ref()
                        .is_some_and(|info| info.game_version.is_some())
                ));
                Refresh::Updated
            }
            Err(err) => {
                // Automatic recovery (automatic mode only — never against an
                // explicit pin): when the live login has no level row yet, its
                // identity usually survives in launcher-telemetry remnants, dated
                // and role-matched (see `find_remnant_role`). A recovered identity
                // synthesizes the same rich data triage would have built.
                if matches!(err, TriageError::AccountNotInLevelData { .. })
                    && self.uid_override.is_none()
                    && self.account_index.is_none()
                    && let Some(live) = self.live_uid.clone()
                {
                    // Expected vs received, shapes only: what triage wanted, what
                    // the database held, and what the telemetry scan found.
                    if let TriageError::AccountNotInLevelData {
                        known_accounts,
                        uid_seen_elsewhere,
                    } = err
                    {
                        log::debug(format!(
                            "level row missing for live login (expected 1 row, found {known_accounts} other row(s), \
               login seen elsewhere: {uid_seen_elsewhere}); trying telemetry remnants"
                        ));
                    }
                    let (recovered, stats) = self.remnant_role(&live);
                    log::debug(format!(
                        "telemetry scan: {} candidate(s), {} login event(s), {} fresh event(s)",
                        stats.candidates, stats.role_hits, stats.fresh_hits
                    ));
                    if let Some(role) = recovered {
                        let age_secs = SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .and_then(|age| u64::try_from(age.as_millis()).ok())
                            .map(|now_ms| now_ms.saturating_sub(role.event_ms) / 1000)
                            .unwrap_or(0);
                        log::info(format!(
                            "role identity recovered from launcher telemetry (event age {age_secs}s): \
               complemented name+level+region; quests unavailable (no quest row for this login); \
               version attached: {}",
                            self.cached_version.is_some()
                        ));
                        self.last_mtime = Some(mtime);
                        self.last_reported = None;
                        self.last = Some(PlayerInfo {
                            uid: live,
                            union_level: role.level.to_string(),
                            region: role.region,
                            quests_done: None,
                            game_version: self.cached_version.clone(),
                            display_name: Some(role.name),
                        });
                        return Refresh::Updated;
                    }
                    log::debug(
                        "telemetry has no fresh event for the live login; still missing: name+level+region",
                    );
                }
                // One line per distinct reason; a persistent condition (game
                // mid-write, unknown account, ...) must not spam every tick.
                if self.last_reported != Some(err) {
                    self.last_reported = Some(err);
                    log::warn(format!(
                        "LocalStorage triage failed ({err}); {}",
                        self.hint(err)
                    ));
                }
                self.cached_or_unavailable()
            }
        }
    }

    /// Actionable hint for a triage failure. Static strings only: never
    /// includes account data.
    pub(crate) fn hint(&self, err: TriageError) -> &'static str {
        match err {
            TriageError::AccountNotInLevelData {
                uid_seen_elsewhere: true,
                ..
            } => {
                "account is known but its level data is missing; pin it with --list-accounts, --account-index N"
            }
            TriageError::AccountNotInLevelData { .. } if self.uid_override.is_some() => {
                "check --kuro-uid: stale or unknown account"
            }
            TriageError::AccountNotInLevelData { .. } => {
                "run --list-accounts to recognize yours by level, then pin it with --account-index N"
            }
            TriageError::UnknownUid => "no account detected; launch the game and log in",
            _ => "keeping previous data",
        }
    }

    /// Display-name tracking, independent of the main snapshot cadence.
    /// Telemetry churns on its own schedule, so this runs every tick, gated
    /// only on its own mtime: re-resolve when it changed (or never resolved),
    /// keep the sticky value otherwise. Role switches (see [`DbReader::refresh`])
    /// already dropped the cache; a re-resolution here always targets the
    /// current login. Attaches to `last_info` when it lacks a name — explicit
    /// `--player-name` still wins downstream, and the triage-built data is
    /// never touched. Values never logged.
    fn refresh_display_name(&mut self) {
        let Some(live_uid) = self.live_uid.clone() else {
            log::debug("display tracking idle: no live login marker");
            return;
        };
        if self
            .cached_display
            .as_ref()
            .is_some_and(|(uid, _)| uid != &live_uid)
        {
            log::debug("display cache dropped on role switch");
            self.cached_display = None;
        }
        let kd_mtime = self
            .game_dir
            .as_ref()
            .and_then(|dir| kddata_path(dir))
            .and_then(|path| fs::metadata(&path).and_then(|meta| meta.modified()).ok());
        if self.cached_display.is_some() && kd_mtime == self.kd_mtime {
            log::debug("telemetry unchanged; keeping display cache");
            return;
        }
        log::debug("telemetry changed; re-resolving display name");
        self.kd_mtime = kd_mtime;
        if let Some(name) = self.live_role_name(&live_uid) {
            log::debug("display name resolved");
            self.cached_display = Some((live_uid, name));
        } else {
            log::debug("no live display name in telemetry rows; still missing: name");
        }
        // Same liveness rule as display_name(): never attribute a name
        // without a matching live login (computed first: separate borrow).
        let display = self.display_name().map(str::to_string);
        if let Some(info) = self.last.as_mut()
            && info.display_name.is_none()
        {
            info.display_name = display;
        }
    }

    /// Best-effort current login uid from a PRIVATE snapshot copy.
    /// Independent of triage: succeeds exactly when `RecentlyLoginUID`
    /// parses, whatever else is broken. Values never leave the reader
    /// except through [`DbReader::live_uid`] (never logged).
    fn peek_live_uid(snap: &Path) -> Option<String> {
        let conn = rusqlite::Connection::open(snap).ok()?;
        let body = get_value(&conn, "RecentlyLoginUID").ok()?;
        json_scalar(&body)
    }

    fn cached_or_unavailable(&self) -> Refresh {
        if self.last.is_some() {
            Refresh::Unchanged
        } else {
            Refresh::Unavailable
        }
    }

    fn remove_quiet(path: &Path) {
        let _ = fs::remove_file(path);
    }

    /// Live display name for one login uid: exact `role_id` match over a RAM
    /// copy of the launcher telemetry database. Best-effort by construction:
    /// rows are ephemeral (written at login, deleted after upload), so
    /// absence is normal and never an error. Follows the same copy-to-RAM
    /// rules as every other game file read, cap included.
    fn live_role_name(&self, role_uid: &str) -> Option<String> {
        let kd_src = kddata_path(self.game_dir.as_ref()?)?;
        let snap_dir = self.snap_db.parent()?;
        let snap_copy = snap_dir.join("kdnames.db");
        // Read by this one lookup and removed right after, so it must start
        // from nothing rather than inherit whatever is on the path.
        bounded_copy(&kd_src, &snap_copy, MAX_DB_BYTES, DestMode::Fresh).ok()?;
        let name = query_kd_names(&snap_copy, role_uid);
        Self::remove_quiet(&snap_copy);
        name
    }

    /// Identity recovery for one login uid from a PRIVATE RAM copy of the
    /// launcher telemetry database, live rows AND deleted-row remnants
    /// (see [`find_remnant_role`]). Same copy-to-RAM rules and size cap as
    /// [`DbReader::live_role_name`]; best-effort, values never logged.
    fn remnant_role(&self, role_uid: &str) -> (Option<RemnantRole>, RemnantStats) {
        let none = (None, RemnantStats::default());
        let Some(game_dir) = self.game_dir.as_deref() else {
            return none;
        };
        let Some(kd_src) = kddata_path(game_dir) else {
            log::debug("telemetry scan skipped: no telemetry database located");
            return none;
        };
        let Some(snap_dir) = self.snap_db.parent() else {
            return none;
        };
        let snap_copy = snap_dir.join("kdremnant.db");
        // Read by this one scan and removed right after, so it must start from
        // nothing rather than inherit whatever is on the path. A copy refused
        // here (too large, rewritten mid-copy) reads its reason from the
        // error, the way the size check used to report it.
        if let Err(err) = bounded_copy(&kd_src, &snap_copy, MAX_DB_BYTES, DestMode::Fresh) {
            log::debug(format!("telemetry scan skipped: copy failed ({err})"));
            return none;
        }
        let bytes = fs::read(&snap_copy).ok();
        Self::remove_quiet(&snap_copy);
        let now_ms = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|age| u64::try_from(age.as_millis()).ok());
        let (Some(now_ms), Some(bytes)) = (now_ms, bytes) else {
            return none;
        };
        find_remnant_role(&bytes, role_uid, now_ms)
    }

    /// One-shot account listing for `--list-accounts`: copies the database
    /// to a temp dir, triages it like a tick would, and returns accounts in
    /// file order with login nicknames attached when locatable. Uids never
    /// leave this function, so the listing is safe to print.
    pub fn list_accounts(
        storage_dir: &Path,
        game_dir: &Path,
    ) -> Result<Vec<StoredAccount>, TriageError> {
        // Unique per call: tests (and paranoid operators) run these concurrently.
        static LIST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tag = LIST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let snap_dir =
            std::env::temp_dir().join(format!("wwrpc-list-{}-{tag}", std::process::id()));
        fs::create_dir_all(&snap_dir).map_err(|_| TriageError::Unopenable)?;
        let snap_db = snap_dir.join("snapshot.db");
        let outcome = Self::query_list(&snap_db, storage_dir, game_dir);
        let _ = fs::remove_dir_all(&snap_dir);
        outcome
    }

    fn query_list(
        snap_db: &Path,
        storage_dir: &Path,
        game_dir: &Path,
    ) -> Result<Vec<StoredAccount>, TriageError> {
        // A listing copies into a directory created for this call alone, so
        // every snapshot starts from nothing: an entry already on the path
        // is refused, not adopted.
        bounded_copy(
            &storage_dir.join(MAIN_DB),
            snap_db,
            MAX_DB_BYTES,
            DestMode::Fresh,
        )
        .map_err(|_| TriageError::Unopenable)?;
        if let Some(snap_dir) = snap_db.parent() {
            for suffix in SIDECAR_SUFFIXES {
                // Best effort, exactly as on the per-tick path: recovery
                // data is a bonus, never a reason to fail a listing.
                let _ = bounded_copy(
                    &storage_dir.join(format!("{MAIN_DB}{suffix}")),
                    &snap_dir.join(format!("snapshot.db{suffix}")),
                    MAX_DB_BYTES,
                    DestMode::Fresh,
                );
            }
        }
        let conn = rusqlite::Connection::open(snap_db).map_err(|_| TriageError::Unopenable)?;
        check_schema(&conn)?;
        check_integrity(&conn)?;
        let body = get_value(&conn, "SdkLevelData")?;
        let accounts = parse_sdk_accounts(&body).ok_or(TriageError::Malformed("SdkLevelData"))?;
        // Launcher files follow the same copy-to-RAM rule: snapshot them into
        // our temp dir and read only the copies, never the originals.
        let (nicknames, last_login) = match launcher_cache_path(game_dir) {
            Some(cache_src) if snap_db.parent().is_some() => {
                let snap_dir = snap_db.parent().expect("snapshot has a parent dir");
                let snap_cache = snap_dir.join("launcher-cache.json");
                // A JSON cache gets its own, much smaller ceiling: the real
                // file holds a handful of accounts in a few kilobytes.
                if bounded_copy(&cache_src, &snap_cache, MAX_JSON_BYTES, DestMode::Fresh).is_err() {
                    (HashMap::new(), None)
                } else {
                    let nicknames = read_nicknames(&snap_cache);
                    let last_login = read_last_login(&snap_cache);
                    let _ = fs::remove_file(&snap_cache);
                    (nicknames, last_login)
                }
            }
            _ => (HashMap::new(), None),
        };
        Ok(accounts
            .into_iter()
            .enumerate()
            .map(|(position, account)| StoredAccount {
                index: position + 1,
                nickname: nicknames.get(&account.uid).cloned(),
                last_login: last_login.as_deref() == Some(account.uid.as_str()),
                region: account.region,
                level: account.level,
            })
            .collect())
    }
}

/// The launcher's `last_login_cuid`, if parseable. Corroboration marker
/// ONLY (see below): this file is first-login-sticky and can never
/// identify the live session, so the value itself is never displayed,
/// logged, or matched for presence — only compared for equality.
pub(crate) fn read_last_login(cache_path: &Path) -> Option<String> {
    let body = fs::read_to_string(cache_path).ok()?;
    let cache = serde_json::from_str::<Value>(&body).ok()?;
    match cache.get("last_login_cuid")?.as_str()? {
        cuid if !cuid.is_empty() => Some(cuid.to_string()),
        _ => None,
    }
}
/// Display name for one login uid from a PRIVATE telemetry copy: exact
/// `role_id` match over live `KDData` rows. First match wins; names are
/// sanity-checked (1–32 chars, no control characters) because they come
/// from an external writer. Anything off — missing table, bad rows,
/// absent uid — yields `None`, never an error.
pub(crate) fn query_kd_names(snap: &Path, role_uid: &str) -> Option<String> {
    const MAX_ROWS: i64 = 500;
    const MAX_NAME_CHARS: usize = 32;
    let conn = rusqlite::Connection::open(snap).ok()?;
    let mut stmt = conn.prepare("SELECT content FROM KDData LIMIT ?1").ok()?;
    let rows = stmt
        .query_map([MAX_ROWS], |row| row.get::<_, String>(0))
        .ok()?;
    for cell in rows.flatten() {
        let Ok(value) = serde_json::from_str::<Value>(&cell) else {
            continue;
        };
        let Some(id) = (match value.get("role_id") {
            Some(Value::String(id)) => Some(id.clone()),
            Some(Value::Number(id)) => Some(id.to_string()),
            _ => None,
        }) else {
            continue;
        };
        if id != role_uid {
            continue;
        }
        let Some(name) = value.get("role_name").and_then(Value::as_str) else {
            continue;
        };
        let name = name.trim().to_string();
        if !name.is_empty()
            && name.chars().count() <= MAX_NAME_CHARS
            && !name.chars().any(char::is_control)
        {
            return Some(name);
        }
    }
    None
}

/// A login-role identity recovered from launcher-telemetry remnants:
/// name, level and server for one role uid, dated by the event itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemnantRole {
    pub name: String,
    pub level: u32,
    pub region: String,
    pub event_ms: u64,
}

/// Scan census for one [`find_remnant_role`] pass: shapes only, never
/// values — safe to log. Lets operators see what the telemetry held
/// (candidates scanned, events for the live login, fresh ones) without
/// ever printing account data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemnantStats {
    /// `"role_id"` occurrences inspected (bounded by scan cap).
    pub candidates: usize,
    /// Events matching the live login uid.
    pub role_hits: usize,
    /// Of those, events fresh enough to describe this session.
    pub fresh_hits: usize,
}
/// Freshness gate for telemetry remnants: only events from the last day
/// can describe the live session (plus a small clock-skew allowance).
pub(crate) const MAX_REMANT_AGE_MS: u64 = 24 * 60 * 60 * 1000;
const REMNANT_SKEW_MS: u64 = 5 * 60 * 1000;
const MAX_REMANT_CANDIDATES: usize = 64;

/// Recover `(name, level, server)` for one login uid from a PRIVATE RAM
/// copy of the launcher telemetry database — including deleted-row
/// remnants. The launcher uploads telemetry rows and deletes them, so
/// live queries usually find nothing; the bytes remain until vacuumed,
/// and every event is self-dating (`event_time_ms`) and self-identifying
/// (`role_id`), so only events that match the live uid AND are fresh
/// count. Newest matching event wins, alongside its scan [`RemnantStats`].
/// Pure over bytes: any failure (or no fresh match) yields `None`, never
/// an error; values never logged.
pub(crate) fn find_remnant_role(
    copy_bytes: &[u8],
    live_uid: &str,
    now_ms: u64,
) -> (Option<RemnantRole>, RemnantStats) {
    const MARKER: &[u8] = b"{\"#distinct_id\"";
    let mut best: Option<RemnantRole> = None;
    let mut stats = RemnantStats::default();
    let mut cursor = 0;
    while stats.candidates < MAX_REMANT_CANDIDATES {
        let Some(relative) = find_subslice(&copy_bytes[cursor..], b"\"role_id\"") else {
            break;
        };
        stats.candidates += 1;
        let hit = cursor + relative;
        cursor = hit + 1;
        // Enclosing event object starts at the nearest marker behind the hit.
        let window_start = hit.saturating_sub(4096);
        let Some(marker) = rfind_subslice(&copy_bytes[window_start..hit], MARKER)
            .map(|relative| window_start + relative)
        else {
            continue;
        };
        let Some(end) = balanced_end(&copy_bytes[marker..], 8192) else {
            continue;
        };
        let Ok(event) = serde_json::from_slice::<Value>(&copy_bytes[marker..marker + end]) else {
            continue;
        };
        let properties = event.get("properties");
        let role_matches = properties
            .and_then(|props| props.get("role_id"))
            .is_some_and(|id| match id {
                Value::String(id) => id == live_uid,
                Value::Number(id) => id.to_string() == live_uid,
                _ => false,
            });
        if !role_matches {
            continue;
        }
        stats.role_hits += 1;
        let event_ms = properties
            .and_then(|props| match props.get("event_time_ms")? {
                Value::Number(ms) => ms.as_u64(),
                Value::String(ms) => ms.trim().parse::<u64>().ok(),
                _ => None,
            })
            .unwrap_or(0);
        if event_ms == 0 || event_ms > now_ms.saturating_add(REMNANT_SKEW_MS) {
            continue;
        }
        if now_ms.saturating_sub(event_ms) > MAX_REMANT_AGE_MS {
            continue;
        }
        stats.fresh_hits += 1;
        let (Some(name), Some(level), Some(region)) = (
            properties
                .and_then(|props| props.get("role_name")?.as_str())
                .map(str::trim)
                .filter(|name| {
                    !name.is_empty()
                        && name.chars().count() <= 32
                        && !name.chars().any(char::is_control)
                })
                .map(str::to_string),
            properties
                .and_then(|props| match props.get("role_level")? {
                    Value::String(level) => level.trim().parse::<u32>().ok(),
                    Value::Number(level) => {
                        level.as_u64().and_then(|level| u32::try_from(level).ok())
                    }
                    _ => None,
                })
                .filter(|level| *level > 0),
            properties
                .and_then(|props| props.get("server_name")?.as_str())
                .map(str::trim)
                .filter(|region| !region.is_empty() && region.len() <= 64)
                .map(str::to_string),
        ) else {
            continue;
        };
        if best
            .as_ref()
            .is_some_and(|known| known.event_ms >= event_ms)
        {
            continue;
        }
        best = Some(RemnantRole {
            name,
            level,
            region,
            event_ms,
        });
    }
    (best, stats)
}

/// Byte-subslice search (avoids UTF-8 assumptions over foreign bytes).
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Last occurrence: the marker nearest BEHIND a hit, not the first in the
/// window (a window can span several adjacent events).
fn rfind_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// End offset (exclusive) of the `{...}` object starting at `bytes[0]`,
/// honoring strings and escapes; `None` past `limit` or on imbalance.
fn balanced_end(bytes: &[u8], limit: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().take(limit) {
        if index == 0 && *byte != b'{' {
            return None;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// One stored account for display. Carries no uid: `--list-accounts`
/// output must stay safe to paste anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredAccount {
    /// 1-based position, stable for `--account-index` within one database.
    pub index: usize,
    pub region: String,
    pub level: String,
    /// Login nickname from the launcher cache, when locatable. Recognition
    /// aid only: may be outdated, never used for matching or display.
    pub nickname: Option<String>,
    /// Whether this row matches the launcher's last-login marker. A
    /// corroboration hint for humans, NOT session truth (the marker only
    /// refreshes on logout/login cycles).
    pub last_login: bool,
}

/// Nearest `steamapps` ancestor of any path beneath a Steam library.
/// Shared root for prefix-derived lookups (launcher cache, KDData).
fn steamapps_dir(anywhere: &Path) -> Option<PathBuf> {
    anywhere
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "steamapps"))
        .map(Path::to_path_buf)
}

/// Locate the launcher account cache for Steam installs:
/// `<steamapps>/compatdata/<steam-app>/pfx/.../KRSDKUserCache.json`.
/// `None` anywhere else (Twintail layouts vary): nicknames are best-effort.
pub(crate) fn launcher_cache_path(game_dir: &Path) -> Option<PathBuf> {
    let cache = steamapps_dir(game_dir)?.join(format!(
    "compatdata/{STEAM_APP_ID}/pfx/drive_c/users/steamuser/AppData/Roaming/KR_G153/A1834/KRSDKUserCache.json"
  ));
    cache.is_file().then_some(cache)
}

/// Locate the launcher telemetry database for Steam installs:
/// `<steamapps>/compatdata/<steam-app>/pfx/.../KDData-data.db`.
/// `None` anywhere else. Live rows are queried first; deleted-row remnants
/// are ALSO usable — every telemetry event carries its own `#time` /
/// `event_time_ms` plus `role_id`, so a remnant is datable and correlatable
/// (see [`find_remnant_role`]), never trusted blindly.
pub(crate) fn kddata_path(anywhere: &Path) -> Option<PathBuf> {
    let db = steamapps_dir(anywhere)?.join(format!(
    "compatdata/{STEAM_APP_ID}/pfx/drive_c/users/steamuser/AppData/Roaming/KR_G153/KDData-data.db"
  ));
    db.is_file().then_some(db)
}

/// `cuid -> thirdNickName` for accounts that have one. Parses ONLY those
/// two string fields: mails, tokens and codes are never read, let alone
/// stored. Any failure yields an empty map.
///
/// Note: the cache's `last_login_cuid` is deliberately ignored — it only
/// refreshes on logout/login cycles (first-login-sticky), so it can never
/// identify the live session.
pub(crate) fn read_nicknames(cache_path: &Path) -> HashMap<String, String> {
    let Ok(body) = fs::read_to_string(cache_path) else {
        return HashMap::new();
    };
    let Ok(cache) = serde_json::from_str::<Value>(&body) else {
        return HashMap::new();
    };
    let Some(accounts) = cache.get("account_list").and_then(|list| list.as_array()) else {
        return HashMap::new();
    };
    let mut nicknames = HashMap::new();
    for account in accounts {
        if let (Some(cuid), Some(nickname)) = (
            account.get("cuid").and_then(Value::as_str),
            account.get("thirdNickName").and_then(Value::as_str),
        ) && !cuid.is_empty()
            && !nickname.is_empty()
        {
            nicknames.insert(cuid.to_string(), nickname.to_string());
        }
    }
    nicknames
}

/// Game version from the device database (`DeviceSaved/DeviceStorage.db`,
/// key `PatchVersion`). Same copy-to-RAM rules as the main database, byte
/// ceiling included; best-effort: any failure yields `None` without
/// affecting triage.
fn read_device_version(storage_dir: &Path, snap_db: &Path) -> Option<String> {
    let device_src = storage_dir.parent()?.join("DeviceSaved/DeviceStorage.db");
    if !device_src.is_file() {
        return None;
    }
    let snap_copy = snap_db.parent()?.join("device.db");
    // Nothing else bounds this file: a runaway device database must be
    // refused here, not copied into the snapshot directory byte by byte.
    bounded_copy(&device_src, &snap_copy, MAX_DB_BYTES, DestMode::Fresh).ok()?;
    let version = rusqlite::Connection::open(&snap_copy)
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT value FROM LocalStorage WHERE key = 'PatchVersion'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
        })
        .and_then(|raw| match serde_json::from_str::<Value>(&raw).ok()? {
            Value::String(version) => Some(version),
            _ => None,
        })
        .map(|version| version.trim().to_string())
        .filter(|version| !version.is_empty() && version.len() <= 32);
    DbReader::remove_quiet(&snap_copy);
    version
}

/// Resolve `Client/Saved/LocalStorage` under a game directory, accepting
/// both known layouts: Steam (`<dir>/Client/...`) and standalone
/// launchers such as Twintail (`<dir>/Wuthering Waves Game/Client/...`).
/// The Wine prefix is intentionally irrelevant: LocalStorage always lives
/// next to the game files, never inside the prefix.
pub fn resolve_storage_dir(game_dir: &Path) -> Option<PathBuf> {
    let direct = game_dir.join("Client/Saved/LocalStorage");
    if direct.is_dir() {
        return Some(direct);
    }
    for nested in ["Wuthering Waves Game", "wuthering waves game"] {
        let candidate = game_dir.join(nested).join("Client/Saved/LocalStorage");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// Default roots probed by [`discover_game_dir`]: Steam library, a generic
/// `~/Games` (Heroic/Lutris/manual installs), and Twintail's Flatpak data
/// dir (its exact game folder is user-chosen, so it is probed below).
pub fn default_roots() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let base = PathBuf::from(home);
    vec![
        base.join(".steam/steam/steamapps/common/Wuthering Waves"),
        base.join("Games"),
        base.join(".var/app/app.twintaillauncher.ttl/data"),
    ]
}

/// Auto-discover a game directory: each root, then subdirectories up to
/// two levels deep (covers user-chosen folders), bounded so pathological
/// trees cannot stall startup. Returns the game dir (not the storage
/// dir). `None` means: pass `--game-dir`.
pub fn discover_game_dir(roots: &[PathBuf]) -> Option<PathBuf> {
    const MAX_DEPTH: u32 = 2;
    const MAX_DIRS: usize = 1024;
    let mut visited = 0usize;
    let mut stack: Vec<(PathBuf, u32)> = roots.iter().map(|root| (root.clone(), 0)).collect();
    while let Some((dir, depth)) = stack.pop() {
        if resolve_storage_dir(&dir).is_some() {
            return Some(dir);
        }
        if depth >= MAX_DEPTH {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if visited >= MAX_DIRS {
                return None;
            }
            let path = entry.path();
            if path.is_dir() {
                visited += 1;
                stack.push((path, depth + 1));
            }
        }
    }
    None
}

/// Resolve `(game_dir, storage_dir)` from an explicit `--game-dir` or
/// auto-discovery roots. Explicit dirs must already contain a storage dir;
/// discovery probes each root (see [`discover_game_dir`]).
pub fn resolve_storage(
    game_dir: Option<&Path>,
    roots: &[PathBuf],
) -> Result<(PathBuf, PathBuf), Box<dyn std::error::Error>> {
    if let Some(dir) = game_dir {
        let storage = resolve_storage_dir(dir).ok_or_else(|| {
            format!(
                "no Client/Saved/LocalStorage under '{}' (tried Steam and standalone layouts)",
                dir.display()
            )
        })?;
        return Ok((dir.to_path_buf(), storage));
    }
    let found = discover_game_dir(roots)
        .and_then(|dir| resolve_storage_dir(&dir).map(|storage| (dir, storage)));
    match found {
        Some((dir, storage)) => {
            log::info(format!("auto-discovered game at '{}'", dir.display()));
            Ok((dir, storage))
        }
        None => Err("game not found; launch it once via Steam/Twintail/etc., \
      or pass --game-dir pointing at the folder containing Client/"
            .into()),
    }
}

/// `true` for `LocalStorage2.db`, `LocalStorage10.db`, ... — never for the
/// main `LocalStorage.db` or its sidecars (`-journal`, `-wal`, `-shm`).
///
/// # Examples
///
/// ```
/// use wwrpc::db::is_numbered_db;
///
/// assert!(!is_numbered_db("LocalStorage.db"));
/// assert!(!is_numbered_db("LocalStorage.db-journal"));
/// assert!(is_numbered_db("LocalStorage2.db"));
/// ```
pub fn is_numbered_db(file_name: &str) -> bool {
    let Some(rest) = file_name.strip_prefix("LocalStorage") else {
        return false;
    };
    let Some(num) = rest.strip_suffix(".db") else {
        return false;
    };
    !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit())
}

/// `true` if any numbered database exists in `dir`. I/O errors count as
/// `false`: the copy flow reports them separately.
fn has_numbered_db(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_str().is_some_and(is_numbered_db))
        })
        .unwrap_or(false)
}

/// Full triage of a PRIVATE snapshot copy, in order: openable as SQLite,
/// expected table + columns, integrity check, required keys present with
/// expected shapes, known uid (or explicit 1-based `account_index`).
/// Only `Ok` releases data to presence. Also returns the live login uid
/// (`RecentlyLoginUID`), when readable, for display-name lookup — it is
/// independent of pinning/override by design.
fn query_snapshot(
    snap: &Path,
    uid_override: Option<&str>,
    account_index: Option<usize>,
) -> Result<(PlayerInfo, Option<String>), TriageError> {
    let conn = rusqlite::Connection::open(snap).map_err(|_| TriageError::Unopenable)?;
    // Belt and suspenders: even temp files for sorting must stay in RAM.
    let _ = conn.execute_batch("PRAGMA temp_store = MEMORY");
    check_schema(&conn)?;
    check_integrity(&conn)?;
    let live_uid = load_uid(&conn, None).ok();
    let account = match account_index {
        Some(index) => load_sdk_level_by_index(&conn, index)?,
        None => {
            let uid = load_uid(&conn, uid_override)?;
            let (region, level) = load_sdk_level(&conn, &uid)?;
            SdkAccount { uid, region, level }
        }
    };
    // Quest count is best-effort: absence never blocks the rest.
    let quests_done = load_quest_count(&conn, &account.uid);
    Ok((
        PlayerInfo {
            uid: account.uid,
            union_level: account.level,
            region: account.region,
            quests_done,
            // Attached by refresh() from the device database, when readable.
            game_version: None,
            // Attached by refresh() from live telemetry, when readable.
            display_name: None,
        },
        live_uid,
    ))
}

/// One `SdkLevelData` row by 1-based position (see `--list-accounts`).
/// Out-of-range positions reject like an unknown uid.
pub(crate) fn load_sdk_level_by_index(
    conn: &rusqlite::Connection,
    index: usize,
) -> Result<SdkAccount, TriageError> {
    let body = get_value(conn, "SdkLevelData")?;
    let mut accounts = parse_sdk_accounts(&body).ok_or(TriageError::Malformed("SdkLevelData"))?;
    if index == 0 || index > accounts.len() {
        return Err(TriageError::UnknownUid);
    }
    Ok(accounts.remove(index - 1))
}

/// The snapshot must expose `LocalStorage(key text, value text)`.
pub(crate) fn check_schema(conn: &rusqlite::Connection) -> Result<(), TriageError> {
    // PRAGMA table_info rows: (cid, name, type, notnull, default, pk).
    let mut stmt = conn
        .prepare("PRAGMA table_info(LocalStorage)")
        .map_err(|_| TriageError::MissingTable)?;
    let columns: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(1)?, row.get(2)?)))
        .map_err(|_| TriageError::MissingTable)?
        .collect::<Result<_, _>>()
        .map_err(|_| TriageError::BadSchema)?;
    if columns.is_empty() {
        return Err(TriageError::MissingTable);
    }
    let has = |name: &str| {
        columns
            .iter()
            .any(|(col, _)| col.eq_ignore_ascii_case(name))
    };
    if has("key") && has("value") {
        Ok(())
    } else {
        Err(TriageError::BadSchema)
    }
}

/// Fast corruption gate: catches torn copies the mtime check may miss.
pub(crate) fn check_integrity(conn: &rusqlite::Connection) -> Result<(), TriageError> {
    let status: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| TriageError::Corrupt)?;
    if status == "ok" {
        Ok(())
    } else {
        Err(TriageError::Corrupt)
    }
}

fn get_value(conn: &rusqlite::Connection, key: &'static str) -> Result<String, TriageError> {
    use rusqlite::OptionalExtension as _;
    conn.query_row(
        "SELECT value FROM LocalStorage WHERE key = ?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|_| TriageError::Malformed(key))?
    .ok_or(TriageError::MissingKey(key))
}

/// Resolve the account uid: explicit override wins (blank means absent),
/// otherwise the auto-detected `RecentlyLoginUID`.
pub(crate) fn load_uid(
    conn: &rusqlite::Connection,
    uid_override: Option<&str>,
) -> Result<String, TriageError> {
    match uid_override.map(str::trim) {
        Some(uid) if !uid.is_empty() => Ok(uid.to_string()),
        _ => json_scalar(&get_value(conn, "RecentlyLoginUID")?).ok_or(TriageError::UnknownUid),
    }
}

/// Region + Level for one uid. A well-formed database that simply lacks
/// the uid reports how many accounts it stores and whether the uid shows
/// up anywhere else, so operators can tell "known account, level row
/// missing yet" apart from "fully unknown account".
pub(crate) fn load_sdk_level(
    conn: &rusqlite::Connection,
    uid: &str,
) -> Result<(String, String), TriageError> {
    let body = get_value(conn, "SdkLevelData")?;
    let content_len = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| value.get("Content")?.as_array().map(Vec::len))
        .ok_or(TriageError::Malformed("SdkLevelData"))?;
    if let Some(found) = parse_sdk_level_data(&body, uid) {
        return Ok(found);
    }
    Err(TriageError::AccountNotInLevelData {
        known_accounts: content_len,
        uid_seen_elsewhere: has_account_data(conn, uid),
    })
}

/// Whether ANY per-uid key (`<anything>_<uid>`) exists. The pattern is
/// parameterized, never interpolated, so odd uids can only over-match,
/// never inject.
pub(crate) fn has_account_data(conn: &rusqlite::Connection, uid: &str) -> bool {
    let suffix = format!("_{uid}");
    conn.query_row(
        "SELECT COUNT(*) FROM LocalStorage WHERE key LIKE '%' || ?1",
        [suffix],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .unwrap_or(false)
}

/// Finished-quest count for one uid. Best-effort by design: `None` only
/// drops the quests segment of presence.
fn load_quest_count(conn: &rusqlite::Connection, uid: &str) -> Option<usize> {
    let body = get_value(conn, "UserFinishedQuests").ok()?;
    parse_quest_count(&body, uid)
}

/// Normalize a raw `LocalStorage` value (JSON number or string) to text.
fn json_scalar(body: &str) -> Option<String> {
    match serde_json::from_str::<Value>(body).ok()? {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s),
        _ => None,
    }
}

/// One parsed `SdkLevelData` row. The uid only travels as far as the
/// triage/quest lookups below; it is never logged or displayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SdkAccount {
    pub uid: String,
    pub region: String,
    pub level: String,
}

/// Parse every account row of `SdkLevelData`. Malformed rows are skipped
/// so one odd entry (e.g. added by a future patch) never nukes the whole
/// parse; `None` means structurally not an account map at all.
pub(crate) fn parse_sdk_accounts(body: &str) -> Option<Vec<SdkAccount>> {
    let value: Value = serde_json::from_str(body).ok()?;
    let content = value.get("Content")?.as_array()?;
    let mut accounts = Vec::with_capacity(content.len());
    for entry in content {
        let Some(pair) = entry.as_array() else {
            continue;
        };
        let Some(uid) = pair.first().and_then(|uid| uid.as_str()) else {
            continue;
        };
        let Some(data) = pair
            .get(1)
            .and_then(|data| data.as_array())
            .and_then(|items| items.first())
        else {
            continue;
        };
        let Some(region) = data.get("Region").and_then(|region| region.as_str()) else {
            continue;
        };
        let level = match data.get("Level") {
            Some(Value::Number(level)) => level.to_string(),
            Some(Value::String(level)) => level.clone(),
            _ => continue,
        };
        if region.is_empty() || level.is_empty() {
            continue;
        }
        accounts.push(SdkAccount {
            uid: uid.to_string(),
            region: region.to_string(),
            level,
        });
    }
    Some(accounts)
}

/// Valid `(region, level)` pairs of `SdkLevelData`, without uids, for
/// display purposes such as `--list-accounts`.
///
/// # Examples
///
/// ```
/// use wwrpc::db::parse_account_list;
///
/// let body = r#"{"Content": [["1", [{"Region": "Europe", "Level": 60}]], ["2", [{"Region": "X"}]]]}"#;
/// assert_eq!(
///   parse_account_list(body),
///   Some(vec![("Europe".to_string(), "60".to_string())])
/// );
/// assert_eq!(parse_account_list("oops"), None);
/// ```
pub fn parse_account_list(body: &str) -> Option<Vec<(String, String)>> {
    parse_sdk_accounts(body)
        .map(|accounts| accounts.into_iter().map(|a| (a.region, a.level)).collect())
}

/// Parse `SdkLevelData` (`{"Content": [[uid, [{Region, Level}]]], ...}`)
/// for one account. Returns `(region, level)`.
///
/// # Examples
///
/// ```
/// use wwrpc::db::parse_sdk_level_data;
///
/// let body = r#"{"Content": [["1", [{"Region": "Europe", "Level": 60}]]]}"#;
/// assert_eq!(
///   parse_sdk_level_data(body, "1"),
///   Some(("Europe".to_string(), "60".to_string()))
/// );
/// assert_eq!(parse_sdk_level_data(body, "2"), None);
/// ```
pub fn parse_sdk_level_data(body: &str, uid: &str) -> Option<(String, String)> {
    parse_sdk_accounts(body).and_then(|accounts| {
        accounts
            .into_iter()
            .find_map(|account| (account.uid == uid).then_some((account.region, account.level)))
    })
}

/// Count finished quests for one account (`UserFinishedQuests` holds
/// `[[uid, {Content: [...]}], ...]`). Only the entry count is used —
/// quest ids themselves are never read.
///
/// # Examples
///
/// ```
/// use wwrpc::db::parse_quest_count;
///
/// let body = r#"{"Content": [["1", {"Content": [7, 8, 9]}]]}"#;
/// assert_eq!(parse_quest_count(body, "1"), Some(3));
/// assert_eq!(parse_quest_count(body, "2"), None);
/// ```
pub fn parse_quest_count(body: &str, uid: &str) -> Option<usize> {
    let value: Value = serde_json::from_str(body).ok()?;
    let content = value.get("Content")?.as_array()?;
    for entry in content {
        let pair = entry.as_array()?;
        if pair.first()?.as_str()? != uid {
            continue;
        }
        return pair.get(1)?.get("Content")?.as_array().map(Vec::len);
    }
    None
}
