//! Unit tests for the account listing's own rules: the mask applied to a
//! login nickname before it is printed ([`crate::mask_name`]), and the row
//! those masked values land in ([`crate::account_line`]).
//!
//! Neither is exercised here through the terminal — that would mean a
//! database on disk, a subprocess and captured stdout — so both are pinned
//! as the pure functions they are. The mask has edges (names of one and two
//! characters, an empty one, names outside ASCII), and the row is the one
//! place where a nickname could reach a screen unmasked: a rule with edges
//! and a sink with no test around it are each a thing that gets edited
//! without one.

use crate::db::StoredAccount;
use crate::{account_line, mask_name};

/// One stored account to render, built from the fields a row is made of.
/// The struct owns its strings, so a borrowed field is copied here rather
/// than carried through the whole test.
fn account(
    index: usize,
    region: &str,
    level: &str,
    nickname: Option<&str>,
    last_login: bool,
) -> StoredAccount {
    StoredAccount {
        index,
        region: region.to_string(),
        level: level.to_string(),
        nickname: nickname.map(String::from),
        last_login,
    }
}

/// Pins the shape a reader recognises: first character, last character, and
/// one star per character between — so the mask is as long as the name it
/// hides, whatever the name is. A nickname is a word or a string of digits,
/// and both mask the same way.
#[test]
fn mask_keeps_both_ends_and_stars_the_rest() {
    assert_eq!(mask_name("Game"), "G**e", "a word keeps its two ends");
    assert_eq!(mask_name("500042"), "5****2", "digits mask like any word");
    assert_eq!(mask_name("Rover"), "R***r", "any length keeps both ends");
}

/// Pins the edges the rule has to settle by itself. A name of one or two
/// characters has no interior: keeping both ends would print the name
/// itself, so there is nothing left to hide behind a mask.
#[test]
fn mask_stars_out_names_too_short_to_keep_two_ends() {
    assert_eq!(mask_name(""), "", "a name with no characters has no mask");
    assert_eq!(
        mask_name("A"),
        "*",
        "the one character kept would be the name"
    );
    assert_eq!(
        mask_name("AB"),
        "**",
        "both characters kept would be the name"
    );
    assert_eq!(mask_name("ABC"), "A*C", "three characters keep both ends");
}

/// Pins that the rule counts characters and never bytes — the detail a
/// rewrite that slices the name at a byte offset gets wrong, and gets
/// wrong quietly: it cuts a two-byte letter in half and prints a
/// replacement character where the name used to be.
#[test]
fn mask_counts_characters_not_bytes() {
    // Nine characters, thirteen bytes: a byte-indexed mask would land inside
    // `Á` and inside `ő`.
    assert_eq!(mask_name("Árvíztűrő"), "Á*******ő");
    // Three characters, nine bytes.
    assert_eq!(mask_name("鳴神月"), "鳴*月");
    // Three characters, twelve bytes.
    assert_eq!(mask_name("🌊🌊🌊"), "🌊*🌊");
}

/// Pins the invariant behind the rows above across every script the rows
/// cover: the mask is as long as the name in characters, its two ends are
/// the name's two ends, and nothing else of the name survives — so a rule
/// that changed shape fails here even on a name no other row lists.
#[test]
fn mask_keeps_exactly_the_two_ends() {
    for name in ["Game", "500042", "Rover", "Árvíztűrő", "鳴神月", "🌊🌊🌊"] {
        let masked = mask_name(name);
        let characters: Vec<char> = name.chars().collect();
        let last = characters.len() - 1;

        assert_eq!(
            masked.chars().count(),
            characters.len(),
            "one output character per input character: {name} -> {masked}"
        );
        assert!(
            masked.starts_with(characters[0]),
            "the first character survives: {name} -> {masked}"
        );
        assert!(
            masked.ends_with(characters[last]),
            "the last character survives: {name} -> {masked}"
        );
        assert!(
            masked.chars().skip(1).take(last - 1).all(|c| c == '*'),
            "only the two ends survive: {name} -> {masked}"
        );
    }
}

/// Pins that a row opens with the index, the region and the level, in that
/// order and out of the account's own fields.
///
/// The index is the payload of a listing: it is the number the reader types
/// into `--account-index N`. A row reformatted so the index moved, became
/// optional or turned into a label makes the command useless for its only
/// purpose, and nothing else in the suite would say so.
#[test]
fn row_leads_with_index_region_and_level() {
    for (index, region, level) in [(1usize, "Europe", "60"), (7, "Asia", "45")] {
        let line = account_line(&account(index, region, level, Some("RoverOne"), false));
        assert!(
            line.starts_with(&format!("{index}: {region}, Union {level}")),
            "a row opens with the value the reader pins: {line}"
        );
    }
}

/// Pins the sink itself: whatever a nickname is, the row carries it through
/// the mask and never through the name.
///
/// This is the assertion that fails if `account_line` is ever reworked into
/// printing `nickname` where it prints `mask_name(nickname)` — one token,
/// and until now no test anywhere in the suite would have noticed.
///
/// The names here all have three characters or more, because that is where
/// the two forms differ: below three, the mask *is* the name, by the rule
/// [`mask_name`] pins for itself.
#[test]
fn row_shows_a_masked_nickname_and_never_the_whole_one() {
    for nickname in ["RoverOne", "500042", "Árvíztűrő"] {
        let line = account_line(&account(1, "Europe", "60", Some(nickname), false));
        assert!(
            line.contains(&mask_name(nickname)),
            "the masked nickname is what the row shows: {line}"
        );
        assert!(
            !line.contains(nickname),
            "the nickname itself must never reach the row: {line}"
        );
    }
}

/// Pins the last-login marker: on the end of the row, after the closing
/// parenthesis, and only on a row that carries it.
#[test]
fn row_appends_the_marker_only_after_the_closing_paren() {
    let with_nickname = account_line(&account(2, "America", "30", Some("RoverOne"), true));
    assert!(
        with_nickname.ends_with("(login nickname: R******e?) [last launcher login]"),
        "the marker closes a row that carries a nickname: {with_nickname}"
    );

    let without_nickname = account_line(&account(2, "America", "30", None, true));
    assert!(
        without_nickname.ends_with("2: America, Union 30 [last launcher login]"),
        "and it follows the level when there is no nickname: {without_nickname}"
    );

    let unmarked = account_line(&account(2, "America", "30", Some("RoverOne"), false));
    assert!(
        !unmarked.contains("last launcher login"),
        "a row without the marker carries none of it: {unmarked}"
    );
}

/// Pins that a row with no nickname carries no parentheses at all: the
/// parentheses are what tell a reader the value inside is a nickname and
/// not a level, so an empty pair would misread as one.
#[test]
fn row_without_a_nickname_carries_no_parentheses() {
    let line = account_line(&account(3, "Asia", "45", None, false));
    assert!(
        !line.contains('(') && !line.contains(')'),
        "there is no nickname to put between parentheses: {line}"
    );
}

/// Pins that a name outside ASCII survives the trip into the row whole.
///
/// The mask is built character by character, so this row carries no
/// replacement character: a mask assembled from bytes would, and the row
/// would show the name as it looked mid-slicing rather than as it is.
#[test]
fn row_renders_a_multi_byte_nickname_without_a_replacement_character() {
    let nickname = "Árvíztűrő tükörfúrógép";
    let line = account_line(&account(3, "Asia", "45", Some(nickname), false));

    assert!(
        !line.contains('\u{FFFD}'),
        "a row is UTF-8 from end to end, never a cut letter: {line}"
    );
    assert!(
        line.contains(&mask_name(nickname)),
        "the mask survives the trip intact: {line}"
    );
}
