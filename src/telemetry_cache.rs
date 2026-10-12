// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Change detection for files wwrpc reads on a poll loop.
//!
//! Every answer the poll loop draws from a game file it must copy first —
//! the launcher telemetry rows behind the player's display name, the
//! remnants behind a recovered login — has one thing in common: "there is
//! nothing here" is the ordinary answer, not an exceptional one. The rows
//! are written at login and deleted once the launcher uploads them, so most
//! ticks find no name at all. A cache that only remembers hits would
//! therefore re-read the file on exactly the ticks where there is nothing
//! new in it, which is the same as never caching anything.
//!
//! [`TelemetryCache`] keys a lookup on the file it was read from — its
//! length and its whole modification time, through [`file_signature`] —
//! and keeps the answer either way. The loader runs when the file was
//! written since the last lookup and not otherwise, so a miss costs one
//! copy per version of the file instead of one per tick.
//!
//! Two limits follow from a signature describing bytes, and both are the
//! caller's to handle. A read that never completed is not an answer and is
//! never stored, so a refused copy is tried again rather than frozen
//! ([`Lookup`]); and an answer reached from something outside the file — the
//! clock, most of all — is not re-derived either, so a cached one goes on
//! standing whichever way time moves it.
//!
//! The signature describes a file, not a question. A caller whose question
//! changes while the file does not — a different account asking for its own
//! name in telemetry nobody wrote since — must say so with
//! [`TelemetryCache::clear`], because the cache alone cannot tell the two
//! lookups apart.

use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

/// One version of one file: how long it is, and when it was last written.
///
/// Two observations of an unchanged file are the same version, which is the
/// whole of what a lookup cache needs to know. The time is read whole
/// (seconds plus the nanoseconds the filesystem records), so a rewrite
/// inside the same second is still a new version wherever the filesystem
/// keeps that much precision.
///
/// A signature says nothing about the contents: a file rewritten to exactly
/// the same length at exactly the same time keeps its identity, and a lookup
/// keyed on one cannot see the difference. Callers that need more than "did
/// this change" — a database whose rows are deleted without changing its
/// size, say — must not read one as proof of anything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileSignature {
    mtime: SystemTime,
    len: u64,
}

impl FileSignature {
    /// A signature for an observation taken by hand rather than by
    /// [`file_signature`]: a caller folding several files into one
    /// signature, where a file that is not there contributes a fixed
    /// sentinel instead of an error.
    #[must_use]
    pub fn new(mtime: SystemTime, len: u64) -> Self {
        Self { mtime, len }
    }
}

/// The signature of the file at `path`: its length and its whole
/// modification time.
///
/// # Errors
///
/// Whatever measuring the file failed with, untouched in kind. A file that
/// is not there is [`io::ErrorKind::NotFound`], which callers turn into
/// "nothing to read" rather than into a signature: an absent file must
/// never compare equal to some other absent file forever.
pub fn file_signature(path: &Path) -> io::Result<FileSignature> {
    let meta = fs::metadata(path)?;
    Ok(FileSignature {
        mtime: meta.modified()?,
        len: meta.len(),
    })
}

/// What one read of a file produced: whether it happened at all, and the
/// answer when it did.
///
/// The two are worth separating because only one of them is knowledge. "The
/// rows hold no such name" and "the rows could not be read" both arrive as a
/// `None` from a plain [`Option`], and only the first is an answer — while a
/// signature describes the file's bytes and says nothing about the disk they
/// sit on. Storing the second under one would freeze a full filesystem, a
/// busy destination or a permission change until the writer next touched the
/// file, which for a launcher that mostly does not write is the rest of the
/// session. [`TelemetryCache`] keeps the first and forgets the second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<T> {
    /// The file was never read: nowhere to copy it, a copy that was refused,
    /// bytes that would not come back. Nothing was learned, so nothing is
    /// cached and the next lookup runs its loader again.
    Unread,
    /// The file was read, and this is what it said. `None` inside is an
    /// answer — "no such name in these rows" — and is kept as one.
    Answered(Option<T>),
}

/// One cached lookup, kept against the signature of the file it was read
/// from.
///
/// Empty until the first lookup, which always runs its loader: there is no
/// signature yet to match. After that the loader runs exactly when the
/// signature differs from the cached one, and a lookup that found nothing
/// is cached like any other answer — the file cannot start containing a
/// name without changing, and re-reading it would cost a copy per tick for
/// an answer the bytes cannot produce differently.
///
/// What the bytes cannot produce differently is the whole of it, and that
/// limit is the caller's to respect. A lookup that answers from something
/// outside the file — the clock, most of all — is not re-derived here, so a
/// cached answer freezes that reading too, in whichever direction time moves
/// it. The remnant scan in [`crate::db`] is the case that documents it: a
/// scan judges event age against the clock, and while the signature holds,
/// both an accepted event keeps being accepted and a rejected one keeps
/// being rejected. The one reading the cache refuses to freeze is one that
/// never happened: [`Lookup::Unread`] is not stored at all.
///
/// The answer is handed back by copy, since the caller usually keeps it: a
/// display name the reader stores against the login it belongs to, a census
/// the tick logs.
#[derive(Debug, Clone)]
pub struct TelemetryCache<T: Clone> {
    value: Option<T>,
    signature: Option<FileSignature>,
}

impl<T: Clone> TelemetryCache<T> {
    /// An empty cache: the next lookup runs its loader whatever the file
    /// looks like.
    #[must_use]
    pub fn new() -> Self {
        Self {
            value: None,
            signature: None,
        }
    }

    /// The answer for `signature`, read from `loader` only when the file was
    /// written since the last lookup.
    ///
    /// The loader runs when `signature` differs from the one the cached
    /// answer was read at, and not at all while they are the same. Returns
    /// the cached answer for a signature already seen — `None` included,
    /// since "no name in there" is an answer, not the absence of one.
    ///
    /// A loader that reports [`Lookup::Unread`] stores nothing. The cache
    /// keeps what it held and stays without a signature, so the next call
    /// runs the loader again whatever the file looks like — the retry a
    /// failed read owes. The returned value cannot tell a failed read from a
    /// cached miss: for this crate's two call sites both mean "no answer on
    /// this tick", and only the cache needs to know the difference.
    pub fn get_with(
        &mut self,
        signature: FileSignature,
        loader: impl FnOnce() -> Lookup<T>,
    ) -> Option<T> {
        if self.signature == Some(signature) {
            return self.value.clone();
        }
        match loader() {
            Lookup::Unread => None,
            Lookup::Answered(answer) => {
                self.value = answer.clone();
                self.signature = Some(signature);
                answer
            }
        }
    }

    /// Forget the answer and the signature it was read at, so the next
    /// lookup runs its loader even if the file has not changed.
    ///
    /// For the caller whose question changed while the file did not: a new
    /// login asking a telemetry database that has not been written since
    /// for its own name would otherwise be answered with the previous
    /// login's.
    pub fn clear(&mut self) {
        self.value = None;
        self.signature = None;
    }
}

/// An empty cache, the same state [`TelemetryCache::new`] builds.
impl<T: Clone> Default for TelemetryCache<T> {
    fn default() -> Self {
        Self::new()
    }
}
