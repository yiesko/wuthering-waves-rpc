<div align="center">
  <h1>wwrpc</h1>
  <p>Wuthering Waves Discord Rich Presence for Linux (Steam/Proton), in Rust</p>
  <p>
    <a href="https://github.com/yiesko/wwrpc/actions/workflows/ci.yml"><img src="https://github.com/yiesko/wwrpc/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  </p>
</div>

# Showcase

What your Discord profile looks like with wwrpc running:

![Static fallback presence](assets/screenshots/1.png)

*Static presence — display name plus `Exploring SOL-III` with session timer.*

![Rich presence with level and region](assets/screenshots/2.png)

*Rich presence — display name, `Lv. 65 | Region: America`, portrait icon and session timer.*

![Profile card with game-page linkage](assets/screenshots/3.png)

*Profile card — custom character as image, custom display name & etc.*

# Features

* **Real Rich Presence on Linux.** Display name (or
  `Union Level {level}`), `Lv. {n} | Region: {region}`, quest count and
  game version on icon hover, portrait art, and a session timer.
  (Details: How it works.)
* **63 character portraits** via `--character` (`--list-characters`
  lists them all, Rover variants included). Case-insensitive; unknown
  names publish iconless. (Details: Character icons.)
* **Game-page linkage** via `--game-page`: a companion slot on the
  official app links the status to the in-Discord WuWa page, with
  portraits via `--game-page-portraits` (zero uploads). (Details: Game
  page.)
* **Read-only and private.** Game files are only ever byte-copied to
  tmpfs and queried there; uids and account values never reach logs —
  or anywhere beyond Discord's local IPC. (Details: Safety, Data
  sources.)
* **Multi-account friendly.** The live login is re-resolved each tick,
  with `--account-index` / `--kuro-uid` pins; missing data degrades
  through tiers down to silence instead of a broken card. (Details: How
  it works.)
* **Coexists with rsRPC (my fork)** ≥ 0.32.0, which yields the official
  slot while the companion is up and resumes when it clears. (Details:
  Game page.)

# Install

Requirements: Cargo + Rust 1.88+ (edition 2024).

```bash
cargo install --path wwrpc   # from the repo root → ~/.cargo/bin/wwrpc
```

Autostart (systemd user service, no wizard):

```bash
mkdir -p ~/.config/systemd/user
cp wwrpc/assets/wwrpc.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now wwrpc.service
```

Per-user settings go in a drop-in
(`~/.config/systemd/user/wwrpc.service.d/override.conf`), e.g.:

```ini
[Service]
Environment=WWRPC_CHARACTER=augusta
Environment=WWRPC_APP_ID=1546176429048463360
```

# Usage

```bash
wwrpc [OPTIONS]
```

| Flag | Env | What it does |
| --- | --- | --- |
| `--game-dir <DIR>` | — | Game install dir (auto-discovered: Steam, `~/Games`, Twintail data) |
| `--no-database` | — | Static presence only; zero game-file access |
| `--static-fallback` | — | Static `Exploring SOL-III` when there is no rich data (default: stay silent) |
| `--kuro-uid <UID>` | — | Override auto-detected login (prefer `--account-index`) |
| `--account-index <N>` | — | Pin the Nth stored account (wins over `--kuro-uid`) |
| `--player-name <NAME>` | — | Free-text display name; always wins over resolved names |
| `--character <NAME>` | `WWRPC_CHARACTER` | Portrait icon (see Character icons) |
| `--game-page` | `WWRPC_GAME_PAGE=1` | Companion presence on the official app (see Game page) |
| `--game-page-portraits` | `WWRPC_GAME_PAGE_PORTRAITS=1` | Portrait via URL on the companion (needs both above) |
| `--list-accounts` | — | List stored accounts (`N: Region, Union LEVEL`, no uids) and exit |
| `--list-characters` | — | List valid character names and exit |
| `--print-activity` | — | Print this moment's activity JSON and exit (dry run: no IPC, no lock) |
| `--interval <S>` | — | Presence tick seconds, min 5 (default 15) |
| `--detect-interval <S>` | — | Detection scan seconds, min 1 (default 5) |
| `--app-id <ID>` | `WWRPC_APP_ID` | Discord application id (default below) |
| `--verbose` | `WWRPC_LOG=debug` / `WWRPC_VERBOSE=1` | Per-tick debug logging |
| `--quiet` | `WWRPC_LOG=quiet` / `WWRPC_QUIET=1` | Warnings and errors only |

Flag wins over env; blank env is unset. Default app id
`1546176429048463360` (community app with logo + portraits); use your own
app id for a personal setup (see Character icons).

# How it works

Every 15s (detection scans `/proc` every 5s):

```text
detect → snapshot + triage + enrichment → publish tier → send (dedup + heartbeat)
```

* **Identity first.** The live login is resolved each tick, independent
  of everything else. A role switch drops stale data at once; gaps change
  nothing. Explicit pins (`--account-index`, `--kuro-uid`) disable
  guessing.
* **Level, in order:** pinned account → override → auto login →
  `SdkLevelData` row → telemetry recovery (fresh <24h events matching the
  live role, newest wins).
* **Enrichment is independent:** display name, version and quests each
  ride along on their own — one missing source never sinks the rest.
* **Tiers:** rich level data → named identity (verified login + name) →
  static text (only with `--static-fallback`) → silence (clear once, let
  game detection own the slot).
* **Session clock** from process start time; restarts on pid change or
  account switch. Identical payloads are skipped (heartbeat every 12th
  tick). One instance only (kernel `flock` on a lock file, pid kept for
  diagnostics). Ctrl+C clears presence.

# Safety

Game files are **never opened as SQLite, never written, never chmodded,
never kept open**. Each tick byte-copies the database (+ journal) to
tmpfs, rejects the copy if the source changed mid-copy (`mtime`), queries
only the copy, then deletes it. The same copy-to-RAM rule covers the
telemetry, device and launcher-cache reads.

Background: the game abandons `LocalStorage.db` (spawning
`LocalStorage2.db`, … and resetting settings) if the file is held open
elsewhere — so a sentinel watches the folder and, if a numbered database
ever appears, wwrpc stops touching game files for the session and logs
loudly. Never set game files read-only; `--no-database` removes even the
copy step.

> Reading local game files may breach the game's ToS. Use at your own
> risk; not affiliated with Kuro Games.

# Data sources

Read from RAM copies only:

| Source | Provides |
| --- | --- |
| `RecentlyLoginUID` | live login identity (session truth, never logged) |
| `SdkLevelData` | `{Region, Level}` per login id |
| `UserFinishedQuests` | finished-quest count (ids never read) |
| `PatchVersion` (device DB) | game version hover bit |
| `KDData-data.db` live rows | current display name by exact `role_id` |
| `KDData-data.db` remnants | dated, role-matched login events → name+level+region when the level row is missing |
| `KRSDKUserCache.json` | login nicknames, recognition aid for `--list-accounts` only (mails/tokens/codes never touched) |

Snapshots are triaged in order (openable → schema → integrity → keys →
shapes → known uid); failures keep previous good data and log key names
only — values and uids never appear in logs.

Two id spaces exist with no local mapping: role ids (live login) and
login ids (level rows, launcher cache) — they differ per login method.

Deliberately unused: `TDData-data.db` (no live identity rows),
`LoginTime_*` (recency without level linkage), `SingleMapId` (no public
id→name map), `AreaExplorePlayState` (unverified semantics),
`Client.log` (would need reverse-engineering). Login nicknames are
first-login-sticky — recognition aid, never session truth.

# Character icons

`--character <name>` shows a portrait as `small_image`
(`Denia • v3.6.13`). Unknown names warn and publish iconless.

Portraits follow `ryanbenson/wuthering-waves-assets` (`images/*.png`)
by name only — nothing copied in (art belongs to Kuro Games). Discord
renders only art uploaded to the app in use: the default community app
(`1546176429048463360`) ships all 63 portraits plus the logo,
so every `--list-characters` name resolves out of the box. (No public
API lists an app's assets — verify in the portal: Rich Presence → Art
Assets should show 64 entries. A new upstream portrait needs two steps:
upload it under its lowercase key, and add its line to `character.rs`.)
Running your own app id instead? Upload the same keys there (steps
below) — a personal app also isolates you from community-app changes.

1. [Developer Portal](https://discord.com/developers/applications) →
   New Application → copy the Application ID.
2. Rich Presence → Art Assets → upload `wwrpc/assets/logo.png` as
   `logo` (512×512 original emblem, in this repo; MIT, © 2023 Dawid Szymaniak)
   and each portrait as its lowercase stem (`Denia.png` → `denia`;
   1024×1024 recommended, 512×512 minimum). Rover variants keep their
   suffix. Keys can't be edited after saving.
3. `--app-id <id>` or `WWRPC_APP_ID=<id>`. New art can take hours to
   appear; restart Discord to clear its icon cache.

Keep only character portraits (64 files with the logo): lowercase
stat/UI icons, echo-set names, legacy `Rover-*` dash files, skins and
`Unknown*` placeholders in `images/` are not selectable and stay out.
Upstream renames files occasionally (`RoverElectroFemale.png` →
`Roverelectrofemale.png`) — matching is case-insensitive, so renames
never break `--character`.

# Game page

Clicking the rich presence normally shows just the app. `--game-page`
(or `WWRPC_GAME_PAGE=1`) additionally publishes a companion presence on
the **official** game app (`1247227126416146462`), which Discord links
to the in-client Wuthering Waves page (info, art) — same details/state,
no custom art (official-app keys are unknown; a broken slot is worse
than none). One slot = one app, so both stay up: yours (icons, name,
level) + official (page linkage).

This needs the official slot free of *generic* publishers: rsRPC
≥ 0.32.0 handles that itself via IPC-wins handoff (its generic card
shows until this companion publishes, yields, and resumes if the
companion clears). With older rsRPC, silence the slot instead, e.g.
`RSRPC_IGNORE_IDS=1247227126416146462` (or `--ignore-ids`).
The companion uses its own process pid (never the game pid), so both
slots stay up with distinct bridge identities and clear independently.
The companion follows the primary: silent stretches and game close
clear both; `--print-activity` shows both payloads.

With `--game-page-portraits` (needs `--game-page` + `--character`), the
companion also carries the portrait as an external `https://` image
(zero uploads — Discord proxies it). Same trade, no portal step.

# Logging

`ERROR`/`WARN` always print; `INFO` unless quiet; `DEBUG` needs verbose.
Quiet wins on conflict. `WWRPC_LOG` also takes `trace` (=debug),
`info`/`warn`/`error` (=flags only), `off`/`none`/`silent` (=quiet);
`WWRPC_VERBOSE`/`WWRPC_QUIET` take `1`/`true`/`yes`/`on`.

Tier changes log at INFO with inputs (`level data?/name?/login
known?/static flag?/character?`); recoveries state what was complemented
vs still missing.

# Troubleshooting

* **Silent with game open** → `--verbose` for one tick: `telemetry scan:
  0 …` means no local data for the login — pin via `--account-index`.
* **`account has no level data yet`** → login known, level row missing;
  remnant recovery runs first, pinning is the explicit fallback.
* **Icon missing** → payload is fine if `--print-activity` shows
  `small_image`: check YOUR app id, exact lowercase key, propagation
  delay, Discord restart.
* **Second copy won't start** → by design (`flock` on the lock file);
  diagnostics never take the lock. On some filesystems the kernel refuses a
  lock for a moment after the previous instance released it, so a start can
  come back refused once with nothing running — just start it again.

# Testing

```bash
cargo test -p wwrpc                 # unit + integration + doctests
cargo clippy -p wwrpc --all-targets # zero warnings
cargo doc -p wwrpc --no-deps        # zero warnings
```

Fixtures are synthetic only (`111111111`, `TestRover`, …).

# Credits

* [yiesko/rsRPC](https://github.com/yiesko/rsRPC) — detection engine.
* [ryanbenson/wuthering-waves-assets](https://github.com/ryanbenson/wuthering-waves-assets) — portrait sources (referenced, never vendored).

# License

GPL-3.0-only, see [COPYING](COPYING).
