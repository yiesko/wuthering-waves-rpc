// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! wwrpc library root: detection, LocalStorage reading, Discord IPC and
//! presence payloads — plus [`run`], the program behind the CLI. The
//! binary is the shell around it.
//!
//! # How a tick flows
//!
//! ```text
//! detect (/proc scan) → db.refresh (private snapshot + triage + enrichment)
//!   → decide_publish (rich > named-static > static > silent)
//!   → ipc (send once, skip duplicates, heartbeat)
//! ```
//!
//! # Which runs read the game files
//!
//! [`needs_game_data`] says whether an invocation is about the game's files
//! at all, and [`run`] decides from that what it actually opens. The
//! character list and a `--no-database` run carrying no diagnostic flag read
//! nothing, so a wrong `--game-dir` must not be able to stop either.
//!
//! # Modules
//!
//! - [`character`]: `--character` icon keys; the art lives in the Discord app.
//! - [`db`]: copy-to-private-dir `LocalStorage` reader with ordered triage;
//!   recovers the live identity from launcher-telemetry remnants when the
//!   level row is missing.
//! - [`detect`]: rsRPC engine with a WuWa-only database (cache → online →
//!   stale cache → bundled snapshot).
//! - [`ipc`]: minimal Discord Unix-socket client (`SET_ACTIVITY`/clear).
//! - [`lock`]: single-instance guard — one `flock` on `wwrpc.lock`, its pid
//!   inside for diagnostics only.
//! - [`log`]: levelled stderr logging (`--verbose`/`--quiet`, `WWRPC_*`).
//! - [`presence`]: pure payload builders and the publish-tier decision.
//! - [`telemetry_cache`]: the signature-keyed cache behind the launcher
//!   telemetry lookups — one read per version of the file, misses included,
//!   and a read that failed retried rather than remembered.
//! - [`mod@runtime_dir`]: the private runtime directory (0700 dirs, 0600
//!   files, symlink refusal, RAII temp dirs) holding the daemon's snapshots
//!   and instance lock, plus the bounded copy that fills it (plain files
//!   only, byte-capped).
//!
//! Privacy rule across every module: account values (uids, names, levels)
//! never reach logs — only shapes (counts, booleans, ages).

pub mod character;
pub mod db;
pub mod detect;
pub mod ipc;
pub mod lock;
pub mod log;
pub mod presence;
pub mod runtime_dir;
pub mod telemetry_cache;

#[cfg(test)]
mod tests;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clap::Parser;

use crate::character::Character;
use crate::db::{DbReader, Refresh, StoredAccount};
use crate::detect::{Detector, process_start_millis};
use crate::ipc::IpcClient;
use crate::lock::SingleInstance;
use crate::presence::LiveContext;
use crate::runtime_dir::{PrivateTempDir, runtime_dir};

/// Default Discord application id: the community app carrying the logo
/// and portrait assets (see `# Character icons` in the README). Override
/// with `--app-id` / `WWRPC_APP_ID` for a personal app.
const DEFAULT_APP_ID: &str = "1546176429048463360";

/// Official Wuthering Waves application: publishing here links the
/// presence to the in-Discord game page (directory entry). Custom art
/// lives on the user's own app and will NOT render in this slot.
const OFFICIAL_APP_ID: &str = "1247227126416146462";

/// Sleep slice: Ctrl+C stays responsive without busy-waiting.
const SLEEP_SLICE: Duration = Duration::from_millis(250);

/// Whether the publishing loop should keep going. The signal handler
/// clears it and the loop checks it between ticks, so a signal ends the
/// run the same way an exhausted loop does — through the code that clears
/// our presence on the way out.
static RUNNING: AtomicBool = AtomicBool::new(true);

/// Command-line flags of one run.
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Wuthering Waves Discord Rich Presence for Linux"
)]
pub struct Args {
    /// Game install directory (auto-discovered when omitted: Steam,
    /// ~/Games, Twintail data; standalone layouts included).
    #[arg(long)]
    pub game_dir: Option<String>,

    /// Publish without reading the game's files: static presence with
    /// `--static-fallback`, silence without it. The one exception is
    /// `--list-accounts`, which exists to read the account database and does
    /// so even with this flag.
    #[arg(long)]
    pub no_database: bool,

    /// Publish static "Exploring SOL-III" text when there is no rich data.
    /// Default (off): stay silent so game detection (e.g. rsRPC) owns the
    /// slot instead of being shadowed by a static fallback.
    #[arg(long)]
    pub static_fallback: bool,

    /// Kuro Games UID, matched against the level database. Defaults to the
    /// auto-detected RecentlyLoginUID. Prefer `--account-index`: uids differ
    /// per login space and yours may simply have no level row.
    #[arg(long)]
    pub kuro_uid: Option<String>,

    /// Use the Nth stored account (see `--list-accounts`) instead of uid
    /// resolution. Wins over `--kuro-uid`.
    #[arg(long)]
    pub account_index: Option<usize>,

    /// Display name shown as `{name} • Union Level {level}` in details.
    /// Free text (your Rover name); never read from game files.
    #[arg(long)]
    pub player_name: Option<String>,

    /// Character icon (`small_image`): any name from `--list-characters`
    /// (e.g. `--character Cartethyia`). Needs the matching art asset
    /// uploaded to the Discord app under the same lowercase name; unknown
    /// names warn and publish without an icon. Also `WWRPC_CHARACTER`.
    #[arg(long)]
    pub character: Option<String>,

    /// Companion presence on the official game app for the in-Discord game
    /// page (same details/state, no custom art). rsRPC ≥ 0.32.0 hands that
    /// slot over by itself; use its `--ignore-ids` only to silence the slot
    /// outright, or with an older rsRPC that cannot hand it over. Also
    /// `WWRPC_GAME_PAGE=1`.
    #[arg(long)]
    pub game_page: bool,

    /// Companion slot shows the character portrait via external URL
    /// (zero uploads, renders anywhere). Needs `--game-page` and
    /// `--character`. Also `WWRPC_GAME_PAGE_PORTRAITS=1`.
    #[arg(long)]
    pub game_page_portraits: bool,

    /// List selectable character names (`Display (key)`) and exit.
    #[arg(long)]
    pub list_characters: bool,

    /// List stored accounts (`N: Region, Union LEVEL`) and exit. Uids are
    /// never printed: recognize yours by level.
    #[arg(long)]
    pub list_accounts: bool,

    /// Print the exact activity JSON that would be published right now and
    /// exit. Dry run: no IPC, no lock, snapshot in an isolated temp dir.
    #[arg(long)]
    pub print_activity: bool,

    /// Seconds between presence updates (minimum 5).
    #[arg(long, default_value_t = 15)]
    pub interval: u64,

    /// Seconds between game-detection scans (minimum 1).
    #[arg(long, default_value_t = 5)]
    pub detect_interval: u64,

    /// Discord application id. Custom icons need YOUR OWN app (see
    /// `# Character icons` in the README): create one, upload the art, and
    /// point here via `--app-id` or `WWRPC_APP_ID` (flag wins, blank is unset).
    #[arg(long, default_value = DEFAULT_APP_ID)]
    pub app_id: String,

    /// Verbose per-tick debug logging.
    #[arg(long, conflicts_with = "quiet")]
    pub verbose: bool,

    /// Only warnings and errors.
    #[arg(long)]
    pub quiet: bool,
}

/// Whether this invocation is about the game's files at all.
///
/// `--list-characters` is answered from this crate's own tables and
/// `--no-database` alone asks for no game data, so neither needs an
/// install. The two diagnostics do: `--list-accounts` lists the level
/// database and `--print-activity` reproduces what a run would publish
/// from it, so each names the game data even when `--no-database` takes
/// the reading away. [`run`] asks a second question on top of this one
/// before opening anything.
///
/// # Examples
///
/// ```
/// use clap::Parser;
/// use wwrpc::{Args, needs_game_data};
///
/// assert!(!needs_game_data(&Args::parse_from(["wwrpc", "--no-database"])));
/// assert!(needs_game_data(&Args::parse_from(["wwrpc", "--no-database", "--print-activity"])));
/// assert!(needs_game_data(&Args::parse_from(["wwrpc", "--list-accounts"])));
/// ```
pub fn needs_game_data(args: &Args) -> bool {
    !args.no_database || args.list_accounts || args.print_activity
}

/// Installs the handler that turns a termination signal into the end of a
/// run.
///
/// It covers SIGINT (Ctrl+C), SIGTERM and SIGHUP. SIGTERM is what
/// `systemctl --user stop` sends, so stopping the unit ends the run the
/// same graceful way an interactive Ctrl+C does. The handler clears the
/// run flag the publishing loop checks between ticks and does nothing
/// else: it publishes nothing and clears no presence itself, because the
/// loop is the only thing that knows which pid it published under.
///
/// Fails when the operating system refuses the signal registration, when
/// the thread that runs the handler cannot start, or when this process has
/// already registered a handler — a handler installed elsewhere is
/// replaced, but a second registration of our own is rejected.
pub fn install_signal_handler() -> Result<(), Box<dyn Error>> {
    ctrlc::set_handler(|| RUNNING.store(false, Ordering::SeqCst))?;
    Ok(())
}

/// One run: the diagnostics the flags ask for, then the publishing loop.
///
/// The order is the contract: `--list-characters` answers from built-in
/// data before anything is looked up, game files are resolved only for a
/// run that reads them, and the instance lock is taken before the
/// detector — a second copy must be refused before it starts scanning.
pub fn run(args: Args) -> Result<(), Box<dyn Error>> {
    if args.list_characters {
        print_characters();
        return Ok(());
    }

    let game_files = if reads_game_data(&args) {
        Some(resolve_game_files(&args)?)
    } else {
        None
    };

    if args.list_accounts {
        // A listing is a read of the database by definition, so the gate
        // above resolved it for this run — whatever else shares the
        // command line with it. That leaves the fallback below unreachable,
        // and it stays as a reported error rather than a panic: a gate that
        // ever stops resolving a listing has to say so on its own. The
        // invariant behind it is pinned in `src/tests/gating.rs`.
        let (game_dir, storage_dir) = game_files
            .as_ref()
            .ok_or("--list-accounts reached without the game files its gate resolved")?;
        return print_accounts(game_dir, storage_dir);
    }

    let character = resolve_character(character_input(args.character.as_deref()).as_deref());
    let game_page = env_flag(args.game_page, "WWRPC_GAME_PAGE");
    let game_page_portraits = env_flag(args.game_page_portraits, "WWRPC_GAME_PAGE_PORTRAITS");
    let companion_portrait = companion_portrait(game_page, game_page_portraits, character.as_ref());

    if args.print_activity {
        return print_activity(
            &args,
            game_files.as_ref(),
            character.as_ref(),
            game_page,
            companion_portrait.as_deref(),
        );
    }

    // One runtime directory for the session: the instance lock lives in it
    // and the per-tick snapshots are written to it.
    let runtime = runtime_dir()?;
    let _instance = SingleInstance::acquire(&runtime).map_err(|err| format!("[wwrpc] {err}"))?;

    let detector = Detector::new()?;
    let db = match game_files {
        Some((game_dir, storage_dir)) => {
            Some(open_reader(&game_dir, &storage_dir, &args, &runtime)?)
        }
        None => None,
    };

    publish_loop(args, detector, db, character, game_page, companion_portrait)
}

/// Whether this run reads the game's files, as opposed to merely being
/// about them ([`needs_game_data`]).
///
/// The two differ for one invocation only: a dry run under
/// `--no-database` is about the game's data — it prints what this run
/// would publish — and reads none of it, so it must not so much as stat
/// the install roots looking for a directory it will never open. A
/// listing is not such a case: asking for the stored accounts *is*
/// asking for the database, whatever else shares the command line.
fn reads_game_data(args: &Args) -> bool {
    needs_game_data(args) && !(args.no_database && args.print_activity && !args.list_accounts)
}

/// Locate the game's `LocalStorage` directory, from `--game-dir` or by
/// discovery. Fatal when it fails: a run that reads game data has nothing
/// to fall back on.
fn resolve_game_files(args: &Args) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let requested = args.game_dir.as_deref().map(Path::new);
    db::resolve_storage(requested, &db::default_roots())
}

/// Print every selectable character name, then how to use it. Built-in
/// data only: nothing here touches the game or the filesystem.
fn print_characters() {
    for character in crate::character::all() {
        println!("{} ({})", character.display, character.key);
    }
    println!("Upload each portrait to the Discord app under its lowercase key,");
    println!("then select it with --character <name>.");
}

/// Print the stored accounts, in file order, as `N: Region, Union LEVEL`.
/// Login nicknames are shown in parentheses when locatable, masked to at
/// most their two ends; uids never leave the reader.
fn print_accounts(game_dir: &Path, storage_dir: &Path) -> Result<(), Box<dyn Error>> {
    let accounts = DbReader::list_accounts(storage_dir, game_dir)?;
    if accounts.is_empty() {
        println!("No stored accounts with level data.");
        return Ok(());
    }
    for account in &accounts {
        println!("{}", account_line(account));
    }
    println!("Nicknames in () are login nicknames, may be outdated, and are shown");
    println!("masked to at most their first and last character; recognize yours by");
    println!("region/level, then pin it with --account-index N.");
    Ok(())
}

/// One stored account rendered for the listing: `N: Region, Union LEVEL`,
/// the login nickname in parentheses when the row has one, and the
/// last-login marker when the row carries it.
///
/// What this guarantees is the contract of a printed row. The index
/// leads, because it is the value the reader types into
/// `--account-index N`. A nickname reaches the line only through
/// [`mask_name`], so a row carries the name in full only where the mask
/// hides nothing but stars — a name the mask's own marker already spells.
/// A row with no nickname carries no parentheses at all.
///
/// It formats and nothing more: it reads no file, opens no socket, and the
/// caller decides what becomes of the string — here, one line of a listing
/// the reader asked for by running the command that produces it.
fn account_line(account: &StoredAccount) -> String {
    let marker = if account.last_login {
        " [last launcher login]"
    } else {
        ""
    };
    match account.nickname.as_deref() {
        // Login nickname: recognition aid, may be outdated — never used
        // for matching, and printed masked to at most its first and last
        // character.
        Some(nickname) => format!(
            "{}: {}, Union {} (login nickname: {}?){marker}",
            account.index,
            account.region,
            account.level,
            mask_name(nickname)
        ),
        None => format!(
            "{}: {}, Union {}{marker}",
            account.index, account.region, account.level
        ),
    }
}

/// Mask a name for printing: first character, last character, and one `*`
/// for every character between.
///
/// A listing is meant to be pasted into a bug report, so the whole name
/// does not have to be what lands on the screen: the two ends recognise
/// a name the reader already knows, and carry nothing else.
///
/// A name of one or two characters keeps neither end — masking both
/// would print it in full — so it is starred out entirely. An empty name
/// stays empty.
///
/// Characters are counted, never bytes, so a name written in any script
/// keeps its first and last character instead of being cut mid-letter.
///
/// What this does not hide: the name's length, which the mask matches
/// character for character, and the name itself for anyone who has it.
fn mask_name(name: &str) -> String {
    let characters = name.chars().count();
    if characters <= 2 {
        return "*".repeat(characters);
    }
    let mut masked = String::with_capacity(name.len());
    for (position, character) in name.chars().enumerate() {
        masked.push(if position == 0 || position + 1 == characters {
            character
        } else {
            '*'
        });
    }
    masked
}

/// Print the activity a tick would publish right now, then exit: a dry
/// run takes no instance lock, connects to nothing, and snapshots into a
/// scratch directory of its own instead of the daemon's runtime
/// directory. With `--no-database` no game file is read at all, so the
/// payload is built from nothing — the same payload the daemon would
/// publish on a static or silent tick.
fn print_activity(
    args: &Args,
    game_files: Option<&(PathBuf, PathBuf)>,
    character: Option<&Character>,
    game_page: bool,
    companion_portrait: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    // The scratch guard rides along with the reader: it is what keeps the
    // directory alive for as long as snapshots are taken from it.
    let mut reader = match game_files {
        Some((game_dir, storage_dir)) => {
            let scratch = PrivateTempDir::new("print")?;
            let mut reader = DbReader::new(
                storage_dir.clone(),
                scratch.path().to_path_buf(),
                args.kuro_uid.clone(),
            )?;
            reader.set_game_dir(game_dir.clone());
            if let Some(index) = args.account_index {
                reader.set_account_index(index);
            }
            Some((scratch, reader))
        }
        None => None,
    };

    // One refresh, exactly as a tick would do it, so a dry run reports the
    // data the daemon would publish with it. `Unavailable` reads as no
    // data at all, which is what the daemon sees on that tick too.
    let refreshed = reader.as_mut().map(|(_, reader)| reader.refresh());
    let info = match refreshed {
        Some(Refresh::Unavailable) | None => None,
        Some(Refresh::Updated | Refresh::Unchanged) => {
            reader.as_ref().and_then(|(_, reader)| reader.last_info())
        }
    };
    let live = match &reader {
        Some((_, reader)) => LiveContext {
            uid_known: reader.live_uid().is_some(),
            display_name: reader.display_name(),
            version: reader.cached_version(),
        },
        None => LiveContext {
            uid_known: false,
            display_name: None,
            version: None,
        },
    };
    let publish = presence::decide_publish(
        info,
        args.static_fallback,
        presence::now_millis(),
        args.player_name.as_deref(),
        character,
        live,
    );
    match publish.into_activity() {
        Some(activity) => {
            println!("{}", serde_json::to_string_pretty(&activity)?);
            if game_page
                && let (Some(details), state) = (
                    activity.get("details").and_then(|v| v.as_str()),
                    activity.get("state").and_then(|v| v.as_str()).unwrap_or(""),
                )
            {
                println!("--- official companion ---");
                println!(
                    "{}",
                    serde_json::to_string_pretty(&presence::official_activity(
                        presence::now_millis(),
                        details,
                        state,
                        companion_portrait
                    ))?
                );
            }
        }
        None => println!("(silent: no rich data — game detection would own the slot)"),
    }
    Ok(())
}

/// Open the polling reader for a run that reads game data, validating the
/// account pin once so a torn startup copy cannot leave the pin
/// permanently off. Advisory only: the per-tick triage retries anyway.
fn open_reader(
    game_dir: &Path,
    storage_dir: &Path,
    args: &Args,
    snapshot_dir: &Path,
) -> Result<DbReader, Box<dyn Error>> {
    if let Some(index) = args.account_index {
        match DbReader::list_accounts(storage_dir, game_dir) {
            Ok(accounts) if index == 0 || index > accounts.len() => {
                log::warn(format!(
                    "account index {index} out of range ({} stored)",
                    accounts.len()
                ));
            }
            Err(err) => {
                log::warn(format!(
                    "account pre-validation skipped ({err}); retrying per tick"
                ));
            }
            _ => {}
        }
    }
    let mut reader = DbReader::new(
        storage_dir.to_path_buf(),
        snapshot_dir.to_path_buf(),
        args.kuro_uid.clone(),
    )?;
    reader.set_game_dir(game_dir.to_path_buf());
    if let Some(index) = args.account_index {
        reader.set_account_index(index);
    }
    Ok(reader)
}

/// The publishing loop: one detection tick per pass, with a presence
/// update on top of it, until Ctrl+C asks it to stop.
fn publish_loop(
    args: Args,
    detector: Detector,
    mut db: Option<DbReader>,
    character: Option<Character>,
    game_page: bool,
    companion_portrait: Option<String>,
) -> Result<(), Box<dyn Error>> {
    // `--app-id` wins, `WWRPC_APP_ID` fills in, default app otherwise.
    // (clap always fills the default, so "flag was passed" is detected by
    // comparing against it; blank env is unset.)
    let app_id = if args.app_id != DEFAULT_APP_ID {
        args.app_id.clone()
    } else {
        std::env::var("WWRPC_APP_ID")
            .ok()
            .map(|env| env.trim().to_string())
            .filter(|env| !env.is_empty())
            .unwrap_or_else(|| args.app_id.clone())
    };
    let mut ipc = IpcClient::new(app_id);
    // Companion slot on the official app (game page linkage). Separate
    // connection + session from the primary slot.
    let mut ipc_official = game_page.then(|| IpcClient::new(OFFICIAL_APP_ID.to_string()));
    // The companion MUST NOT reuse the game pid: the bridge keys presence
    // by socketId = pid, so two connections sharing one pid overwrite each
    // other and only the last-published slot displays. Our own pid is
    // stable for the session, nonzero (so clears still count as genuine),
    // and can never collide with the game pid.
    let official_pid = std::process::id() as u64;
    let presence_every = Duration::from_secs(args.interval.max(5));
    let detect_every = Duration::from_secs(args.detect_interval.max(1));

    let mut session: Option<Session> = None;
    let mut official_session: Option<Session> = None;
    let mut last_pid: Option<u64> = None;
    let mut announced_waiting_game = false;
    let mut announced_waiting_discord = false;
    let mut announced_waiting_discord_official = false;
    // A lost connection is announced like a wait, but the two latches reset
    // on different evidence: a wait clears when the client connects or a game
    // session starts, a loss clears when a presence round trip comes back
    // acknowledged — the only thing in this loop that proves a reply can
    // still be read whole — and when a game session starts, because a fresh
    // game is a fresh chance to be told. That latch also gates the
    // "connected" line below, because a client reconnecting into a loss
    // nobody has acknowledged is not connected in any sense worth reporting,
    // and repeating the line every tick would bury the one warning that
    // matters. The price is one silent recovery per loss episode: the latch
    // is set by the first failed publish, and the publish that clears it
    // only runs after the reconnect it silences.
    let mut announced_lost_discord = false;
    let mut announced_lost_discord_official = false;

    while RUNNING.load(Ordering::SeqCst) {
        let game = match detector.detect() {
            Ok(game) => game,
            Err(err) => {
                log::warn(format!("detection error: {err}"));
                sleep_checked(detect_every);
                continue;
            }
        };

        let Some(game) = game else {
            if session.take().is_some() {
                log::info("game closed, clearing presence and waiting...");
                if let Some(pid) = last_pid {
                    ipc.clear(pid);
                    if let Some(official) = ipc_official.as_mut() {
                        official.clear(official_pid);
                    }
                }
                ipc.close();
                if let Some(official) = ipc_official.as_mut() {
                    official.close();
                }
                official_session = None;
            } else if !announced_waiting_game {
                log::info("waiting for Wuthering Waves...");
                announced_waiting_game = true;
            }
            sleep_checked(detect_every);
            continue;
        };
        announced_waiting_game = false;
        // A missing pid should not happen (detection always stamps one);
        // skipping the tick beats publishing pid 0.
        let Some(pid) = game.pid else {
            log::warn("detected game without pid; skipping tick");
            sleep_checked(detect_every);
            continue;
        };
        last_pid = Some(pid);

        // New pid (fresh launch or restart) starts a new session clock.
        let fresh = !matches!(&session, Some(active) if active.pid == pid);
        if fresh {
            if session.is_some() {
                log::info(format!("game restarted (pid {pid}), starting new session"));
            } else {
                log::info(format!("game detected (pid {pid}), starting session"));
            }
            announced_waiting_discord = false;
            announced_lost_discord = false;
            announced_lost_discord_official = false;
            session = Some(Session::new(pid));
            official_session = ipc_official.as_ref().map(|_| Session::new(pid));
        }
        let active = session.as_mut().expect("session just set");

        // Presence cadence runs on top of the faster detection tick.
        let due = active
            .last_send
            .is_none_or(|at| at.elapsed() >= presence_every);
        if !due {
            sleep_checked(detect_every);
            continue;
        }

        if !ipc.is_connected() {
            match ipc.connect() {
                Ok(()) => {
                    // Withheld while a loss is still unacknowledged: the socket is
                    // up, but no presence round trip has been acknowledged
                    // since the last one failed, and that is the only proof
                    // this loop has that a reply can still be read whole.
                    if !announced_lost_discord {
                        log::info("connected to Discord");
                    }
                    announced_waiting_discord = false;
                }
                Err(err) => {
                    if !announced_waiting_discord {
                        log::warn(format!("{err}"));
                        announced_waiting_discord = true;
                    }
                    sleep_checked(detect_every);
                    continue;
                }
            }
        }

        // Companion slot: same cadence, own connection. A failed companion
        // connect skips only this slot for the tick (never the primary).
        if let Some(official) = ipc_official.as_mut()
            && !official.is_connected()
        {
            match official.connect() {
                Ok(()) => {
                    if !announced_lost_discord_official {
                        log::info("connected to Discord (official slot)");
                    }
                    announced_waiting_discord_official = false;
                }
                Err(err) => {
                    if !announced_waiting_discord_official {
                        log::warn(format!("official slot: {err}"));
                        announced_waiting_discord_official = true;
                    }
                }
            }
        }

        let refresh = db.as_mut().map(|db| db.refresh());
        log::debug(format!(
            "presence tick: pid {pid}, refresh {refresh:?}, degraded {}",
            db.as_ref().is_some_and(|db| db.is_degraded())
        ));
        // Stale-but-validated data survives transient blips; only a clean
        // triage state (or no data ever) decides what publishes.
        let info = db.as_ref().and_then(|db| db.last_info());
        // A different account behind the data restarts the session clock.
        // Unknown stretches (None) never count as a switch.
        let current_uid = info.map(|info| info.uid.as_str());
        if presence::account_switched(active.uid.as_deref(), current_uid) {
            log::info("account switched; starting new session");
            active.start_ms = presence::now_millis();
            active.uid = current_uid.map(str::to_string);
            active.last_body = None;
            active.unchanged_streak = 0;
            active.last_send = None;
            active.silence_cleared = false;
            // Companion slot restarts with it (same clock, fresh payload).
            if let Some(session) = official_session.as_mut() {
                session.start_ms = active.start_ms;
                session.uid = active.uid.clone();
                session.last_body = None;
                session.unchanged_streak = 0;
                session.last_send = None;
                session.silence_cleared = false;
            }
        } else if active.uid.is_none() {
            active.uid = current_uid.map(str::to_string);
        }
        // Every source complements the others: session identity (live login),
        // display name (explicit flag, else live-resolved), version (cached).
        let live = LiveContext {
            uid_known: db.as_ref().and_then(|db| db.live_uid()).is_some(),
            display_name: db.as_ref().and_then(|db| db.display_name()),
            version: db.as_ref().and_then(|db| db.cached_version()),
        };
        let publish = presence::decide_publish(
            info,
            args.static_fallback,
            active.start_ms,
            args.player_name.as_deref(),
            character.as_ref(),
            live,
        );
        // What the tick decided and from what: expected inputs (level data,
        // name, known login, static flag, character icon) next to the chosen
        // tier, so a silent stretch explains itself instead of just happening.
        let tier = tier_name(&publish);
        let named = args
            .player_name
            .as_deref()
            .or(live.display_name)
            .or(info.and_then(|info| info.display_name.as_deref()))
            .is_some();
        log::debug(format!(
            "publish tier: {tier} (level data: {}, name: {}, login known: {}, static flag: {}, character: {})",
            info.is_some(),
            named,
            live.uid_known,
            args.static_fallback,
            character.as_ref().map(|c| c.key).unwrap_or("none")
        ));
        if active.last_tier != Some(tier) {
            log::info(format!(
                "presence tier: {} -> {tier}",
                active.last_tier.unwrap_or("none")
            ));
            active.last_tier = Some(tier);
        }
        let Some(activity) = publish.into_activity() else {
            // Nothing rich to say: clear once and stay silent so game detection
            // (e.g. rsRPC) owns the slot instead of a static fallback.
            if !active.silence_cleared {
                ipc.clear(pid);
                if let Some(session) = official_session.as_mut() {
                    if let Some(official) = ipc_official.as_mut() {
                        official.clear(official_pid);
                    }
                    session.silence_cleared = true;
                }
                active.silence_cleared = true;
                log::info("no rich data; leaving presence to game detection");
            }
            sleep_checked(detect_every);
            continue;
        };
        active.silence_cleared = false;
        // Companion slot follows the primary: same details/state (hence the
        // same tier decision), official app for the game-page linkage. No
        // custom art here by design — those keys only exist on our own app.
        if let (Some(session), Some(official)) = (official_session.as_mut(), ipc_official.as_mut())
        {
            session.silence_cleared = false;
            let details = activity
                .get("details")
                .and_then(|v| v.as_str())
                .unwrap_or("Wuthering Waves");
            let state = activity.get("state").and_then(|v| v.as_str()).unwrap_or("");
            let companion = presence::official_activity(
                active.start_ms,
                details,
                state,
                companion_portrait.as_deref(),
            );
            if let Ok(body) = serde_json::to_string(&companion) {
                if presence::should_resend(
                    session.last_body.as_deref(),
                    &body,
                    session.unchanged_streak,
                ) {
                    match official.set_activity(official_pid, companion) {
                        Ok(()) => {
                            log::debug(format!("official slot sent ({} bytes)", body.len()));
                            session.last_body = Some(body);
                            session.unchanged_streak = 0;
                            session.last_send = Some(Instant::now());
                            announced_lost_discord_official = false;
                        }
                        Err(_) => {
                            if !announced_lost_discord_official {
                                log::warn(
                                    "official slot: Discord connection lost, reconnecting...",
                                );
                                announced_lost_discord_official = true;
                            }
                            official.close();
                        }
                    }
                } else {
                    log::debug("official slot unchanged, skipping send");
                    session.unchanged_streak += 1;
                    session.last_send = Some(Instant::now());
                }
            }
        }
        let body = serde_json::to_string(&activity)?;
        if presence::should_resend(active.last_body.as_deref(), &body, active.unchanged_streak) {
            match ipc.set_activity(pid, activity) {
                Ok(()) => {
                    log::debug(format!("presence sent ({} bytes)", body.len()));
                    active.last_body = Some(body);
                    active.unchanged_streak = 0;
                    active.last_send = Some(Instant::now());
                    active.silence_cleared = false;
                    // An acknowledged publish is the proof that ends a loss:
                    // the socket both wrote and read a whole frame again.
                    announced_lost_discord = false;
                }
                Err(_) => {
                    if !announced_lost_discord {
                        log::warn("Discord connection lost, reconnecting...");
                        announced_lost_discord = true;
                    }
                    ipc.close();
                }
            }
        } else {
            log::debug("presence unchanged, skipping send");
            active.unchanged_streak += 1;
            active.last_send = Some(Instant::now());
        }

        sleep_checked(detect_every);
    }

    if let Some(pid) = last_pid {
        ipc.clear(pid);
        if let Some(official) = ipc_official.as_mut() {
            official.clear(official_pid);
        }
    }
    Ok(())
}

/// One game run: pid, session clock, and what was last published.
#[derive(Debug)]
struct Session {
    pid: u64,
    start_ms: i64,
    /// Account uid behind the current data, to restart the clock on switch.
    uid: Option<String>,
    last_body: Option<String>,
    unchanged_streak: u32,
    last_send: Option<Instant>,
    /// A clear was already sent for the current silent stretch.
    silence_cleared: bool,
    /// Last publish tier, to announce tier changes once (INFO).
    last_tier: Option<&'static str>,
}

impl Session {
    fn new(pid: u64) -> Self {
        // Anchor the clock to the real launch when readable; the tick we
        // noticed it is only the fallback.
        let start_ms = process_start_millis(pid).unwrap_or_else(presence::now_millis);
        Self {
            pid,
            start_ms,
            uid: None,
            last_body: None,
            unchanged_streak: 0,
            last_send: None,
            silence_cleared: false,
            last_tier: None,
        }
    }
}

/// Resolve the character icon: `--character` wins, `WWRPC_CHARACTER`
/// fills in when the flag is absent; blank means unset. Unknown names
/// warn (with the input echoed — it is the operator's own flag, not game
/// data) and publish without an icon instead of failing.
fn resolve_character(input: Option<&str>) -> Option<Character> {
    let input = input.map(str::trim).filter(|input| !input.is_empty())?;
    match crate::character::find(input) {
        Some(character) => Some(character),
        None => {
            log::warn(format!(
                "unknown character '{input}'; run --list-characters for valid names, publishing without an icon"
            ));
            None
        }
    }
}

/// `--character` wins, `WWRPC_CHARACTER` fills in when the flag is
/// absent; blank means unset.
fn character_input(flag: Option<&str>) -> Option<String> {
    flag.map(str::trim)
        .filter(|flag| !flag.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("WWRPC_CHARACTER")
                .ok()
                .map(|env| env.trim().to_string())
                .filter(|env| !env.is_empty())
        })
}

/// Portrait URL for the companion slot (external art, zero uploads).
/// Needs the companion slot itself and a character to show.
fn companion_portrait(
    game_page: bool,
    game_page_portraits: bool,
    character: Option<&Character>,
) -> Option<String> {
    if !game_page_portraits {
        return None;
    }
    match (character, game_page) {
        (Some(character), true) => Some(crate::character::portrait_url(character)),
        (None, _) => {
            log::warn("--game-page-portraits needs --character; companion stays imageless");
            None
        }
        (_, false) => {
            log::warn("--game-page-portraits needs --game-page; ignoring");
            None
        }
    }
}

/// Truthy env (`1/true/yes/on`, any case): `--flag` wins, env fills in.
fn env_flag(flag: bool, name: &str) -> bool {
    flag || std::env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Short tier name for logging: what a tick decided, in one word.
fn tier_name(publish: &presence::Publish) -> &'static str {
    match publish {
        presence::Publish::Rich(_) => "rich",
        presence::Publish::NamedStatic(_) => "named-static",
        presence::Publish::Static(_) => "static",
        presence::Publish::Silent => "silent",
    }
}

/// Sleep in small slices so Ctrl+C stays responsive.
fn sleep_checked(total: Duration) {
    let mut waited = Duration::ZERO;
    while RUNNING.load(Ordering::SeqCst) && waited < total {
        std::thread::sleep(SLEEP_SLICE.min(total - waited));
        waited += SLEEP_SLICE;
    }
}
