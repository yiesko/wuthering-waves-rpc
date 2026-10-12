// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Integration tests for the claims the documentation makes about this
//! repository: where it lives, which Rust version builds it, and how to
//! install it.
//!
//! None of that is checked by the compiler. A wrong repository URL, a
//! minimum Rust version the manifest no longer holds, or an install
//! command naming a directory that is not in the tree all reach the reader
//! intact, so these tests read the files that carry the claims and check
//! them against what this repository actually is.
//!
//! The expected values come from two directions. The minimum Rust version
//! is read out of the manifest's own `rust-version` key and the README is
//! checked against it, so raising the manifest leaves the prose to follow.
//! The canonical repository URL is written here, and the documented files
//! are checked against it.

use std::fs;
use std::path::Path;

/// The repository this project lives in, as every document that points at
/// this project must name it.
const CANONICAL_REPO: &str = "https://github.com/yiesko/wuthering-waves-rpc";

/// The detection engine's own repository: a different project by the same
/// author, which this one builds against. Naming it is right where the
/// engine is credited and wrong where a file claims to describe this
/// project.
const DETECTION_ENGINE: &str = "https://github.com/yiesko/rsRPC";

/// The files a reader is given the project's coordinates, its history or
/// its install instructions in.
const DOCUMENTED: [&str; 4] = [
    "Cargo.toml",
    "README.md",
    "CHANGELOG.md",
    "assets/wwrpc.service",
];

/// Reads a file of this repository, named by its path from the root.
fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    fs::read_to_string(&full).unwrap_or_else(|err| panic!("{} is readable ({err})", full.display()))
}

/// The value of `rust-version` in the manifest's `[package]` table, with
/// any trailing comment and the surrounding quotes removed.
///
/// Returns a slice borrowed from `manifest`, so the caller keeps ownership
/// of the string it was read from.
fn manifest_msrv(manifest: &str) -> &str {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if in_package && key.trim() == "rust-version" {
            return value
                .split('#')
                .next()
                .unwrap_or_default()
                .trim()
                .trim_matches('"');
        }
    }
    panic!("[package] in Cargo.toml declares a rust-version")
}

/// Pins the manifest's `repository` field to the canonical project URL.
///
/// Everything published from this manifest — the crates.io page included —
/// reads that one field, so a wrong value there points every consumer at a
/// repository that is not this one.
///
/// The expected value is the constant above rather than a copy of the
/// current line, so the field is asserted instead of read back.
#[test]
fn test_manifest_names_the_canonical_repository() {
    let manifest = read("Cargo.toml");
    let declared = manifest
        .lines()
        .find_map(|line| line.trim().strip_prefix("repository"))
        .and_then(|rest| rest.trim().strip_prefix('='))
        .unwrap_or_else(|| panic!("Cargo.toml declares a repository field; got:\n{manifest}"))
        .trim()
        .trim_matches('"');

    assert_eq!(
        declared, CANONICAL_REPO,
        "the manifest must declare this project's repository"
    );
}

/// Pins that no documented file points at a path this repository does not
/// live at.
///
/// The README's CI badge is the only place the README points at this
/// repository, and no test that checks a named value reads it: this sweep
/// is what covers the badge's `href` and `src`.
///
/// The manifest, the changelog and the unit are swept here as well, and
/// each of those also has a test pinning its exact value.
///
/// The comparison is case-insensitive because repository URLs resolve case
/// insensitively, so `wwRPC` is the same wrong path as `wwrpc`.
#[test]
fn test_no_documented_file_names_another_path_to_this_repository() {
    for file in DOCUMENTED {
        let text = read(file).to_lowercase();
        assert!(
            !text.contains("yiesko/wwrpc"),
            "{file} must not name `yiesko/wwrpc`; it must use {CANONICAL_REPO}"
        );
    }
}

/// Pins every changelog link definition to the canonical repository.
///
/// The changelog closes with reference definitions, and the reader who
/// follows `[Unreleased]` or a released version lands on the compare or
/// release page they name.
///
/// Each definition is checked on its own, so an extra or missing one is
/// reported too: a changelog whose links all resolve is the contract, not
/// one whose two links resolve.
#[test]
fn test_changelog_links_resolve_on_the_canonical_repository() {
    let changelog = read("CHANGELOG.md");
    let links: Vec<&str> = changelog
        .lines()
        .filter(|line| line.starts_with('[') && line.contains("]: "))
        .collect();

    assert!(
        !links.is_empty(),
        "CHANGELOG.md ends in version link definitions; got:\n{changelog}"
    );
    for link in links {
        assert!(
            link.contains(CANONICAL_REPO),
            "a changelog link definition must use {CANONICAL_REPO}; got: {link}"
        );
    }
}

/// Pins the systemd unit's `Documentation=` to this project's repository.
///
/// `systemctl --user status`, and every bug report that quotes the unit,
/// shows this field as where to read more about the service it describes.
///
/// The field names this project, never the detection engine: that is a
/// separate project and its repository documents nothing here.
#[test]
fn test_service_unit_documents_the_canonical_repository() {
    let unit = read("assets/wwrpc.service");
    let documentation = unit
        .lines()
        .find_map(|line| line.trim().strip_prefix("Documentation="))
        .unwrap_or_else(|| panic!("assets/wwrpc.service declares Documentation=; got:\n{unit}"));

    assert_eq!(
        documentation, CANONICAL_REPO,
        "the unit's Documentation= must name the project the service runs"
    );
    assert!(
        !unit.contains(DETECTION_ENGINE),
        "the unit describes this project, never the detection engine; got:\n{unit}"
    );
}

// The crate is read from the checkout itself, so the install path is the
// current directory rather than a subdirectory of it; `--locked` installs
// the versions the lock file pins instead of whatever resolves today.
// Neither half is optional: the pinned command contains both.
/// Pins the install command to the repository root, in every file that
/// gives it.
///
/// `cargo install --path wwrpc` names a directory that is not in this
/// repository, and `cargo install --locked --path .` is the command that
/// resolves from a checkout. Both the README and the unit's comment above
/// `ExecStart` must carry it.
#[test]
fn test_install_command_targets_the_repository_root() {
    for file in ["README.md", "assets/wwrpc.service"] {
        let text = read(file);
        assert!(
            text.contains("cargo install --locked --path ."),
            "{file} must install with `cargo install --locked --path .`, run from the repository root"
        );
        assert!(
            !text.contains("--path wwrpc"),
            "{file} must not name a `wwrpc/` directory that the repository does not contain"
        );
    }
}

// The expected value is not written here as a number: a hardcoded minimum
// stops catching anything the day the manifest is bumped, and then
// contradicts the file it was written to match.
/// Pins the README's stated minimum Rust version to the manifest's own
/// `rust-version`.
///
/// The expected value is read from the manifest, never written here.
#[test]
fn test_readme_states_the_manifest_minimum_rust_version() {
    let manifest = read("Cargo.toml");
    let msrv = manifest_msrv(&manifest);
    let readme = read("README.md");
    let requirements = readme
        .lines()
        .find(|line| line.starts_with("Requirements:"))
        .unwrap_or_else(|| {
            panic!("the README opens Install with a Requirements: line; got:\n{readme}")
        });
    let stated = requirements
        .split_once("Rust ")
        .and_then(|(_, rest)| rest.split_once('+'))
        .map(|(version, _)| version.trim())
        .unwrap_or_else(|| {
            panic!(
                "the Requirements: line gives the minimum as `Rust <version>+`; got: {requirements}"
            )
        });

    assert_eq!(
        stated, msrv,
        "the README must state the manifest's minimum supported Rust version ({msrv}); \
         got: {requirements}"
    );
}

// A `wwrpc/` prefix is the one mistake a reader cannot see without trying
// the path: the file is named, the sentence reads correctly, and the copy
// fails only once it is run.
/// Pins that the README never routes a repository path through a `wwrpc/`
/// prefix.
///
/// The README is read from inside a checkout, so a file that ships with it
/// is named by its path from the root: `assets/wwrpc.service`, never
/// `wwrpc/assets/wwrpc.service`, which resolves to nothing in a clone.
#[test]
fn test_readme_never_names_a_path_through_a_wwrpc_prefix() {
    let readme = read("README.md");

    assert!(
        !readme.contains("wwrpc/assets"),
        "the README names files of this repository from its root, never through a `wwrpc/` prefix; \
         got:\n{readme}"
    );
}
