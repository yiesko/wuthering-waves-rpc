// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Minimal Discord IPC client (Unix socket): handshake, `SET_ACTIVITY`
//! and clear. Only what wwrpc needs: no subscriptions, no proxying.
//!
//! # What connected means
//!
//! [`IpcClient::is_connected`] is the field the publishing loop asks before
//! it publishes anything, so it is true only while a socket is held that
//! completed a handshake and whose framing still lines up. A candidate
//! socket becomes the connection after Discord answers its handshake, never
//! before, and any frame exchange that fails takes it back out: a reply
//! read half-way leaves the cursor inside a body, and what follows can no
//! longer be told from a frame header.
//!
//! Losing a connection costs one presence tick — the loop reconnects on the
//! next one — which is why the client fails one it cannot prove instead of
//! keeping the socket.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::log;

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
/// Largest acceptable reply: Discord frames are kilobytes; anything past
/// this is a desync, and must not become an allocation.
const MAX_REPLY_BYTES: u32 = 1024 * 1024;
/// Sockets probed per base directory (`discord-ipc-0..9`).
const MAX_IPC_SOCKETS: u8 = 10;
/// Give up on slow/dead sockets instead of hanging a tick.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Why a client configured with something other than a number refuses to
/// look for a socket at all.
const BAD_APP_ID: &str =
    "Discord application id is not a number; set --app-id to the numeric id of your Discord app";
/// What completes the sentence of an error frame that carried no words.
const UNSAID: &str = "without saying why";

/// One Discord IPC frame: `op` + little-endian length + JSON body.
pub(crate) fn encode_frame(op: u32, payload: &str) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.extend_from_slice(&op.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload.as_bytes());
    frame
}

/// Candidate Discord sockets: `discord-ipc-0..9` under `$XDG_RUNTIME_DIR`
/// (native) and the system temp dir (Flatpak/Snap re-exports), in order.
pub(crate) fn socket_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let bases = [
        std::env::var("XDG_RUNTIME_DIR").ok().map(PathBuf::from),
        Some(std::env::temp_dir()),
    ];
    for base in bases.into_iter().flatten() {
        for n in 0..MAX_IPC_SOCKETS {
            paths.push(base.join(format!("discord-ipc-{n}")));
        }
    }
    // `$XDG_RUNTIME_DIR/discord-ipc-N` covers native installs; the `/tmp`
    // entries cover Flatpak/Snap layouts that re-export sockets there.
    paths
}

/// Connects to one Discord IPC socket and arms the read and write timeouts
/// that every later read and write on it runs under.
///
/// A connected socket is not a usable one: this proves only that something
/// is listening at `path`, not that it is Discord and not that it answers.
/// [`handshake`] is what turns the result into a connection, and until it
/// returns `Ok` the socket belongs to nobody.
///
/// Fails when nothing is listening at `path` — the common case, since the
/// names are probed rather than looked up — and when the timeouts cannot be
/// armed on a socket that did answer.
pub fn try_socket(path: &Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

/// Announces `client_id` to Discord and reads the one reply the handshake
/// owes, leaving `stream` at the first frame that follows it.
///
/// The reply is read, not merely written to: a peer that never answers — a
/// hangup, a silent socket, a read that runs into its timeout — is an error
/// here.
///
/// Two clauses fail a handshake: `evt` set to `ERROR`, or any `code` in
/// `data` — presence alone, whatever the value. Discord's own `message`
/// carries the reason when it sent one, and a reply carrying none is
/// reported without saying why. Nothing else is asked of the reply, so a
/// shape this code has never seen still passes.
///
/// `client_id` is the numeric id of the Discord application to publish as,
/// carried as a string inside the frame. A stream handed in without timeouts
/// blocks until the peer answers; [`try_socket`] is what arms them.
pub fn handshake(stream: &mut UnixStream, client_id: u64) -> io::Result<()> {
    let payload = json!({"v": 1, "client_id": client_id.to_string()});
    write_frame(stream, OP_HANDSHAKE, &payload.to_string())?;
    // Not best-effort: a caller cannot tell a connection that never
    // answered from one whose reply arrived half-read, and only the second
    // leaves the framing out of step with Discord's.
    let reply = read_frame(stream)?;
    // Not shape-strict on purpose: a check demanding today's reply shape
    // would turn any future answer from Discord into a broken connection.
    match refusal(&reply) {
        Some(reason) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Discord refused the handshake {reason}"),
        )),
        None => Ok(()),
    }
}

/// The reason a refusal-shaped reply gives, or `None` for every other reply —
/// including one in a shape this code has never seen.
///
/// Two clauses make a reply refusal-shaped: `evt` is the string `ERROR`, or
/// `data` carries a `code` at all. The second tests presence alone, so a
/// `code` holding JSON `null` or a string marks a refusal just as a number
/// does. The reason is `data.message`, else a top-level `message`; a reply
/// carrying neither is reported without saying why.
///
/// That asymmetry is the contract: a check demanding a known shape would
/// turn any future answer from Discord into a broken connection.
fn refusal(reply: &Value) -> Option<&str> {
    let named_error = reply.get("evt").and_then(Value::as_str) == Some("ERROR");
    let carries_code = reply
        .get("data")
        .and_then(|data| data.get("code"))
        .is_some();
    if !named_error && !carries_code {
        return None;
    }
    Some(
        reply
            .get("data")
            .and_then(|data| data.get("message"))
            .or_else(|| reply.get("message"))
            .and_then(Value::as_str)
            .unwrap_or(UNSAID),
    )
}

/// Minimal Discord client: one Unix socket, handshake on `connect`,
/// `SET_ACTIVITY` per tick. The socket it holds is a handshake that
/// Discord answered; see the module docs.
pub struct IpcClient {
    // `None` marks a configured application id that is not a number: no
    // handshake can announce it, so such a client says so once at
    // construction and never connects, instead of failing on every tick.
    client_id: Option<u64>,
    stream: Option<UnixStream>,
    nonce: u64,
}

impl IpcClient {
    /// Unconnected client for one Discord application id. A value that is
    /// not a number is reported once here and leaves a client that never
    /// connects; no other I/O happens until [`IpcClient::connect`].
    pub fn new(client_id: impl Into<String>) -> Self {
        let client_id = client_id.into();
        let parsed = client_id.trim().parse::<u64>().ok();
        if parsed.is_none() {
            log::warn(format!(
                "Discord application id '{client_id}' is not a number; no presence can be published"
            ));
        }
        Self {
            client_id: parsed,
            stream: None,
            nonce: 0,
        }
    }

    /// `true` only while the client holds a socket that completed a
    /// handshake.
    ///
    /// [`connect`](Self::connect) promotes a socket after Discord answers
    /// it, and both [`close`](Self::close) and a frame exchange that fails
    /// take it back out. One that failed part-way through a reply is not a
    /// connection, whatever this field says: the read cursor can sit inside
    /// a body, and the next read would take its header from body bytes. So
    /// the next tick reconnects instead of reading from it.
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// Connects to Discord and leaves the client connected, or reports why it
    /// could not.
    ///
    /// Probes `discord-ipc-0..9` under `$XDG_RUNTIME_DIR` and the system temp
    /// dir in order, and stops at the first candidate that both connects
    /// ([`try_socket`]) and completes a handshake ([`handshake`]). Only that
    /// candidate becomes the connection.
    ///
    /// A candidate that answers the connection but not the handshake is
    /// dropped and the search moves on. When the candidates run out the client
    /// is left disconnected — including a connection that was open before the
    /// call, which `connect` replaces as a whole — and the error says either
    /// that no socket was there at all, or which candidate ended the search
    /// and what its handshake returned — a refusal Discord stated, a hangup,
    /// a timeout, or any other error `handshake` reports, carried as it
    /// stands rather than summarized.
    ///
    /// The error names a cause rather than an attempt, and how often to call
    /// again is the caller's decision.
    pub fn connect(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let Some(client_id) = self.client_id else {
            self.stream = None;
            return Err(BAD_APP_ID.into());
        };
        let mut refused = None;
        // A handshake that fails on one candidate does not end the search: a
        // Discord that re-exports its socket in several places serves only
        // one of them, so one answer is evidence about that candidate alone.
        for path in socket_paths() {
            let Ok(mut candidate) = try_socket(&path) else {
                continue;
            };
            match handshake(&mut candidate, client_id) {
                Ok(()) => {
                    self.stream = Some(candidate);
                    return Ok(());
                }
                Err(err) => refused = Some((path, err)),
            }
        }
        self.stream = None;
        match refused {
            Some((path, err)) => Err(format!(
                "Discord did not complete the handshake on {}: {err}",
                path.display()
            )
            .into()),
            None => Err("no Discord IPC socket found (is Discord running?)".into()),
        }
    }

    /// Publishes `activity` for `pid` and reads the reply it is owed.
    ///
    /// Fails when the frame cannot be written or the reply does not arrive
    /// whole, and in both cases leaves the client disconnected, because a
    /// socket that lost an exchange is one the next tick must rebuild.
    /// Calling [`close`](Self::close) afterwards is still right and costs
    /// nothing.
    pub fn set_activity(
        &mut self,
        pid: u64,
        activity: Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.nonce += 1;
        let payload = json!({
          "cmd": "SET_ACTIVITY",
          "args": {"pid": pid, "activity": activity},
          "nonce": self.nonce.to_string(),
        });
        self.send(OP_FRAME, &payload.to_string())?;
        self.read_reply()?;
        Ok(())
    }

    /// Best-effort clear; never fails the caller.
    ///
    /// Publishes an empty activity for `pid`, which is how Discord drops
    /// the presence that was published for that pid. A socket that fails
    /// either direction of that exchange is dropped like any other, so the
    /// next tick reconnects instead of reusing a stream whose framing is out
    /// of step.
    pub fn clear(&mut self, pid: u64) {
        self.nonce += 1;
        let payload = json!({
          "cmd": "SET_ACTIVITY",
          "args": {"pid": pid, "activity": Value::Null},
          "nonce": self.nonce.to_string(),
        });
        if let Ok(body) = serde_json::to_string(&payload) {
            let _ = self.send(OP_FRAME, &body);
            let _ = self.read_reply();
        }
    }

    /// Drop the socket without a goodbye frame; the next `connect` starts
    /// over. Idempotent.
    pub fn close(&mut self) {
        self.stream = None;
    }

    /// Writes one frame, dropping the socket when the write fails: a peer
    /// that will not take a frame is gone, and the client must not keep
    /// calling itself connected to it.
    fn send(&mut self, op: u32, payload: &str) -> io::Result<()> {
        let sent = match self.stream.as_mut() {
            Some(stream) => write_frame(stream, op, payload),
            None => Err(not_connected()),
        };
        if sent.is_err() {
            self.stream = None;
        }
        sent
    }

    /// Reads one reply frame, dropping the socket unless it arrives whole.
    ///
    /// A reply that stops mid-body leaves the read cursor inside that body,
    /// so the next read takes its eight header bytes from body bytes and
    /// every frame after that is a guess. Only a new connection can undo
    /// that, so the socket goes out instead of being read again; what the
    /// client writes is still well-framed — it is the replies it can no
    /// longer read.
    fn read_reply(&mut self) -> io::Result<Value> {
        let reply = match self.stream.as_mut() {
            Some(stream) => read_frame(stream),
            None => Err(not_connected()),
        };
        if reply.is_err() {
            self.stream = None;
        }
        reply
    }
}

/// The error a frame exchange reports when the client holds no socket to
/// exchange it over.
fn not_connected() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "Discord IPC not connected")
}

/// Writes one frame and flushes it; the socket carries no userspace
/// buffering, so the flush only reports a failure the write did not.
fn write_frame(stream: &mut UnixStream, op: u32, payload: &str) -> io::Result<()> {
    stream.write_all(&encode_frame(op, payload))?;
    stream.flush()
}

/// Reads exactly one frame: an 8-byte header and the body it announces.
fn read_frame(stream: &mut UnixStream) -> io::Result<Value> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header)?;
    let len = u32::from_le_bytes(
        header[4..8]
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "short IPC header"))?,
    );
    if len > MAX_REPLY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC reply exceeds sanity size",
        ));
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
