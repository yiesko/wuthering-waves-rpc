//! Unit tests for the gate `run` consults before it opens anything:
//! [`crate::reads_game_data`], which says whether this invocation reads the
//! game's files — not merely whether it is about them. The public half of
//! the pair ([`crate::needs_game_data`]) is pinned by the integration suite,
//! which drives the real binary; these pin the second question directly,
//! since it is the one that decides what a run opens.

use clap::Parser;

use crate::{Args, needs_game_data, reads_game_data};

/// The arguments that decide whether an invocation reads the game's files,
/// built from the flags alone.
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

/// The flag names a row was built from, quoted into an assertion message.
fn flag_names(no_database: bool, list_accounts: bool, print_activity: bool) -> String {
    format!(
        "--no-database={no_database} --list-accounts={list_accounts} --print-activity={print_activity}"
    )
}

/// Pins the whole table, one row per combination of the three flags that can
/// want the game's files: the same rows [`needs_game_data`] is pinned on,
/// resolved into what a run actually opens.
///
/// A listing reads in every combination — it exists to read the database.
/// A dry run reads unless `--no-database` took the reading away, the one
/// invocation where the two gates disagree: it stays about the game's data
/// and reads none of it, so a `--game-dir` that points at nothing cannot stop
/// it.
#[test]
fn reads_game_data_truth_table() {
    let rows = [
        // no_database, list_accounts, print_activity, reads_game_data, why
        (
            false,
            false,
            false,
            true,
            "the daemon copies the database every tick",
        ),
        (
            false,
            false,
            true,
            true,
            "a dry run reproduces the payload a tick would publish",
        ),
        (
            false,
            true,
            false,
            true,
            "a listing is a read of the level database",
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
            "static presence alone opens no game file",
        ),
        (
            true,
            false,
            true,
            false,
            "a dry run without the database resolves nothing at all",
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
            "the listing wins over the dry run sharing its command line",
        ),
    ];

    for (no_database, list_accounts, print_activity, expected, why) in rows {
        let args = args_with(no_database, list_accounts, print_activity);
        assert_eq!(
            reads_game_data(&args),
            expected,
            "{}: {why}",
            flag_names(no_database, list_accounts, print_activity)
        );
    }
}

/// Pins the invariant behind the run's unreachable fallback: asking for the
/// stored accounts reads the database whatever else is on the command line.
///
/// That is what lets a listing take the directories the gate resolved. The
/// fallback in `run` cannot fire while these rows hold; should a later gate
/// ever let `--no-database` win over a listing, this test fails first and
/// the fallback becomes reachable on purpose.
#[test]
fn list_accounts_always_reads_game_data() {
    for no_database in [false, true] {
        for print_activity in [false, true] {
            let args = args_with(no_database, true, print_activity);
            let flags = flag_names(no_database, true, print_activity);
            assert!(
                reads_game_data(&args),
                "{flags}: a listing exists to read the database"
            );
            assert!(
                needs_game_data(&args),
                "{flags}: and it is always about it, flag or no flag"
            );
        }
    }
}
