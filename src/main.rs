// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! wwrpc binary: wait for Wuthering Waves, publish Rich Presence.
//!
//! Two polling tiers keep it light: a fast detection tick (default 5s,
//! `/proc` scan only) and a slower presence tick (default 15s: snapshot,
//! parse, send). Identical payloads are not resent except as a periodic
//! heartbeat. Default mode reads Union Level + Region from a private
//! copy of `LocalStorage.db` (the live game files are never SQLite-opened,
//! written or chmodded). `--no-database` disables all game-file access —
//! which is why it needs no game install either; [`wwrpc::needs_game_data`]
//! says which invocations concern the game's files at all, even when
//! `--no-database` leaves one of them nothing to read.
//!
//! The run itself lives in [`wwrpc::run`]; this file is the shell around
//! it: parse the flags, configure logging, catch Ctrl+C and turn the
//! outcome into an exit code.

use clap::Parser;

use wwrpc::Args;

fn main() {
    let args = Args::parse();
    wwrpc::log::configure(args.verbose, args.quiet);

    // The run needs a signal handler before it starts polling: Ctrl+C has
    // to reach the loop so presence is cleared on the way out.
    match wwrpc::install_signal_handler().and_then(|()| wwrpc::run(args)) {
        Ok(()) => {}
        Err(err) => {
            eprintln!("Error: {err}");
            std::process::exit(1);
        }
    }
}
