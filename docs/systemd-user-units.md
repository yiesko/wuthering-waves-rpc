# Systemd user units — complete guide (wwrpc)

Run `wwrpc` as a user service — no root needed — with every
environment variable, flag interaction, and operational gotcha in one
place.

## 1. System vs user units

|  | System unit | User unit |
|---|---|---|
| Path | `/usr/lib/systemd/system/`, `/etc/systemd/system/` | `~/.config/systemd/user/` |
| Needs root | yes | **no** |
| Lifetime | whole machine | your login session (or boot, with linger) |

All commands are identical, with `--user` in the middle:

```bash
systemctl --user status wwrpc.service
```

## 2. Anatomy of `wwrpc.service`

```ini
[Unit]
Description=Wuthering Waves Discord Rich Presence
After=graphical-session.target

[Service]
ExecStart=%h/.cargo/bin/wwrpc
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target   # start at login, always (no graphical session needed)
```

Handy specifiers: `%h` home, `%t` runtime dir (`/run/user/1000`),
`%u` username.

## 3. Complete environment reference

Flags win when both are given; `WWRPC_GAME_PAGE`-style toggles accept
`1`/`true`/`yes`/`on` (any case).

| Variable | Flag equivalent | Default | Meaning |
|---|---|---|---|
| `WWRPC_CHARACTER` | `--character <NAME>` | unset | Portrait icon: any name from `--list-characters`. Needs the matching art asset uploaded to the Discord app under the same lowercase name; unknown names warn and publish without an icon |
| `WWRPC_APP_ID` | `--app-id <ID>` | `1546176429048463360` | Discord application id. Custom icons need your own app: create one, upload the art, point here (flag wins, blank is unset) |
| `WWRPC_GAME_PAGE` | `--game-page` | off | Companion presence on the official app (`1247227126416146462`) for the in-Discord game page (same details/state, no custom art) |
| `WWRPC_GAME_PAGE_PORTRAITS` | `--game-page-portraits` | off | Companion shows the portrait via external URL (zero uploads). Needs `--game-page` + `--character` |
| `WWRPC_LOG` | — | unset | `debug`/`trace` (verbose on) or `quiet`/`off`/`none` (quiet on) |
| `WWRPC_VERBOSE` | `--verbose` | unset | `1` = per-tick debug logging |
| `WWRPC_QUIET` | `--quiet` | unset | `1` = only warnings and errors |

One-shot / diagnostic flags (no daemon needed, no env equivalent):

| Flag | Meaning |
|---|---|
| `--game-dir <DIR>` | Game install dir (auto-discovered: Steam, `~/Games`, Twintail data) |
| `--no-database` | No game-file reads; silence unless `--static-fallback` adds static text (`--list-accounts` still lists the database) |
| `--static-fallback` | Publish static "Exploring SOL-III" with no rich data (default off: stay silent so game detection owns the slot) |
| `--kuro-uid <UID>` | Match account by Kuro UID (auto-detected login otherwise; prefer `--account-index`) |
| `--account-index <N>` | Pin the Nth stored account (see `--list-accounts`; wins over `--kuro-uid`) |
| `--player-name <NAME>` | Free-text display name (`{name} • Union Level {level}`); never read from files |
| `--list-accounts` | List stored accounts (`N: Region, Union LEVEL`, no uids) and exit |
| `--list-characters` | List selectable character names and exit |
| `--print-activity` | Print this moment's activity JSON and exit (dry run: no IPC, no lock) |
| `--interval <SECS>` | Seconds between presence updates, min 5 (default 15) |
| `--detect-interval <SECS>` | Seconds between game-detection scans, min 1 (default 5) |

## 4. Drop-ins: configure without editing the unit

Instead of touching the `.service` file, create
`~/.config/systemd/user/wwrpc.service.d/override.conf`:

```ini
[Service]
Environment=WWRPC_CHARACTER=hsin
Environment=WWRPC_APP_ID=1546176429048463360
Environment=WWRPC_GAME_PAGE=1
Environment=WWRPC_GAME_PAGE_PORTRAITS=1
```

Only what is in the drop-in overrides/adds. Inspect the result with
`systemctl --user cat wwrpc.service`. Apply with:

```bash
systemctl --user daemon-reload   # ALWAYS after editing units or drop-ins
systemctl --user restart wwrpc.service
```

## 5. Lifecycle

```bash
systemctl --user daemon-reload        # ALWAYS after editing units or drop-ins
systemctl --user enable wwrpc.service # start at login
systemctl --user start|stop|restart wwrpc.service
systemctl --user status wwrpc.service # state + recent log lines
systemctl --user is-active wwrpc.service
systemctl --user cat wwrpc.service    # shows the unit + applied drop-ins
```

## 6. Logs (everything goes to the journal)

```bash
journalctl --user -u wwrpc.service -f              # live
journalctl --user -u wwrpc.service --since "10 min ago"
journalctl --user -u wwrpc.service -b              # since boot
```

Tiers in the log: `rich` (full data) → `named-static` / `static` →
`silent` (no data; leaves the slot to game detection). Tier changes log
once at `INFO`.

## 7. Surviving logout and reboot (`linger`)

```bash
loginctl show-user $USER | grep Linger   # must say Linger=yes
sudo loginctl enable-linger $USER        # if not (needs sudo, once)
```

With linger, `enable`d units boot even without a graphical login.

## 8. Updating the binary safely

The service executes the file in place — overwriting a running binary
fails with `Text file busy`. Always:

```bash
systemctl --user stop wwrpc.service
cp ./target/release/wwrpc ~/.cargo/bin/wwrpc
systemctl --user start wwrpc.service
systemctl --user status wwrpc.service
```

## 9. Known gotchas

- **SIGTERM is graceful**: `stop` flips the run flag — the loop exits
  through the normal shutdown path, clearing both presence slots
  (check the log for the clear lines).
- **Second copy refuses to start**: fail-fast on double-publish —
  diagnostics (`--list-accounts`, `--print-activity`) run before the
  lock on purpose.
- **No `--ignore-ids` needed anymore**: rsRPC ≥ 0.32.0 hands the slot
  over (generic shows → companion takes over → generic resumes on
  clear). Keep the ignore only to fully silence a slot.
- **Companion uses its own pid**: both slots stay up with distinct
  bridge identities and clear independently.
- **Portrait needs the lowercase key**: `--character Hsin` resolves,
  but the Discord art asset must be uploaded as `hsin`.

## 10. Creating a unit from scratch (recipe)

Second instance with another character preset (no binary copy — just
different args):

```ini
# ~/.config/systemd/user/wwrpc-alt.service
[Unit]
Description=wwrpc (alt character preset)
After=graphical-session.target

[Service]
ExecStart=%h/.cargo/bin/wwrpc --character Cartethyia --game-page --game-page-portraits
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
```

```bash
systemd-analyze --user verify wwrpc-alt.service   # validate syntax
systemctl --user daemon-reload
systemctl --user enable --now wwrpc-alt.service   # enable + start in one go
systemctl --user status wwrpc-alt.service
```

Rules of thumb: one concern per unit; `Type=exec` (default `simple`
works too) + `Restart=on-failure` for daemons; prefer
`Environment=`/drop-ins over wrapper scripts; secrets go in
`EnvironmentFile=` (mode `600`), never in the unit.
