// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Integration tests for the launcher account cache parser.
//!
//! The launcher writes its account cache as JSON that also holds each
//! account's e-mail address, its token and its verification codes. The
//! parser is the only thing in this program that parses that file, so what
//! it chooses to read out of it is the whole of what can ever leave the
//! file: a field it has no use for cannot be logged, cached or leaked,
//! because it is never turned into a value in the first place.
//!
//! The parser is also the only bound on the read itself. A cache past the
//! byte ceiling is refused while its bytes are still arriving, so it is safe
//! to hand the parser any path a caller happens to have — which is what
//! makes it safe for a caller that never made a copy first.
//!
//! Every fixture here is written by the test into a directory of its own.
//! None of it depends on a game being installed: an oversized file is grown
//! here, never on the machine's real cache.

use std::fs;
use std::path::{Path, PathBuf};

use wwrpc::db::{
    LauncherCache, MAX_CUID_CHARS, MAX_JSON_BYTES, MAX_NAME_CHARS, MAX_ROWS, read_launcher_cache,
};

/// Prefix of every fixture directory this file creates.
const FIXTURE_PREFIX: &str = "wwrpc-launcher-cache";

/// A cuid no account can ever hold, so a test can never match a real one.
const SAMPLE_CUID: &str = "111111111";

/// The byte ceiling as the length type a reader works in.
fn byte_ceiling() -> usize {
    usize::try_from(MAX_JSON_BYTES).expect("the ceiling fits in memory")
}

/// Empty fixture directory for one test.
fn fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{FIXTURE_PREFIX}-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the fixture dir is created");
    dir
}

/// Write `body` as a cache file in `dir` and hand back its path.
fn write_cache(dir: &Path, tag: &str, body: &str) -> PathBuf {
    let path = dir.join(format!("{tag}.json"));
    fs::write(&path, body).expect("the cache fixture is written");
    path
}

/// Read a cache fixture, insisting it parsed: a test that is not about
/// refusal has no reason to look at an [`Err`].
fn read_ok(path: &Path) -> LauncherCache {
    read_launcher_cache(path).unwrap_or_else(|err| panic!("the fixture parses: {err}"))
}

/// Pins that the parser reads the two fields a listing needs and nothing
/// else. The account object carries an e-mail address, a token, a
/// verification code and a nested profile the launcher keeps beside them,
/// and none of them has a field on [`AccountEntry`](wwrpc::db::AccountEntry)
/// to arrive in — the `Debug` rendering of the whole cache, which prints
/// every field each entry does have, is where they would show if they had
/// been read.
#[test]
fn test_launcher_parser_ignores_unknown_fields() {
    let dir = fixture("unknown-fields");
    let path = write_cache(
        &dir,
        "cache",
        r#"{"last_login_cuid": "111111111", "account_list": [
            {"cuid": "111111111", "thirdNickName": "RoverOne",
             "email": "someone@example.invalid", "token": "secret-token",
             "verify_code": "123456", "profile": {"nickname": "hidden"}}
        ], "machine_id": "machine-secret"}
"#,
    );

    let cache = read_ok(&path);

    assert_eq!(cache.last_login_cuid.as_deref(), Some(SAMPLE_CUID));
    let accounts = cache.accounts.as_ref().expect("the account list is read");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].cuid.as_deref(), Some(SAMPLE_CUID));
    assert_eq!(accounts[0].third_nickname.as_deref(), Some("RoverOne"));
    let rendered = format!("{cache:?}");
    for secret in [
        "someone@example.invalid",
        "secret-token",
        "123456",
        "machine-secret",
        "hidden",
    ] {
        assert!(
            !rendered.contains(secret),
            "the parser must never read {secret} into a value it holds"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

/// Pins both caps and, more than that, the difference between them: an
/// account list past the row cap stops at it, because the accounts past it
/// are never built, while an over-long name is *dropped* rather than
/// shortened, because a cut name would be a different — wrong — name shown
/// to a user. A value sitting exactly on its cap is kept whole.
#[test]
fn test_launcher_parser_caps_and_bounds() {
    let dir = fixture("caps");
    let accounts: Vec<String> = (0..1000)
        .map(|row| format!(r#"{{"cuid": "{row:09}", "thirdNickName": "Rover{row}"}}"#))
        .collect();
    let many = write_cache(
        &dir,
        "many",
        &format!(r#"{{"account_list": [{}]}}"#, accounts.join(",")),
    );

    let cache = read_ok(&many);
    let parsed = cache.accounts.as_ref().expect("the account list is read");
    assert_eq!(
        parsed.len(),
        MAX_ROWS,
        "an account list past the row cap is cut down to it"
    );
    assert_eq!(
        parsed[0].third_nickname.as_deref(),
        Some("Rover0"),
        "the cap keeps the first accounts in the file, not an arbitrary slice"
    );
    let last = MAX_ROWS - 1;
    let last_kept = format!("Rover{last}");
    assert_eq!(
        parsed[last].third_nickname.as_deref(),
        Some(last_kept.as_str()),
        "the last kept account is the last one inside the cap"
    );

    let name_at_cap = "n".repeat(MAX_NAME_CHARS);
    let name_past_cap = "n".repeat(MAX_NAME_CHARS + 1);
    let cuid_past_cap = "c".repeat(MAX_CUID_CHARS + 1);
    let bounded = write_cache(
        &dir,
        "bounded",
        &format!(
            r#"{{"last_login_cuid": "{cuid_past_cap}", "account_list": [
                {{"cuid": "111111111", "thirdNickName": "{name_at_cap}"}},
                {{"cuid": "222222222", "thirdNickName": "{name_past_cap}"}},
                {{"cuid": "{cuid_past_cap}", "thirdNickName": "RoverThree"}}
            ]}}"#
        ),
    );

    let cache = read_ok(&bounded);
    assert_eq!(
        cache.last_login_cuid, None,
        "a marker past the cuid cap is not one this program expects to match"
    );
    let parsed = cache.accounts.as_ref().expect("the account list is read");
    assert_eq!(
        parsed[0].third_nickname.as_deref(),
        Some(name_at_cap.as_str()),
        "a name sitting exactly on the cap is kept whole"
    );
    assert_eq!(
        parsed[1].third_nickname, None,
        "a name past the cap is dropped, never shortened"
    );
    assert_eq!(
        parsed[1].cuid.as_deref(),
        Some("222222222"),
        "dropping a name costs only the name"
    );
    assert_eq!(
        parsed[2].cuid, None,
        "an identifier past the cap is dropped, never shortened"
    );
    assert_eq!(
        cache.nicknames().get(SAMPLE_CUID).copied(),
        Some(name_at_cap.as_str()),
        "the nickname lookup offers the same bounded values the parse kept"
    );
    assert!(
        !cache.nicknames().contains_key("222222222"),
        "an account whose name was dropped has no nickname to offer"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Pins the byte ceiling on the read itself: a cache past it is refused
/// without ever being held whole, and a file sitting exactly on it is still
/// read. The ceiling is enforced by the reader rather than left to whoever
/// copies the file, because the parser is handed a path by whoever calls it.
#[test]
fn test_launcher_parser_rejects_oversized_file() {
    let dir = fixture("oversized");
    let ceiling = byte_ceiling();
    let head = r#"{"last_login_cuid": "1", "pad": ""#;
    let tail = "\"}";
    let at_ceiling = format!(
        "{head}{}{tail}",
        "a".repeat(ceiling.saturating_sub(head.len() + tail.len()))
    );
    assert_eq!(
        at_ceiling.len(),
        ceiling,
        "the boundary fixture is exactly as long as the ceiling says"
    );
    let over = write_cache(&dir, "over", &format!("{at_ceiling} "));

    let err = read_launcher_cache(&over).expect_err("a cache past the ceiling is refused");
    assert!(
        err.contains(&MAX_JSON_BYTES.to_string()),
        "the refusal says which ceiling was crossed: {err}"
    );
    assert_eq!(
        err.chars().next().map(char::is_lowercase),
        Some(true),
        "error messages start lowercase: {err}"
    );

    let boundary = write_cache(&dir, "boundary", &at_ceiling);
    assert_eq!(
        read_ok(&boundary).last_login_cuid.as_deref(),
        Some("1"),
        "a file exactly on the ceiling is still read whole"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Pins that a cache missing the fields this program reads is an empty
/// cache, not a failure. Every one of these shapes is something the launcher
/// could plausibly write, and the answer a listing gets from each of them is
/// the same one it got when the file was walked as untyped values: no
/// accounts, no marker, no panic. The numeric fixtures cover every shape a
/// JSON number can take, because a positive integer, a negative one and a
/// fraction each reach a different arm of the parser.
#[test]
fn test_launcher_parser_missing_fields() {
    let dir = fixture("missing");
    for (tag, body) in [
        ("empty", "{}"),
        ("list-null", r#"{"account_list": null}"#),
        ("list-number", r#"{"account_list": 42}"#),
        ("list-negative", r#"{"account_list": -42}"#),
        ("list-string", r#"{"account_list": "nope"}"#),
        ("list-bool", r#"{"account_list": true}"#),
        ("list-object", r#"{"account_list": {}}"#),
        ("list-of-strings", r#"{"account_list": ["one", "two"]}"#),
        (
            "list-of-numbers",
            r#"{"account_list": [1, -1, 1.5, 9999999999]}"#,
        ),
        ("list-of-empty-objects", r#"{"account_list": [{}, {}]}"#),
        ("document-null", "null"),
        ("document-number", "42"),
        ("document-negative", "-42"),
        ("document-array", r#"["nope"]"#),
        ("document-string", r#""nope""#),
    ] {
        let path = write_cache(&dir, tag, body);
        let cache = read_ok(&path);
        assert_eq!(cache.last_login_cuid, None, "{body} has no marker");
        assert_eq!(
            cache.nicknames().len(),
            0,
            "{body} holds no nickname for any account"
        );
    }

    let empty = read_ok(&write_cache(&dir, "empty-again", "{}"));
    assert_eq!(
        empty,
        LauncherCache::default(),
        "an empty cache is the default one"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Pins that skipping an unusable entry does not skip the entries after it.
/// An entry the launcher wrote as `null`, as a boolean or as a nested list
/// is not an account and carries nothing, but a parser that steps over it
/// without consuming the rest of it would lose every account after it —
/// and the account list is the one part of this file a listing reads.
#[test]
fn test_launcher_parser_keeps_accounts_after_an_unusable_entry() {
    let dir = fixture("skipped-entries");
    let path = write_cache(
        &dir,
        "mixed",
        r#"{"last_login_cuid": "111111111", "account_list": [
            null, true, ["nested"], {"cuid": ""}, {},
            {"cuid": "111111111", "thirdNickName": "RoverOne"},
            {"thirdNickName": "Orphan"}
        ]}"#,
    );

    let cache = read_ok(&path);

    assert_eq!(
        cache.nicknames().len(),
        1,
        "only the entry with both halves is a nickname, however many were skipped before it"
    );
    assert_eq!(
        cache.nicknames().get(SAMPLE_CUID).copied(),
        Some("RoverOne"),
        "the account after the skipped entries is still read"
    );
    let accounts = cache.accounts.as_ref().expect("the account list is read");
    assert_eq!(
        accounts.len(),
        3,
        "only the entries carrying one of the two fields are accounts; an empty \
         string is still what the launcher wrote, and it is the nickname lookup \
         that decides it cannot be used"
    );
    assert_eq!(accounts[0].cuid.as_deref(), Some(""));
    assert_eq!(accounts[1].third_nickname.as_deref(), Some("RoverOne"));
    assert_eq!(accounts[2].cuid, None);

    let _ = fs::remove_dir_all(&dir);
}

/// Pins what the last-login marker accepts, field by field: a marker this
/// program cannot use as one is an absent marker, never a failed read. The
/// marker is corroboration only — it refreshes on logout/login cycles — so
/// losing it costs a hint and nothing else.
#[test]
fn test_launcher_parser_marker_shapes() {
    let dir = fixture("marker");
    for (tag, body, expected) in [
        (
            "present",
            r#"{"last_login_cuid": "222222222"}"#,
            Some("222222222"),
        ),
        ("absent", r#"{"account_list": []}"#, None),
        ("empty", r#"{"last_login_cuid": ""}"#, None),
        ("number", r#"{"last_login_cuid": 123}"#, None),
        ("negative", r#"{"last_login_cuid": -123}"#, None),
        ("null", r#"{"last_login_cuid": null}"#, None),
        ("list", r#"{"last_login_cuid": ["222222222"]}"#, None),
        (
            "object",
            r#"{"last_login_cuid": {"cuid": "222222222"}}"#,
            None,
        ),
    ] {
        let cache = read_ok(&write_cache(&dir, tag, body));
        assert_eq!(cache.last_login_cuid.as_deref(), expected, "{body}");
    }

    let unreadable = write_cache(&dir, "garbage", "not json");
    assert!(
        read_launcher_cache(&unreadable).is_err(),
        "bytes that are not JSON are refused rather than half-read"
    );
    let absent = dir.join("never-written.json");
    assert!(
        read_launcher_cache(&absent).is_err(),
        "a cache that is not there is refused rather than assumed empty"
    );

    let _ = fs::remove_dir_all(&dir);
}
