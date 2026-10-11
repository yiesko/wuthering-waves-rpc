# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- CodeQL workflow: weekly and per-change static analysis of the Rust
  source. CodeQL reads the code directly, so it adds no build step.
- Dependabot: weekly pull requests for crates.io dependencies and
  GitHub Actions, with minor and patch updates grouped per dependency.
- rsRPC pin check: a weekly job that compares the tag in `Cargo.toml`
  with the newest tag upstream and opens an issue when they drift.
  Dependabot cannot do this — it does not version-bump git
  dependencies — and it also reports a tag that has disappeared.

### Fixed
- Fresh clones and clean CI runs could not build. The `v0.32.0` tag was
  never published upstream, so the dependency could not be resolved;
  local builds only worked because an old copy sat in the cargo cache.

### Changed
- rsRPC dependency moved from tag `v0.32.0` to `v0.38.0`, and from the
  single `rsrpc` crate to `rsrpc-core` plus `rsrpc-detect` after the
  upstream library split into a workspace. `RPCServer` became `Daemon`;
  detection is still synchronous, so wwrpc still runs no threads and
  no async runtime of its own.
- Minimum supported Rust version raised to 1.95, matching rsRPC.

## [0.2.0] - 2026-09-08

### Added
- Game-page companion (`--game-page` / `WWRPC_GAME_PAGE=1`): second
  presence on the official game app (`1247227126416146462`), which
  Discord links to the in-client Wuthering Waves page — same
  details/state as the primary slot; `--print-activity` shows both.
- Companion portraits (`--game-page-portraits` /
  `WWRPC_GAME_PAGE_PORTRAITS=1`, needs `--game-page` + `--character`):
  external `https://` portrait as the companion's large image (zero
  uploads — Discord proxies it).
- Character roster growth (63 entries, incl. Hsin and Suoming);
  `--list-characters` covers them.

### Fixed
- Companion slot uses its own process pid instead of the game pid:
  both slots now display simultaneously (same pid used to overwrite
  one slot on pid-keyed bridges) and clear independently.

### Changed
- rsRPC dependency pinned to tag `v0.32.0` (was floating `main`):
  the game-page companion relies on its IPC-wins handoff.

[Unreleased]: https://github.com/yiesko/wwRPC/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/yiesko/wwRPC/releases/tag/v0.2.0
