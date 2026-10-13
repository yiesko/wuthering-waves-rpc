// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Integration tests for the CLI gating: which invocations may start
//! without a game install, and which game files each one needs.
//!
//! The binary is run against a `--game-dir` that cannot exist, so these
//! tests mean the same thing on a machine with the game installed as on
//! one without it: resolving that directory always fails, and an
//! invocation that survives it is one that never needed the game's files
//! in the first place. A test that only relied on "nothing installed"
//! would quietly stop testing anything on a developer machine.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use clap::Parser;

use wwrpc::Args;

/// A game directory that cannot exist, whatever is installed here.
const MISSING_GAME_DIR: &str = "/nonexistent";

/// Variables that steer a run from outside the command line, so a dry
/// run's output depends only on the flags under test.
const STEERING: [&str; 4] = [
    "WWRPC_CHARACTER",
    "WWRPC_APP_ID",
    "WWRPC_GAME_PAGE",
    "WWRPC_GAME_PAGE_PORTRAITS",
];

/// Run the real binary with `flags`, with the operator's own `WWRPC_*`
/// variables removed.
fn run(flags: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wwrpc"));
    command.args(flags);
    for name in STEERING {
        command.env_remove(name);
    }
    command.output().expect("the wwrpc binary runs")
}

/// stdout of a finished run, trimmed.
fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// stderr of a finished run, quoted into an assertion message.
fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

/// Empty scratch directory for the spawned daemon, so its runtime
/// directory and lock never touch (or lock) the real ones.
fn sandbox(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wwrpc-gating-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the sandbox dir is created");
    dir
}

/// Pins the dry run a user runs to debug a card: game data switched off
/// means the game files are not read, so a `--game-dir` that points at
/// nothing must not stop it. `--static-fallback` is what makes the
/// no-data payload a JSON payload instead of the silence line, and the
/// static text proves the database was never consulted.
#[test]
fn test_no_database_needs_no_game_install() {
    let output = run(&[
        "--no-database",
        "--print-activity",
        "--static-fallback",
        "--game-dir",
        MISSING_GAME_DIR,
    ]);

    assert!(
        output.status.success(),
        "a dry run with --no-database needs no install; it failed with {:?}: {}",
        output.status,
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    let activity: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!("the dry run prints the activity payload as JSON ({err}); got: {stdout}")
    });
    assert_eq!(
        activity.get("details").and_then(|d| d.as_str()),
        Some("Exploring SOL-III"),
        "--no-database must publish the static tier, never data read from the game"
    );
}

/// Pins the other half of that promise: without `--static-fallback` a
/// data-less dry run has nothing to say and says exactly what the daemon
/// would do — leave the slot to game detection.
#[test]
fn test_print_activity_without_data_is_silent() {
    let output = run(&[
        "--no-database",
        "--print-activity",
        "--game-dir",
        MISSING_GAME_DIR,
    ]);

    assert!(
        output.status.success(),
        "a dry run with --no-database needs no install; it failed with {:?}: {}",
        output.status,
        stderr_of(&output)
    );
    assert_eq!(
        stdout_of(&output),
        "(silent: no rich data — game detection would own the slot)",
        "with no data and no --static-fallback there is nothing to publish"
    );
}

/// Pins that the character list is answered from the crate's own tables:
/// no install, no game data, no lookup at all.
#[test]
fn test_list_characters_needs_no_game_install() {
    let output = run(&["--list-characters", "--game-dir", MISSING_GAME_DIR]);

    assert!(
        output.status.success(),
        "the character list is built-in data; it failed with {:?}: {}",
        output.status,
        stderr_of(&output)
    );
    let expected: Vec<String> = wwrpc::character::all()
        .map(|character| format!("{} ({})", character.display, character.key))
        .collect();
    let stdout = stdout_of(&output);
    let listed: Vec<&str> = stdout.lines().take(expected.len()).collect();
    assert_eq!(
        listed.len(),
        expected.len(),
        "every character must be listed; got: {stdout}"
    );
    assert_eq!(
        listed,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
}

/// Pins the startup every user hits: `--no-database` promises the game
/// files are not needed, so a wrong `--game-dir` must not stop the daemon
/// before its loop even begins.
#[test]
fn test_no_database_starts_without_a_game_install() {
    let base = sandbox("start");
    // Same environment as the dry runs: nothing outside the command line
    // may decide whether this daemon starts.
    let mut command = Command::new(env!("CARGO_BIN_EXE_wwrpc"));
    for name in STEERING {
        command.env_remove(name);
    }
    let mut child = command
        .args(["--no-database", "--game-dir", MISSING_GAME_DIR])
        .env("XDG_RUNTIME_DIR", &base)
        .env("XDG_CACHE_HOME", base.join("cache"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the wwrpc binary starts");
    std::thread::sleep(Duration::from_millis(1000));
    // Read the verdict, then kill before asserting: a failing assertion
    // must not leave a daemon behind.
    let exited = child.try_wait().expect("the child's state is readable");
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        exited.is_none(),
        "the daemon must keep running without an install, but it exited: {exited:?}"
    );
    let _ = fs::remove_dir_all(&base);
}

/// Pins that `--no-database` does not turn an account listing into a
/// no-op — a listing *is* a read of the database — and that sharing the
/// command line with a dry run changes nothing about it: both forms fail
/// on the missing install, with the same message.
#[test]
fn test_list_accounts_still_needs_the_install() {
    let listing = run(&["--list-accounts", "--game-dir", MISSING_GAME_DIR]);
    let with_dry_run = run(&[
        "--no-database",
        "--list-accounts",
        "--print-activity",
        "--game-dir",
        MISSING_GAME_DIR,
    ]);

    assert!(
        !listing.status.success(),
        "a listing with no install has nothing to list"
    );
    assert_eq!(
        stderr_of(&with_dry_run),
        stderr_of(&listing),
        "sharing the command line with a dry run must not change how a listing fails"
    );
}

/// The arguments that decide whether an invocation is about the game's
/// files, built from the flags alone.
fn args_with(no_database: bool, list_accounts: bool, print_activity: bool) -> Args {
    let mut argv = vec!["wwrpc"];
    for (flag, present) in [
        ("--no-database", no_database),
        ("--list-accounts", list_accounts),
        ("--print-activity", print_activity),
    ] {
        if present {
            argv.push(flag);
        }
    }
    Args::parse_from(argv)
}

/// Pins the whole truth table, one row per combination of the three flags
/// that can want the game's files: a run reads them unless `--no-database`
/// took the reading away, while the two diagnostics name the data even
/// when they end up reading none of it.
#[test]
fn test_needs_game_data_truth_table() {
    let rows = [
        // no_database, list_accounts, print_activity, needs_game_data, why
        (
            false,
            false,
            false,
            true,
            "the daemon reads the database every tick",
        ),
        (
            false,
            false,
            true,
            true,
            "a dry run reproduces the tick's payload",
        ),
        (
            false,
            true,
            false,
            true,
            "the listing is a read of the level database",
        ),
        (
            false,
            true,
            true,
            true,
            "either diagnostic reads the database",
        ),
        (
            true,
            false,
            false,
            false,
            "static presence alone touches no game file",
        ),
        (
            true,
            false,
            true,
            true,
            "a dry run is about the game's data even when it reads nothing",
        ),
        (
            true,
            true,
            false,
            true,
            "a listing cannot be served without the database",
        ),
        (
            true,
            true,
            true,
            true,
            "either diagnostic reads the database",
        ),
    ];

    for (no_database, list_accounts, print_activity, expected, why) in rows {
        let args = args_with(no_database, list_accounts, print_activity);
        let flags = format!(
            "--no-database={no_database} --list-accounts={list_accounts} --print-activity={print_activity}"
        );
        assert_eq!(wwrpc::needs_game_data(&args), expected, "{flags}: {why}");
    }
}
