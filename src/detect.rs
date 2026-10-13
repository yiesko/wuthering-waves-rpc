// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Game detection via the rsRPC engine.
//!
//! No hardcoded game data: whichever database wins at startup is trimmed to
//! the Wuthering Waves entry plus a local linux-executable override. The
//! override exists because a non-Steam launcher sets no Steam id at all; it
//! says which launchers it covers.
//!
//! Each tick then runs one synchronous `detect_once` scan. It reads `/proc`
//! for the processes, and it also refreshes rsRPC's Steam library map and
//! rewrites that map's cache, so a tick is not confined to `/proc`. No
//! threads and no ports.

use rsrpc_core::{Daemon, DetectedGame, RPCConfig};

use crate::log;

/// Wuthering Waves in Discord's detectable database.
pub const WUWA_ID: &str = "1247227126416146462";

const DB_URL: &str = "https://discord.com/api/v9/applications/detectable";
/// Bounded fetch: startup (and Ctrl+C handling) must never hang on network.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Cache file holding the already-selected WuWa entries (bytes, not megabytes).
const CACHE_FILE: &str = "detectable.json";
/// Revalidate online past this age; stale caches still serve as fallback.
const CACHE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Local detection override, merged into the database at startup.
///
/// The online entry ships no `linux` executable, and a non-Steam launcher
/// (Twintail, Heroic, Lutris, plain Wine/Proton) sets no `SteamAppId` in
/// `/proc/*/environ` — so without this, only Steam launches would ever
/// match. Matching is a mere path suffix (`client-win64-shipping.exe`, any
/// directory), which is identical across all launchers.
///
/// This is detection config, not game data: it adds one executable name and
/// no path, and takes its id from the constant the rest of the crate uses.
fn override_entry() -> serde_json::Value {
    serde_json::json!({
      "id": WUWA_ID,
      "name": "Wuthering Waves",
      "hook": false,
      "executables": [
        {"name": "client-win64-shipping.exe", "is_launcher": false, "os": "linux"}
      ]
    })
}

/// Keep only the WuWa entry of a detectable database, plus the local
/// override. Returns `None` when WuWa is absent (entry removed upstream).
pub(crate) fn select_wuwa(body: &str) -> Option<String> {
    let games: Vec<serde_json::Value> = serde_json::from_str(body).ok()?;
    let mut kept: Vec<serde_json::Value> = games
        .into_iter()
        .filter(|game| game.get("id").and_then(|id| id.as_str()) == Some(WUWA_ID))
        .collect();
    if kept.is_empty() {
        return None;
    }
    kept.push(override_entry());
    serde_json::to_string(&kept).ok()
}

/// Game detector: the rsRPC engine fed with a WuWa-only database.
/// Resolution order per construction: fresh disk cache (7 days) → online
/// fetch (refreshes the cache) → stale cache → bundled snapshot. Owns no
/// threads or sockets, and a tick reaches past `/proc` to rsRPC's Steam
/// library map and its cache; [`Detector::detect`] has the detail.
pub struct Detector {
    daemon: Daemon,
}

impl Detector {
    /// Build the engine (one-time cost: fetch/parse of the database, then
    /// arenas are trimmed). The steady state is kilobytes per tick.
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let cache_dir = default_cache_dir();
        // Fresh cache first: avoids the ~150 MB one-time parse on every boot.
        // Otherwise fetch online (and refresh the cache); a stale cache still
        // beats the bundled snapshot when offline, which is the last resort.
        let json = match load_cache(&cache_dir, true) {
            Some(json) => json,
            None => match fetch_online_db() {
                Ok(json) => {
                    store_cache(&cache_dir, &json);
                    json
                }
                Err(err) => match load_cache(&cache_dir, false) {
                    Some(json) => {
                        log::warn(format!(
                            "online database unreachable ({err}); using stale cache"
                        ));
                        json
                    }
                    None => {
                        log::warn(format!(
                            "online database unreachable ({err}); using offline snapshot"
                        ));
                        select_wuwa(rsrpc_detect::db::BUNDLED_DETECTABLE).ok_or(
                            "Wuthering Waves missing from the bundled database; update rsrpc?",
                        )?
                    }
                },
            },
        };
        let detector = Self {
            daemon: Daemon::from_json_str(json, RPCConfig::default())?,
        };
        // Return the one-time fetch/parse arenas to the OS: steady state is
        // kilobytes (one scan plus one snapshot copy at a time).
        // SAFETY: malloc_trim is always safe to call; it only hints the
        // allocator to release free pages.
        unsafe {
            libc::malloc_trim(0);
        }
        Ok(detector)
    }

    /// Returns the running game, if any. Synchronous, on the calling
    /// thread: it reads `/proc`, and it also refreshes and rewrites rsRPC's
    /// Steam library cache. It starts no thread, binds no port and opens no
    /// network connection.
    pub fn detect(&self) -> Result<Option<DetectedGame>, Box<dyn std::error::Error>> {
        Ok(self
            .daemon
            .detect_once()?
            .into_iter()
            .find(|game| game.id == WUWA_ID))
    }
}

/// Start time of a process as Unix millis, from `/proc/<pid>/stat`
/// (field 22, clock ticks since boot) plus the boot epoch from
/// `/proc/stat`. `None` when unreadable — callers fall back to
/// first-seen time. Anchors the session `start` to the real launch
/// instead of our detection tick.
pub fn process_start_millis(pid: u64) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name sits in parens and may itself contain parens or
    // spaces, so split after the LAST ')': fields then start at field 3.
    let after_comm = stat.rsplit(')').next()?;
    let start_ticks: i64 = after_comm.split_whitespace().nth(19)?.parse().ok()?;
    // SAFETY: sysconf with a valid name only reads a constant.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    let boot_epoch: i64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("btime ")
                .and_then(|value| value.trim().parse().ok())
        })?;
    Some((boot_epoch + start_ticks / hz) * 1000)
}

/// Fetch + trim the online database, keeping just the WuWa entry so the
/// per-tick automaton stays trivial.
fn fetch_online_db() -> Result<String, Box<dyn std::error::Error>> {
    // Bounded: startup (and Ctrl+C handling) must never hang on network.
    let body = ureq::get(DB_URL)
        .config()
        .timeout_global(Some(FETCH_TIMEOUT))
        .build()
        .call()?
        .into_body()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()?;
    let trimmed = rsrpc_detect::db::trim_detectable(&body).unwrap_or(body);
    select_wuwa(&trimmed).ok_or_else(|| "Wuthering Waves missing from the online database".into())
}

/// Cache directory for the selected entries (`$XDG_CACHE_HOME/wwrpc`,
/// plain `~/.cache` fallback). Cache files are ours to rewrite.
pub(crate) fn default_cache_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            std::path::PathBuf::from(home).join(".cache")
        });
    base.join("wwrpc")
}

/// Load the cached selection. With `fresh_only`, entries older than
/// [`CACHE_MAX_AGE`] are ignored (callers then revalidate online); the
/// stale read exists so offline boots still work. Corrupt caches are
/// ignored, never fatal.
pub(crate) fn load_cache(dir: &std::path::Path, fresh_only: bool) -> Option<String> {
    let path = dir.join(CACHE_FILE);
    if fresh_only {
        let age = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()?
            .elapsed()
            .ok()?;
        if age >= CACHE_MAX_AGE {
            return None;
        }
    }
    let body = std::fs::read_to_string(&path).ok()?;
    validate_cache(&body)
}

/// The cache must still look like our selection: a non-empty array whose
/// entries all carry the WuWa id.
fn validate_cache(body: &str) -> Option<String> {
    let games: Vec<serde_json::Value> = serde_json::from_str(body).ok()?;
    if games.is_empty()
        || games
            .iter()
            .any(|game| game.get("id").and_then(|id| id.as_str()) != Some(WUWA_ID))
    {
        return None;
    }
    Some(body.to_string())
}

/// Persist a selection atomically (temp file + rename), best effort: a
/// failed cache never fails startup.
pub(crate) fn store_cache(dir: &std::path::Path, json: &str) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = dir.join(format!("{CACHE_FILE}.tmp"));
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(CACHE_FILE));
    }
}
