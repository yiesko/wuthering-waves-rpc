// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! LocalStorage reader with copy-to-a-private-snapshot semantics.
//!
//! A tick copies only when something a snapshot is built from was written
//! since the last one it accepted, and that set is the account database
//! together with the SQLite sidecars beside it ([`snapshot_signature`]). The
//! main file alone is not the set: a game running in WAL mode commits its
//! rows into `-wal` and leaves the main file's timestamp standing still.
//!
//! Safety model: the live game files are NEVER opened with SQLite, never
//! written to, never chmodded and never kept open, so no SQLite lock is ever
//! taken on them. Each tick copies `LocalStorage.db` (plus journal) into a
//! private snapshot directory through [`bounded_copy`], which refuses a
//! source past [`MAX_DB_BYTES`] (past [`MAX_JSON_BYTES`] for the launcher
//! cache), caps the bytes it moves, and proves the source did not change
//! mid-copy. Only the private copy is queried, and it is deleted right
//! after.
//!
//! A private copy is also opened alone: a sidecar a killed run left beside
//! it is cleared first, because SQLite reads a leftover journal as a hot one
//! and would roll its pages back into the copy this tick just made. A clear
//! that fails warns and the tick carries on, because refusing the snapshot
//! would cost the reader its data for the whole session.
//!
//! Copy mode follows the caller's intent, not the directory it picked: the
//! runtime directory is never emptied, so its per-tick copies are
//! *replaced* ([`DestMode::Overwrite`]), while a one-shot listing copies
//! into a [`PrivateTempDir`] built for that single call and starts from
//! nothing, so its copies are [`DestMode::Fresh`] — an entry already on the
//! path is refused rather than adopted.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::log;
use crate::runtime_dir::{DestMode, PrivateTempDir, bounded_copy};
use crate::telemetry_cache::{FileSignature, Lookup, TelemetryCache, file_signature};

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

/// How many rows one read of a file written by someone else takes in. A
/// bound on *count*, applied by stopping at it: a telemetry query stops
/// there, and a cache builds no more entries than this and skips the rest
/// rather than building and discarding them, because a JSON parser has to
/// consume a whole list to know where it ends.
///
/// It bounds the role rows a telemetry read looks at and the account rows
/// the launcher cache hands back. It does **not** bound the account rows
/// `parse_sdk_accounts` walks out of the game's own database, which are
/// bounded only by that file's byte ceiling, nor the quest entries
/// [`parse_quest_count`] counts.
pub const MAX_ROWS: usize = 500;

/// The longest single display name one read accepts, in characters. A bound
/// on *length*, and a different act from [`MAX_ROWS`]: an over-long name is
/// dropped rather than shortened, because a cut name is a different — and
/// wrong — name shown to a user. A name sitting exactly on the cap is kept
/// whole, whatever characters it is made of.
pub const MAX_NAME_CHARS: usize = 32;

/// The longest account identifier one read accepts, in characters. Bounded
/// for the same reason and in the same way as [`MAX_NAME_CHARS`]: an
/// identifier this long is not one this program expects to match, so
/// keeping it would cost memory and map entries and nothing else.
pub const MAX_CUID_CHARS: usize = 32;

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

/// Polling reader over the game's `LocalStorage`: copy-to-private-snapshot,
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
    /// Display name keyed by the role uid it was resolved for. Survives
    /// telemetry gaps; dies on role switch.
    cached_display: Option<(String, String)>,
    /// What the launcher's telemetry database had for the live login, keyed
    /// on that file's signature. Both lookups into it are cached here — the
    /// live rows (see [`TelemetryCache`]) and the deleted-row remnants — and
    /// a miss is kept like a hit, so an unwritten telemetry file costs no
    /// copy at all. Dropped with [`DbReader::cached_display`] whenever a new
    /// login shows up, which no signature could show on its own.
    telemetry_names: TelemetryCache<String>,
    /// Deleted-row remnants for the live login, under the same signature:
    /// the same miss-is-an-answer rule, for the same file. One of those
    /// answers depends on the clock rather than on the bytes — a freshness
    /// gate the signature cannot express, and one whose error is bounded
    /// (see [`DbReader::remnant_role`]).
    telemetry_remnants: TelemetryCache<(Option<RemnantRole>, RemnantStats)>,
    /// Game version from the last successful device read, independent of
    /// triage outcome: every enrichment complements the others, so a
    /// failed level triage must not take the version down with it.
    cached_version: Option<String>,
    /// Signature of the account database and its sidecars at the last
    /// accepted snapshot, so a tick can tell whether anything a snapshot
    /// is built from was written since (see [`snapshot_signature`]).
    /// Written only where a snapshot is accepted, never on a copy that
    /// was refused or discarded.
    last_signature: Option<FileSignature>,
    last: Option<PlayerInfo>,
    degraded: bool,
    /// Why the last warning was printed, to log each distinct reason once
    /// instead of spamming every tick while it persists. One slot rather
    /// than one per kind, because a tick reports at most one reason and
    /// two that alternate are two separate events to warn about, not one
    /// condition to merge.
    last_reported: Option<Reported>,
    /// Ticks that printed the unmeasurable-source-set warning, so a test
    /// can count what the deduplication is for. Test-only; the production
    /// reader has no use for it.
    #[cfg(test)]
    signature_warnings: usize,
}

/// Why a tick degraded and said so. One line per distinct reason: a fault
/// that persists is reported once, and one that clears is reported again if
/// it returns.
///
/// A signature failure and a triage failure cannot happen in the same tick —
/// the first stops the tick before the second is reached — so these are
/// variants rather than a concatenation, and the deduplication is on the
/// variant.
// A tick whose source set cannot be measured and a tick whose triage failed
// need different answers from whoever reads the log, so collapsing the two
// into one record would hide one behind the other.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Reported {
    /// A snapshot was taken and rejected by triage.
    Triage(TriageError),
    /// The source set could not be measured, so the tick published nothing
    /// new. Carries nothing else: the message is built from the error
    /// itself, which names the file that could not be read, and the
    /// deduplication is on this variant alone. One sidecar becoming
    /// unmeasurable while another already is therefore not reported on its
    /// own — the two are one record until a snapshot succeeds and clears
    /// it. The fault is still announced either way; only a second line
    /// naming a second file is not.
    Signature,
}

impl DbReader {
    /// Open a reader: `storage_dir` holds the live `LocalStorage.db`,
    /// `snapshot_dir` holds the private per-tick copies — the
    /// [`runtime_dir`](crate::runtime_dir::runtime_dir) directory in the
    /// daemon's case, a [`PrivateTempDir`] for a dry run — and
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
            telemetry_names: TelemetryCache::new(),
            telemetry_remnants: TelemetryCache::new(),
            cached_version: None,
            last_signature: None,
            last: None,
            degraded: false,
            last_reported: None,
            #[cfg(test)]
            signature_warnings: 0,
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
        match &self.last_reported {
            Some(Reported::Triage(err)) => Some(*err),
            _ => None,
        }
    }

    /// Why the reader last warned, or `None` if it has not. Test-only
    /// observability: the warnings themselves go to the process's stderr,
    /// which a test cannot read, so this exposes the record that decides
    /// whether one is printed.
    #[cfg(test)]
    pub(crate) fn last_warning(&self) -> Option<&'static str> {
        match &self.last_reported {
            Some(Reported::Triage(_)) => Some("triage"),
            Some(Reported::Signature) => Some("signature"),
            None => None,
        }
    }

    /// How many times this reader has printed the "cannot tell whether the
    /// game wrote" warning. Test-only, and counted here rather than
    /// intercepted from the log: a counter beside the emission is what a
    /// test can read, where the line itself goes to a process's stderr.
    #[cfg(test)]
    pub(crate) fn signature_warnings(&self) -> usize {
        self.signature_warnings
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

        // "Has anything been written to the account database, or to the
        // sidecars a snapshot of it is built from, since the last snapshot
        // this reader accepted?" — a question about the whole set, because
        // the set is what the snapshot holds. A game running in WAL mode
        // commits its rows into `-wal` and leaves this database's timestamp
        // standing still, and a decision made from the main file alone
        // would call that unchanged and keep serving the older snapshot.
        //
        // Strict where the sidecar copy loop is lenient: a copy that
        // cannot be made is best effort (a snapshot missing a sidecar is
        // still worth reading), but a signature that cannot be taken means
        // this reader cannot tell whether a file was written, and guessing
        // "unchanged" is how a stale snapshot outlives the write that
        // superseded it. The cost runs the other way from that loop's — an
        // unmeasurable sidecar holds the tick at its last data until the
        // file is readable again — which is why it is announced rather than
        // left to a debug line, and why the two must not be read as one
        // thing: the copy loop's warning is about a sidecar it could not
        // CLEAR, and the tick carries on either way.
        let signature = match snapshot_signature(&self.storage_dir) {
            Ok(signature) => signature,
            Err(err) => {
                // Loud, once. Deduped exactly like triage failures: a
                // condition that persists is worth one line, and one that
                // clears and returns is worth another.
                //
                // The line says the tick publishes nothing new rather than
                // that it holds previous data, because the two are not the
                // same: a fault present from the start has no previous data
                // to hold, and "holding" would describe a snapshot that
                // does not exist.
                if self.last_reported != Some(Reported::Signature) {
                    self.last_reported = Some(Reported::Signature);
                    #[cfg(test)]
                    {
                        self.signature_warnings += 1;
                    }
                    log::warn(format!(
                        "cannot tell whether the game wrote to its database \
                         ({err}); this tick publishes nothing new until that \
                         file can be read again"
                    ));
                }
                return self.cached_or_unavailable();
            }
        };

        if self.last_signature == Some(signature) && self.last.is_some() {
            log::debug("snapshot unchanged since last tick; reusing data");
            return Refresh::Unchanged;
        }

        // Display-name tracking shares the account database's cadence. The
        // login a name is resolved against lives in that database, so a tick
        // finding it unchanged has no new login to resolve for — and the
        // other tick that reaches this line, the one where nothing the
        // snapshot is built from has changed but no snapshot has been
        // accepted, has nothing to resolve for either. A rename waits for
        // the next tick that database does change, which a running game
        // produces on its own. The trade is deliberate: telemetry rows are
        // written at login and deleted once uploaded, so re-reading them
        // every tick meant re-copying a file nobody had written to be told,
        // most of the time, that there was still no name in it. What the
        // lookup reads is a separate question with its own answer — the
        // telemetry file's signature (see `refresh_display_name`).
        self.refresh_display_name();

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
            // Best effort: without sidecars a torn copy cannot recover, and a
            // source that is missing or oversized is refused before the
            // destination is opened at all — one is not there to read, the
            // other is measured and refused before a byte is written. A
            // source rewritten under the copy is caught afterwards instead,
            // and what it leaves on the destination is unlinked rather than
            // kept. Either way the path holds this tick's copy or nothing,
            // but only once the leftover a killed run left has been cleared,
            // which is what the removal below does first. Skipping it would
            // leave a `-journal` in place: not inert data beside a database,
            // since SQLite reads one as a hot journal and rolls the pages it
            // holds back into the copy made a moment ago. A removal that
            // itself fails is warned about rather than made fatal — the next
            // tick tries again, while refusing the snapshot would cost the
            // reader's data for the whole session.
            Self::remove_before_copy(snap);
            let _ = bounded_copy(src, snap, MAX_DB_BYTES, DestMode::Overwrite);
        }

        // Discard torn copies: the game must not have touched the source while
        // we were reading it. A different question from the change
        // signature this tick was gated on, which asked whether anything has
        // changed since the last accepted snapshot — this one asks about the
        // handful of milliseconds the copy just took, and folding the
        // sidecars into it would make an ordinary commit discard a good
        // snapshot instead of noticing it.
        //
        // Every copy here, sidecars included, already re-measures its own
        // source through the handle the bytes came from. What that handle
        // cannot see is the name: a source replaced by a rename while we were
        // copying it keeps the same inode and the same contents, so only a
        // stat of the path notices. This asks exactly that, of the main
        // database — the file a torn snapshot is built on. A sidecar swapped
        // the same way is left to the copy that read it, and the limit is
        // worth stating plainly rather than dressing up: each file that
        // lands is a whole version of itself, so no snapshot is ever torn
        // *within* a file, but the set as a whole can then describe two
        // instants — this tick's database beside the sidecar the path held
        // a moment ago. That is a cheaper price than the alternative, which
        // is discarding the snapshot on every tick a game happens to commit,
        // and it is a price the next tick undoes: the fold measures
        // whatever is at the path then, so the sidecar that arrived in the
        // meantime is what the following snapshot is taken against.
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
            // The telemetry caches go with it: they are keyed on the file
            // alone, and this login arrived without the file changing, so a
            // signature that still matches would hand back the previous
            // account's name and remnant.
            self.telemetry_names.clear();
            self.telemetry_remnants.clear();
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
                self.last_signature = Some(signature);
                self.last = Some(info);
                self.last_reported = None;
                // The login this tick revealed was unknown to the call above,
                // so resolve the display name once more against it. The
                // telemetry lookup is keyed on that file's signature: when
                // nothing was written since, this costs a stat and no copy.
                self.refresh_display_name();
                if log::is_verbose() {
                    let quests = self
                        .last
                        .as_ref()
                        .is_some_and(|info| info.quests_done.is_some());
                    let version = self
                        .last
                        .as_ref()
                        .is_some_and(|info| info.game_version.is_some());
                    log::debug(accepted_message(quests, version));
                }
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
                        && log::is_verbose()
                    {
                        log::debug(missing_level_message(known_accounts, uid_seen_elsewhere));
                    }
                    let (recovered, stats) = self.remnant_role(&live);
                    if log::is_verbose() {
                        log::debug(scan_census_message(stats));
                    }
                    if let Some(role) = recovered {
                        let age_secs = epoch_millis()
                            .map_or(0, |now_ms| now_ms.saturating_sub(role.event_ms) / 1000);
                        if !log::is_quiet() {
                            log::info(recovered_message(age_secs, self.cached_version.is_some()));
                        }
                        self.last_signature = Some(signature);
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
                if self.last_reported != Some(Reported::Triage(err)) {
                    self.last_reported = Some(Reported::Triage(err));
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

    /// Display-name tracking, for the live login only. When this runs and
    /// what it re-reads are two separate questions: the call rides the
    /// account database's cadence — [`DbReader::refresh`] makes it whenever
    /// that database is about to be read again (its signature moved, or no
    /// snapshot has been accepted yet) and once more if triage takes the
    /// copy it just made — while the lookup itself is gated on the
    /// *telemetry* database's own signature and skipped entirely while that
    /// file stands still, whether the answer was a name or the far more
    /// common "no name in these rows".
    ///
    /// A signature says nothing about which login asked, so a login switch
    /// has to drop the caches itself; [`DbReader::refresh`] is where that
    /// happens and nothing else moves `live_uid` once the reader is open.
    /// The guard below re-checks that and cannot fire while it holds —
    /// insurance against a future path, not a second gate on the one that
    /// exists — and a re-resolution here always targets the current login.
    ///
    /// Attaches to `last_info` when it lacks a name; explicit
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
            // Unreachable while `DbReader::refresh` clears the caches in the
            // branch that moves the login — kept as the cheap backstop for a
            // future path that does not, because the cost of being wrong here
            // is another role's name on the presence.
            log::debug("display cache dropped on role switch");
            self.cached_display = None;
            self.telemetry_names.clear();
            self.telemetry_remnants.clear();
        }
        // A telemetry database that is not there is not a version of one:
        // there is nothing to key a lookup on, and nothing to copy either —
        // the name stays whatever it already was.
        if let Some((source, signature)) = self.telemetry_source() {
            let resolved = {
                let role_uid = live_uid.as_str();
                let snap_dir = self.snap_db.parent();
                self.telemetry_names.get_with(signature, move || {
                    // Logged from inside the loader, so this line means a
                    // read really happened rather than a cached answer.
                    log::debug("telemetry rewritten; reading the live display name");
                    Self::live_role_name(&source, snap_dir, role_uid)
                })
            };
            match resolved {
                Some(name) => {
                    log::debug("live display name known");
                    self.cached_display = Some((live_uid, name));
                }
                None => log::debug("no live display name in telemetry rows; still missing: name"),
            }
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

    /// The launcher telemetry database and the signature to key a lookup on
    /// it with. `None` when there is no such file to read: no game dir, a
    /// layout with no Steam prefix beside it, or the file itself gone. Both
    /// telemetry lookups key on this one signature, because live rows and
    /// deleted-row remnants are two readings of the same file.
    fn telemetry_source(&self) -> Option<(PathBuf, FileSignature)> {
        let source = kddata_path(self.game_dir.as_deref()?)?;
        let signature = file_signature(&source).ok()?;
        Some((source, signature))
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

    /// Clear a path this tick is about to re-copy, so a leftover from a
    /// killed run cannot reach the snapshot SQLite opens afterwards.
    ///
    /// The counterpart of [`DbReader::remove_quiet`] for the one removal a
    /// copy cannot make on its own: a refused copy leaves whatever was on
    /// the path untouched, so a failure here is warned about rather than
    /// swallowed. It is still not a reason to skip the snapshot — the next
    /// tick clears the path again, while a snapshot abandoned for the
    /// session would cost the rich presence this reader exists to publish.
    /// A path already gone is the ordinary case and stays silent.
    fn remove_before_copy(path: &Path) {
        if let Err(err) = fs::remove_file(path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            log::warn(format!(
                "cannot clear the leftover '{}': {err}; it may still reach the snapshot",
                path.display()
            ));
        }
    }

    /// Live display name for one login uid: exact `role_id` match over a
    /// private copy of the launcher telemetry database, under the same
    /// copy-to-private-snapshot rules and byte cap as every other game file
    /// read. Best-effort by construction: rows are ephemeral (written at
    /// login, deleted after upload), so absence is normal and never an
    /// error.
    ///
    /// [`Lookup::Unread`] wherever the copy did not happen or did not come
    /// back — nowhere to put it, or a copy that was refused. Only a file
    /// that was actually read answers, empty or not, so a failure costs the
    /// next tick another try instead of freezing the name as missing.
    ///
    /// Free-standing rather than a method because the caller holds the cache
    /// mutably; it needs the source file and a scratch directory, nothing more.
    fn live_role_name(kd_src: &Path, snap_dir: Option<&Path>, role_uid: &str) -> Lookup<String> {
        let Some(snap_dir) = snap_dir else {
            return Lookup::Unread;
        };
        let snap_copy = snap_dir.join("kdnames.db");
        // Replaced, never obeyed: this lookup owns the path, and the runtime
        // directory is never emptied, so a copy a killed run left behind
        // would otherwise refuse every lookup after it.
        if bounded_copy(kd_src, &snap_copy, MAX_DB_BYTES, DestMode::Overwrite).is_err() {
            return Lookup::Unread;
        }
        let name = query_kd_names(&snap_copy, role_uid);
        DbReader::remove_quiet(&snap_copy);
        Lookup::Answered(name)
    }

    /// Identity recovery for one login uid from a PRIVATE copy of the
    /// launcher telemetry database, live rows AND deleted-row remnants (see
    /// [`find_remnant_role`]). Best-effort, values never logged.
    ///
    /// Cached under the same file signature as [`DbReader::live_role_name`],
    /// so a "no fresh event in there" — what an unchanged file says almost
    /// every time — costs one copy per version instead of one per tick.
    ///
    /// Freshness in [`find_remnant_role`] is measured against the clock, not
    /// the file, and the cache does not know that: it freezes the verdict
    /// the scan reached, in both directions time could have moved it.
    ///
    /// The age gate is the later of the scan's two clock gates and only ever
    /// pushes an event toward stale, so a recovery accepted while its event
    /// was fresh goes on being served past the window. Bounded, because
    /// nothing is published from here that a re-scan would have withdrawn:
    /// an accepted recovery sticks in `last`, so a scan run past the window
    /// reports no fresh event and falls through to that same data. The
    /// [`REMNANT_SKEW_MS`] allowance is the earlier gate and pushes the
    /// other way: an event dated that far ahead of our clock is skipped now
    /// and a cached *miss* does not follow it when the clock reaches it, so
    /// it is only picked up if the launcher writes the file again. Smaller
    /// than the direction above: it needs a clock disagreement that outlives
    /// the session, on the path already degraded by a missing level row, and
    /// it costs a recovered identity rather than a wrong one.
    ///
    /// Either way the answer is the live login's own most recent event. What
    /// is given up beyond them is the per-tick census line, which prints
    /// these numbers from the cache and reads like a scan that just ran.
    fn remnant_role(&mut self, role_uid: &str) -> (Option<RemnantRole>, RemnantStats) {
        let Some((source, signature)) = self.telemetry_source() else {
            log::debug("telemetry scan skipped: no telemetry database to read");
            return (None, RemnantStats::default());
        };
        let snap_dir = self.snap_db.parent();
        self.telemetry_remnants
            .get_with(signature, move || {
                Self::scan_remnant_role(&source, snap_dir, role_uid)
            })
            .unwrap_or_default()
    }

    /// Take the private telemetry copy apart in search of a deleted row for
    /// one login uid. The counterpart of [`DbReader::live_role_name`] for the
    /// other reading of the same file: the bytes are gone from SQLite's
    /// tables but still in the pages, so the scan is a byte-level one.
    ///
    /// `Unread` when there was nothing to scan — no scratch directory, a copy
    /// that was refused, unreadable bytes, or no clock to date the events
    /// against. A scan that ran always answers, with the role it found
    /// (possibly none) and the census of what it looked at; both are kept
    /// under the file's signature, and the whole answer depends on the
    /// clock as well as the bytes (see [`DbReader::remnant_role`]).
    fn scan_remnant_role(
        kd_src: &Path,
        snap_dir: Option<&Path>,
        role_uid: &str,
    ) -> Lookup<(Option<RemnantRole>, RemnantStats)> {
        let Some(snap_dir) = snap_dir else {
            return Lookup::Unread;
        };
        let snap_copy = snap_dir.join("kdremnant.db");
        // Replaced, never obeyed: this scan owns the path, and the runtime
        // directory is never emptied, so a copy a killed run left behind
        // would refuse every scan after it. A refusal that still happens (too
        // large, rewritten mid-copy) reads its reason from the error, the way
        // the size check used to report it.
        if let Err(err) = bounded_copy(kd_src, &snap_copy, MAX_DB_BYTES, DestMode::Overwrite) {
            if log::is_verbose() {
                log::debug(format!("telemetry scan skipped: copy failed ({err})"));
            }
            return Lookup::Unread;
        }
        let bytes = fs::read(&snap_copy).ok();
        DbReader::remove_quiet(&snap_copy);
        let (Some(bytes), Some(now_ms)) = (bytes, epoch_millis()) else {
            return Lookup::Unread;
        };
        Lookup::Answered(Some(find_remnant_role(&bytes, role_uid, now_ms)))
    }

    /// One-shot account listing for `--list-accounts`: copies the database
    /// into a private scratch directory of this call's own, triages it like
    /// a tick would, and returns accounts in file order with login
    /// nicknames attached when locatable. Uids never leave this function,
    /// so the listing is safe to print.
    pub fn list_accounts(
        storage_dir: &Path,
        game_dir: &Path,
    ) -> Result<Vec<StoredAccount>, TriageError> {
        // A scratch directory created for this call alone, inside the
        // private runtime directory and gone when the guard drops — no
        // shared temp dir, and nothing for a concurrent listing to trip
        // over. A failure here is about that directory, not the database,
        // so it says which before collapsing into the triage verdict.
        let scratch = PrivateTempDir::new("list").map_err(|err| {
            log::warn(format!("cannot create the listing scratch dir: {err}"));
            TriageError::Unopenable
        })?;
        Self::query_list(&scratch.path().join("snapshot.db"), storage_dir, game_dir)
    }

    fn query_list(
        snap_db: &Path,
        storage_dir: &Path,
        game_dir: &Path,
    ) -> Result<Vec<StoredAccount>, TriageError> {
        // A listing copies into a directory created for this call alone, so
        // every snapshot starts from nothing: an entry already on the path
        // is refused, not adopted — a second barrier behind the private,
        // owner-only directory the copies land in.
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
        // Launcher files follow the same copy-to-private-snapshot rule: snapshot
        // them into our scratch dir and read only the copies, never the
        // originals.
        let cache = match launcher_cache_path(game_dir) {
            Some(cache_src) if snap_db.parent().is_some() => {
                let snap_dir = snap_db.parent().expect("snapshot has a parent dir");
                let snap_cache = snap_dir.join("launcher-cache.json");
                // A JSON cache gets its own, much smaller ceiling: the real
                // file holds a handful of accounts in a few kilobytes. A
                // refusal costs the nicknames, never the listing, and says
                // why instead of failing in silence.
                if let Err(err) =
                    bounded_copy(&cache_src, &snap_cache, MAX_JSON_BYTES, DestMode::Fresh)
                {
                    log::debug(format!("launcher cache skipped: copy failed ({err})"));
                    LauncherCache::default()
                } else {
                    // ONE read, ONE parse, both answers: the nicknames and the
                    // last-login marker are two fields of one file, and a
                    // second pass is a second walk of bytes this one already
                    // had. A reader handed a live file would also be reading
                    // two different versions of it.
                    let cache = read_launcher_cache(&snap_cache).unwrap_or_else(|err| {
                        log::debug(format!("launcher cache unreadable: {err}"));
                        LauncherCache::default()
                    });
                    let _ = fs::remove_file(&snap_cache);
                    cache
                }
            }
            _ => LauncherCache::default(),
        };
        let nicknames = cache.nicknames();
        Ok(accounts
            .into_iter()
            .enumerate()
            .map(|(position, account)| StoredAccount {
                index: position + 1,
                nickname: nicknames
                    .get(account.uid.as_str())
                    .copied()
                    .map(String::from),
                last_login: cache.last_login_cuid.as_deref() == Some(account.uid.as_str()),
                region: account.region,
                level: account.level,
            })
            .collect())
    }
}

/// The line a tick that accepted a snapshot prints: which enrichment
/// survived triage, as shapes only. Built only when the level prints it,
/// so a quiet tick pays nothing for the sentence.
pub(crate) fn accepted_message(quests_done: bool, version_known: bool) -> String {
    format!("snapshot accepted; quests data: {quests_done}, version data: {version_known}")
}

/// The line a tick prints before scanning telemetry remnants: what triage
/// wanted against what the database held, as shapes only. Same lazy rule
/// as [`accepted_message`]: no allocation when the level is off.
pub(crate) fn missing_level_message(known_accounts: usize, uid_seen_elsewhere: bool) -> String {
    format!(
        "level row missing for live login (expected 1 row, found {known_accounts} other row(s), \
         login seen elsewhere: {uid_seen_elsewhere}); trying telemetry remnants"
    )
}

/// The line a tick prints after scanning telemetry remnants: the scan
/// census, shapes only. Built under the same lazy rule; the numbers come
/// from the cache, so this reads like a scan that just ran either way.
pub(crate) fn scan_census_message(stats: RemnantStats) -> String {
    format!(
        "telemetry scan: {} candidate(s), {} login event(s), {} fresh event(s)",
        stats.candidates, stats.role_hits, stats.fresh_hits
    )
}

/// The line a tick that recovered an identity from remnants prints: how
/// old the event was and whether a version rode along, shapes only.
/// An info line, so it is built only while info lines print.
pub(crate) fn recovered_message(event_age_secs: u64, version_attached: bool) -> String {
    format!(
        "role identity recovered from launcher telemetry (event age {event_age_secs}s): \
         complemented name+level+region; quests unavailable (no quest row for this login); \
         version attached: {version_attached}"
    )
}

/// One version of everything a per-tick snapshot is built from: the
/// account database at `dir`/`LocalStorage.db` and the SQLite sidecars
/// beside it, folded into the single value a tick compares against the one
/// its last accepted snapshot was taken at.
///
/// The main database is required; a sidecar is not, and one that is absent
/// contributes nothing rather than failing the call. That contribution can
/// never be confused with a sidecar that is present but empty.
///
/// Equality is not proof that nothing was written: the fold is not
/// injective, and a collision costs one skipped copy until the next write
/// rather than a wrong answer. Nor does a signature here say anything a
/// signature anywhere does not — a rewrite leaving a file's length and
/// modification time exactly where they were is not a change.
///
/// # Errors
///
/// [`io::ErrorKind::NotFound`] when the main database is not there, and
/// whatever measuring a file failed with otherwise, untouched in kind. An
/// absent sidecar is the one absence that is not an error.
pub fn snapshot_signature(dir: &Path) -> io::Result<FileSignature> {
    // The main database first, and on its own terms: it is the file a
    // snapshot is built on, and nothing here substitutes for it.
    let (mut mtime, mut len) = signature_part(&dir.join(MAIN_DB))?;
    for suffix in SIDECAR_SUFFIXES {
        // The same names `DbReader::new` builds its sidecar pairs from, so
        // the fold and the copy can never disagree about which files a
        // snapshot is made of.
        match signature_part(&dir.join(format!("{MAIN_DB}{suffix}"))) {
            Ok((part_mtime, part_len)) => {
                mtime = mtime.saturating_add(part_mtime);
                len = len.saturating_add(part_len);
            }
            // The sentinel: a file the game does not keep contributes
            // nothing at all. A game in rollback-journal mode keeps no
            // `-wal` and no `-shm` at all, and that is the ordinary state
            // of a database nobody has opened yet, not a fault — the fold
            // neither fails nor invents a version for such a file.
            //
            // Only absence is the sentinel. A sidecar that is there and
            // cannot be measured — a changed permission, a symlink loop, an
            // untraversable parent — is a different observation, and
            // folding it into the sentinel would stop the caller noticing a
            // file that exists, which is the one thing a signature is for.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    // Both totals saturate rather than wrap, and neither can fold an
    // absurd file back onto an ordinary value.
    //
    // The time total cannot saturate at all: every part is the span
    // between two `SystemTime` values, at most a `u64` of seconds and so
    // about 1.8e28 nanoseconds, which four of them reach 7.4e28 — more
    // than nine orders of magnitude short of what this accumulator holds.
    // A maintainer who widens the file set has to re-derive that bound
    // rather than trust the arithmetic. The clamps behind it are a
    // backstop, and `nanos_since_epoch` says why they would stay honest
    // if they were ever reached.
    //
    // The length total is a `u64`, and one file of `u64::MAX` bytes already
    // drives it to the ceiling, so its clamp is a real bound and not an
    // unreachable one. It takes 18.45 exabytes to reach, which is above
    // the ceiling of every filesystem in use (ext4 stops at 16 TiB, xfs at
    // 8 EiB); at or past it every such set folds alike.
    //
    // The fold buys skipped copies and can also spend one, and which way
    // that runs is worth knowing before trusting the value. A `-shm` is
    // the shared index SQLite keeps beside a write-ahead log, derived
    // from the log and rewritten by whoever holds the database open, so a
    // game reading its own rows can leave the set looking rewritten with
    // no row changed. Which sidecars the dependency stamps in that case
    // is its business and not this crate's to rely on either way:
    // measured against the SQLite built into this binary, plain reads
    // leave the index's timestamp alone, while another build stamps it.
    // The direction is the safe one — a signature that moves when no row
    // did costs a copy of a database that did not change, never a
    // snapshot that was needed and not taken.
    Ok(FileSignature::new(time_from_nanos(mtime), len))
}

/// One file's contribution to [`snapshot_signature`]: its modification
/// time in nanoseconds since the epoch, and its length plus one.
///
/// The plus one is the presence bit [`snapshot_signature`] describes: it is
/// the difference between a file of no bytes and no file at all.
fn signature_part(path: &Path) -> io::Result<(i128, u64)> {
    // Tagged with its own path, because the underlying error names none:
    // "No such file or directory (os error 2)" is not something an operator
    // can act on when three sidecars are in question. The kind survives the
    // wrap, which is what the caller's `NotFound` branch matches on.
    let meta = fs::metadata(path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot measure '{}': {err}", path.display()),
        )
    })?;
    let mtime = meta
        .modified()
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot read the time of '{}': {err}", path.display()),
            )
        })
        .map(nanos_since_epoch)?;
    Ok((mtime, meta.len().saturating_add(1)))
}

/// Nanoseconds since the epoch, signed: a time before 1970 counts
/// negative, so a set of files can still be added up whatever the clock
/// says about any one of them. The negative branch is ordinary code, not a
/// formality — a Linux filesystem records timestamps from before the
/// epoch, so a game file stamped there reads back as a negative count of
/// seconds and lands here.
fn nanos_since_epoch(time: SystemTime) -> i128 {
    // Neither conversion can fail: `as_nanos` on a `Duration` is at most
    // about 1.8e28, far inside `i128`. The fallbacks are a backstop, and
    // they are chosen so that the negative one negates to `-i128::MAX`
    // rather than the accumulator's own floor — that way a part meeting
    // its opposite still sums to zero instead of clamping onto a neighbour.
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => i128::try_from(since.as_nanos()).unwrap_or(i128::MAX),
        Err(before) => -i128::try_from(before.duration().as_nanos()).unwrap_or(i128::MAX),
    }
}

/// A [`nanos_since_epoch`] total back as a time: the same instants added
/// back up, saturating at both ends rather than panicking on a date no
/// filesystem records.
fn time_from_nanos(nanos: i128) -> SystemTime {
    let magnitude = u64::try_from(nanos.unsigned_abs()).unwrap_or(u64::MAX);
    let duration = Duration::from_nanos(magnitude);
    if nanos < 0 {
        UNIX_EPOCH.checked_sub(duration).unwrap_or(UNIX_EPOCH)
    } else {
        UNIX_EPOCH.checked_add(duration).unwrap_or(UNIX_EPOCH)
    }
}

/// Display name for one login uid from a PRIVATE telemetry copy: exact
/// `role_id` match over live `KDData` rows. First match wins; names are
/// sanity-checked (1–[`MAX_NAME_CHARS`] characters, no control characters)
/// because they come from an external writer. Anything off — missing table,
/// bad rows, absent uid — yields `None`, never an error.
pub(crate) fn query_kd_names(snap: &Path, role_uid: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(snap).ok()?;
    let mut stmt = conn.prepare("SELECT content FROM KDData LIMIT ?1").ok()?;
    let rows = stmt
        .query_map([i64::try_from(MAX_ROWS).unwrap_or(i64::MAX)], |row| {
            row.get::<_, String>(0)
        })
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
        if !name.is_empty() && within(&name, MAX_NAME_CHARS) && !name.chars().any(char::is_control)
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

/// Milliseconds since the Unix epoch, or `None` when the clock says
/// otherwise (a system time before 1970, or one that does not fit). Every
/// telemetry event is dated against this, so a scan that cannot read it has
/// no way to tell a fresh event from a stale one.
fn epoch_millis() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
}

/// Recover `(name, level, server)` for one login uid from a PRIVATE
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
                        && within(name, MAX_NAME_CHARS)
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
                // 64 bytes, not MAX_NAME_CHARS: a server label is a different
                // quantity from a display name, and a different length,
                // from the same external writer — and this one is measured
                // in bytes, where the name bound beside it counts
                // characters. Unrelated to MAX_REMANT_CANDIDATES above,
                // which is the same number and bounds the scan by how many
                // `"role_id"` markers it steps over.
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

/// First occurrence of `needle` in `haystack` (avoids UTF-8 assumptions
/// over foreign bytes).
///
/// Linear in the buffer for a fixed needle: one pass skips to each
/// first-byte hit and only hits pay the full compare, instead of
/// comparing every window. An empty needle matches at the start, and a
/// needle longer than the buffer never matches. Dependency-free: both
/// call sites pass short literals.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    let first = needle[0];
    let mut offset = 0;
    while offset + needle.len() <= haystack.len() {
        let pos = offset + haystack[offset..].iter().position(|&byte| byte == first)?;
        if pos + needle.len() > haystack.len() {
            return None;
        }
        if haystack[pos..].starts_with(needle) {
            return Some(pos);
        }
        offset = pos + 1;
    }
    None
}

/// Last occurrence: the marker nearest BEHIND a hit, not the first in the
/// window (a window can span several adjacent events).
///
/// Linear for the same reason as [`find_subslice`], from the other end:
/// one backward pass over the needle's last byte, full compare only on
/// hits. Empty and over-long needles answer the same way as there, so
/// the two stay interchangeable wherever a direction is not the point.
fn rfind_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(haystack.len());
    }
    if needle.len() > haystack.len() {
        return None;
    }
    let last = needle[needle.len() - 1];
    let mut end = haystack.len();
    loop {
        let stop = haystack[..end].iter().rposition(|&byte| byte == last)? + 1;
        if stop < needle.len() {
            return None;
        }
        let pos = stop - needle.len();
        if haystack[pos..].starts_with(needle) {
            return Some(pos);
        }
        end = stop - 1;
    }
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

/// One account as the launcher's account cache describes it: the two fields
/// a listing needs, and nothing else.
///
/// The launcher keeps an e-mail address, a token and verification codes in
/// the same object, and none of them has a field here. A JSON parser skips
/// a key it is not asked for without building a value for it, so a secret
/// this program never asked for is never turned into a value at all.
///
/// Both fields are optional because both are independent in the file: an
/// account may carry a uid and no nickname. Both are bounded (see
/// [`MAX_CUID_CHARS`] and [`MAX_NAME_CHARS`]), and a value past its cap
/// arrives as [`None`] rather than as a shortened one.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AccountEntry {
    /// The account uid this row belongs to. A value the launcher wrote as
    /// something other than a string, or one past [`MAX_CUID_CHARS`],
    /// arrives as [`None`] rather than as a shortened one.
    pub cuid: Option<String>,
    /// The login nickname, under the same rule: a value past
    /// [`MAX_NAME_CHARS`] arrives as [`None`], never shortened.
    pub third_nickname: Option<String>,
}

/// The launcher account cache, as far as this program reads it: which
/// account the launcher last logged in as, and what it calls the accounts it
/// knows about.
///
/// Nothing here is session truth. The marker only refreshes on a
/// logout/login cycle, so it can name an account other than the one a
/// running game is playing: the launcher may have logged into a second one
/// since. Treat it as a hint a human reads, never as an answer.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LauncherCache {
    /// `last_login_cuid`, if it is a usable one: absent, empty, past
    /// [`MAX_CUID_CHARS`] or not a string all mean no marker at all.
    /// Compare it for equality and nothing else — never display it, never
    /// log it, never match presence on it.
    pub last_login_cuid: Option<String>,
    /// `account_list`, if the launcher wrote a list there. At most
    /// [`MAX_ROWS`] entries are kept, the first ones in the file, and an
    /// entry carrying neither half of a nickname pair is not an account and
    /// is left out.
    pub accounts: Option<Vec<AccountEntry>>,
}

impl LauncherCache {
    /// Every account here that has both a uid and a nickname, borrowed from
    /// the cache: what a listing looks a uid up in. An entry missing either
    /// half, or carrying an empty one, contributes nothing — and these
    /// fields are public, so this accessor is what decides what "usable"
    /// means for a cache it did not parse. A value past its cap is already
    /// dropped from [`Self::accounts`] and cannot appear at all.
    ///
    /// Later entries win over earlier ones for the same uid, matching what
    /// building this map by insertion has always done.
    pub fn nicknames(&self) -> HashMap<&str, &str> {
        self.accounts
            .iter()
            .flatten()
            .filter_map(|account| {
                let (cuid, nickname) =
                    (account.cuid.as_deref()?, account.third_nickname.as_deref()?);
                (!cuid.is_empty() && !nickname.is_empty()).then_some((cuid, nickname))
            })
            .collect()
    }

    /// Drop everything the caps forbid — a value past its own length cap
    /// becomes absent — and the marker when it is empty. Dropping a value
    /// and truncating a list are different acts: see [`MAX_NAME_CHARS`] for
    /// this one and [`MAX_ROWS`] for the other, which the parse applies
    /// rather than here, because an entry past it is never built.
    fn apply_caps(&mut self) {
        // An empty marker is no marker: it can only ever compare unequal to a
        // uid, and keeping it would put an unusable value in a field whose
        // whole job is to be compared for equality.
        self.last_login_cuid = self
            .last_login_cuid
            .take()
            .filter(|cuid| !cuid.is_empty() && within(cuid, MAX_CUID_CHARS));
        let Some(accounts) = &mut self.accounts else {
            return;
        };
        for account in accounts.iter_mut() {
            account.cuid = account
                .cuid
                .take()
                .filter(|cuid| within(cuid, MAX_CUID_CHARS));
            account.third_nickname = account
                .third_nickname
                .take()
                .filter(|nickname| within(nickname, MAX_NAME_CHARS));
        }
        accounts.retain(|account| account.cuid.is_some() || account.third_nickname.is_some());
    }
}

/// Read the launcher account cache at `path`: one bounded read, one parse,
/// and the two answers a listing needs out of the whole file.
///
/// This takes whatever path a caller hands it, so the byte ceiling is
/// enforced on the read itself rather than left to whoever copies the file:
/// the read stops one byte past [`MAX_JSON_BYTES`], refusing an oversized
/// file without ever holding it whole and without trusting the size the
/// filesystem reports. A file that is not there, and bytes that are not
/// JSON, are refused the same way — never half-read. Every value that does
/// come back is bounded: at most [`MAX_ROWS`] accounts, at most
/// [`MAX_NAME_CHARS`] characters of nickname, at most [`MAX_CUID_CHARS`] of
/// cuid.
///
/// # Errors
///
/// A lowercase [`String`] saying why the cache was refused: it could not be
/// opened, it could not be read, it is past the byte ceiling, or its bytes
/// are not JSON. The message carries the reason and the ceiling, never an
/// account value.
pub fn read_launcher_cache(path: &Path) -> Result<LauncherCache, String> {
    // One byte past the ceiling is how the read itself learns the file is
    // too big: the size the filesystem reports is not trusted, and the
    // allocation stops at the ceiling instead of growing with the file.
    let mut file =
        fs::File::open(path).map_err(|err| format!("cannot open the launcher cache: {err}"))?;
    let mut body = Vec::new();
    (&mut file)
        .take(MAX_JSON_BYTES.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|err| format!("cannot read the launcher cache: {err}"))?;
    if u64::try_from(body.len()).unwrap_or(u64::MAX) > MAX_JSON_BYTES {
        return Err(format!(
            "launcher cache is past the {MAX_JSON_BYTES}-byte ceiling"
        ));
    }
    let mut cache: LauncherCache = serde_json::from_slice(&body)
        .map_err(|err| format!("launcher cache is not readable JSON: {err}"))?;
    cache.apply_caps();
    Ok(cache)
}

/// Whether a value fits a cap measured in characters, not bytes: a cap this
/// program counts in characters has to count the same way everywhere.
fn within(value: &str, max_chars: usize) -> bool {
    value.chars().count() <= max_chars
}

// The deserializers below are written out rather than derived, and the
// reason is the launcher's file, not style. A derived `Deserialize` refuses
// a document, a list or a value whose JSON type is not the one it expects,
// and this file is written by an external launcher: every one of those
// refusals has to come back as an absent field instead, because that is the
// answer a listing already got when the same file was walked as untyped
// values. Handling each shape means dispatching on it, which a derive
// cannot express. What a derive would have given for free — ignoring the
// keys nobody asked for — is [`IgnoredAny`] below, which consumes those
// bytes without building a value for them either.

// A text field: the string it holds, or nothing at all for every other JSON
// type. `null`, a number, an object: all the same answer, and the same one a
// missing key gives.
struct Text(Option<String>);

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(TextVisitor)
    }
}

struct TextVisitor;

impl TextVisitor {
    /// The answer for a value that carries no text. Containers are drained
    /// rather than dropped, so whatever follows them still parses.
    fn none<E: de::Error>(self) -> Result<Text, E> {
        Ok(Text(None))
    }
}

impl<'de> Visitor<'de> for TextVisitor {
    type Value = Text;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string, or nothing at all")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Text, E> {
        Ok(Text(Some(value.to_owned())))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Text, E> {
        Ok(Text(Some(value)))
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Text, E> {
        self.none()
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Text, E> {
        self.none()
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Text, E> {
        self.none()
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Text, E> {
        self.none()
    }

    fn visit_none<E: de::Error>(self) -> Result<Text, E> {
        self.none()
    }

    fn visit_unit<E: de::Error>(self) -> Result<Text, E> {
        self.none()
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Text, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        self.none()
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Text, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        self.none()
    }
}

// The `account_list` value: the accounts it holds, or nothing when the
// launcher wrote something other than a list there.
struct Accounts(Option<Vec<AccountEntry>>);

impl<'de> Deserialize<'de> for Accounts {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(AccountsVisitor)
    }
}

struct AccountsVisitor;

impl AccountsVisitor {
    /// The answer for a value that holds no accounts. Containers are drained
    /// rather than dropped, so whatever follows them still parses.
    fn none<E: de::Error>(self) -> Result<Accounts, E> {
        Ok(Accounts(None))
    }
}

impl<'de> Visitor<'de> for AccountsVisitor {
    type Value = Accounts;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a list of account objects")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Accounts, A::Error> {
        let mut accounts = Vec::new();
        while let Some(account) = seq.next_element::<AccountEntry>()? {
            accounts.push(account);
            if accounts.len() == MAX_ROWS {
                break;
            }
        }
        // Past the cap the entries are consumed but not built: a JSON parser
        // has to consume a whole list to know where it ends, and an account
        // that can never be returned is not worth building.
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Accounts(Some(accounts)))
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Accounts, E> {
        self.none()
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Accounts, E> {
        self.none()
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Accounts, E> {
        self.none()
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Accounts, E> {
        self.none()
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Accounts, E> {
        self.none()
    }

    fn visit_none<E: de::Error>(self) -> Result<Accounts, E> {
        self.none()
    }

    fn visit_unit<E: de::Error>(self) -> Result<Accounts, E> {
        self.none()
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Accounts, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        self.none()
    }
}

impl<'de> Deserialize<'de> for AccountEntry {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(AccountVisitor)
    }
}

struct AccountVisitor;

impl AccountVisitor {
    /// The answer for a value that is not an account: an object is an
    /// account, everything else in the list is skipped like an account whose
    /// two fields are missing. Containers are drained rather than dropped,
    /// so whatever follows them still parses.
    fn none<E: de::Error>(self) -> Result<AccountEntry, E> {
        Ok(AccountEntry::default())
    }
}

impl<'de> Visitor<'de> for AccountVisitor {
    type Value = AccountEntry;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an account object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<AccountEntry, A::Error> {
        let mut account = AccountEntry::default();
        while let Some(key) = map.next_key::<Cow<'de, str>>()? {
            match key.as_ref() {
                "cuid" => account.cuid = map.next_value::<Text>()?.0,
                "thirdNickName" => account.third_nickname = map.next_value::<Text>()?.0,
                // The account's e-mail address, its token and its
                // verification codes live under keys like these. Consuming one
                // builds no value for it, so there is nothing here to log,
                // cache or leak.
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(account)
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<AccountEntry, E> {
        self.none()
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<AccountEntry, E> {
        self.none()
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<AccountEntry, E> {
        self.none()
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<AccountEntry, E> {
        self.none()
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<AccountEntry, E> {
        self.none()
    }

    fn visit_none<E: de::Error>(self) -> Result<AccountEntry, E> {
        self.none()
    }

    fn visit_unit<E: de::Error>(self) -> Result<AccountEntry, E> {
        self.none()
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<AccountEntry, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        self.none()
    }
}

impl<'de> Deserialize<'de> for LauncherCache {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(CacheVisitor)
    }
}

struct CacheVisitor;

impl CacheVisitor {
    /// The answer for a document that is not an object: it describes no
    /// accounts and no marker. Containers are drained rather than dropped,
    /// so whatever follows them still parses.
    fn empty<E: de::Error>(self) -> Result<LauncherCache, E> {
        Ok(LauncherCache::default())
    }
}

impl<'de> Visitor<'de> for CacheVisitor {
    type Value = LauncherCache;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a launcher account cache object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<LauncherCache, A::Error> {
        let mut cache = LauncherCache::default();
        while let Some(key) = map.next_key::<Cow<'de, str>>()? {
            match key.as_ref() {
                "last_login_cuid" => cache.last_login_cuid = map.next_value::<Text>()?.0,
                "account_list" => cache.accounts = map.next_value::<Accounts>()?.0,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(cache)
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<LauncherCache, E> {
        self.empty()
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<LauncherCache, E> {
        self.empty()
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<LauncherCache, E> {
        self.empty()
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<LauncherCache, E> {
        self.empty()
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<LauncherCache, E> {
        self.empty()
    }

    fn visit_none<E: de::Error>(self) -> Result<LauncherCache, E> {
        self.empty()
    }

    fn visit_unit<E: de::Error>(self) -> Result<LauncherCache, E> {
        self.empty()
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<LauncherCache, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        self.empty()
    }
}

/// Game version from the device database (`DeviceSaved/DeviceStorage.db`,
/// key `PatchVersion`). Same copy-to-private-snapshot rules as the main
/// database, byte ceiling included; best-effort: any failure yields `None`
/// without affecting triage.
fn read_device_version(storage_dir: &Path, snap_db: &Path) -> Option<String> {
    let device_src = storage_dir.parent()?.join("DeviceSaved/DeviceStorage.db");
    if !device_src.is_file() {
        return None;
    }
    let snap_copy = snap_db.parent()?.join("device.db");
    // Nothing else bounds this file: a runaway device database must be
    // refused here, not copied into the snapshot directory byte by byte.
    // Replaced, never obeyed, like every snapshot in the runtime
    // directory: nothing ever empties that directory, so a copy a killed run
    // left behind would leave the version unreadable for the whole session.
    bounded_copy(&device_src, &snap_copy, MAX_DB_BYTES, DestMode::Overwrite).ok()?;
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
        // 32 bytes, not [`MAX_NAME_CHARS`]: this is a version string the
        // game's own writer produces, not a name from an external writer, and
        // its length is bounded by what that writer emits.
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

/// Fast corruption gate: catches torn copies the mid-copy mtime check may
/// miss.
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

/// The `(region, level)` pair of every account row of `SdkLevelData`, in
/// file order and without uids.
///
/// Rows this program cannot make sense of are skipped rather than failing
/// the whole parse, and `None` means the body is not an account map at
/// all.
///
/// This is `parse_sdk_accounts` with the uid dropped, for a caller that
/// needs a region and a level and never the uid. A `--list-accounts`
/// listing builds `StoredAccount` rows of its own from that same parse
/// rather than from this.
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
