// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! wwrpc library root: detection, LocalStorage reading, Discord IPC and
//! presence payloads. The binary (`main.rs`) is a thin CLI + loop on top.
//!
//! # How a tick flows
//!
//! ```text
//! detect (/proc scan) → db.refresh (RAM snapshot + triage + enrichment)
//!   → decide_publish (rich > named-static > static > silent)
//!   → ipc (send once, skip duplicates, heartbeat)
//! ```
//!
//! # Modules
//!
//! - [`character`]: `--character` icon keys (names only; art lives in the
//!   Discord app).
//! - [`db`]: copy-to-RAM `LocalStorage` reader with ordered triage;
//!   recovers the live identity from launcher-telemetry remnants when the
//!   level row is missing.
//! - [`detect`]: rsRPC engine with a WuWa-only database (cache → online →
//!   stale cache → bundled snapshot).
//! - [`ipc`]: minimal Discord Unix-socket client (`SET_ACTIVITY`/clear).
//! - [`lock`]: single-instance guard — one `flock` on `wwrpc.lock`, with the
//!   pid inside it kept for diagnostics only.
//! - [`log`]: levelled stderr logging (`--verbose`/`--quiet`, `WWRPC_*`).
//! - [`presence`]: pure payload builders + the publish-tier decision.
//! - [`runtime_dir`]: private runtime directory (0700 dirs, 0600 files,
//!   symlink refusal, RAII temp dirs) plus the bounded snapshot copy that
//!   fills it (plain files only, byte-capped).
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

#[cfg(test)]
mod tests;
