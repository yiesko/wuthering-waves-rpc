// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Integration tests for [`wwrpc::ipc`]: what "connected" may mean.
//!
//! [`IpcClient::is_connected`] is the one field the publishing loop asks
//! before it publishes, so it must be true only over a socket Discord
//! answered and whose framing still lines up. Every test here stands a fake
//! Discord behind a real Unix socket inside a sandboxed runtime directory:
//! no Discord, no game install, and no timing beyond what the test itself
//! writes.
//!
//! A `UnixStream::pair` is enough to pin the handshake itself, but nothing
//! short of a listening socket can put a client through a connection that
//! dies under it or answers half a frame: those are failures a peer only
//! shows. So each test plants a stand-in that misbehaves in exactly one
//! way.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use serde_json::json;

use wwrpc::ipc::{IpcClient, handshake};

/// Application id the stand-in is addressed with; any number reaches the
/// same handshake code.
const APP_ID: &str = "1546176429048463360";

/// [`APP_ID`] as the number a handshake announces.
const APP_ID_NUMBER: u64 = 1_546_176_429_048_463_360;

/// Pid the activity tests publish for; a socket does not care about it.
const PID: u64 = 4242;

/// The environment variable that picks the first directory the client
/// searches for a socket.
const XDG: &str = "XDG_RUNTIME_DIR";

/// The environment variable `std::env::temp_dir()` honors: the other
/// directory the client searches.
const TMPDIR: &str = "TMPDIR";

/// The socket name a sandbox exposes, first in the client's search order.
const SOCKET: &str = "discord-ipc-0";

/// Serializes this file: every test moves the process-wide variables that
/// decide where the client looks for a socket.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Holds [`ENV_LOCK`] for one test and restores both variables on drop, so
/// a sandbox never outlives the test that pointed the client at it.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: [(&'static str, Option<OsString>); 2],
}

impl EnvGuard {
    /// Take the file-wide lock and snapshot the two searched variables.
    fn new() -> Self {
        // A panicking test must not wedge every later one.
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self {
            _lock: lock,
            saved: [XDG, TMPDIR].map(|key| (key, std::env::var_os(key))),
        }
    }

    /// Point one searched variable at `dir`.
    fn root(&self, key: &str, dir: &Path) {
        let dir = dir.to_str().expect("a UTF-8 sandbox path");
        // SAFETY: only this guard mutates the environment (the file-wide
        // lock is held for the test's whole life) and the values only steer
        // the socket search inside the client.
        unsafe { std::env::set_var(key, dir) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: restoring the snapshot taken before the test ran, still
        // under the lock this guard holds.
        unsafe {
            for (key, previous) in &self.saved {
                match previous {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// Two private directories the client searches, plus the env guard that
/// keeps it looking there.
///
/// The client searches `$XDG_RUNTIME_DIR` and the system temp dir for
/// `discord-ipc-0..9`, so each gets its own directory: the sockets a test
/// plants in one, and an empty one in the other. Aliasing them would make
/// every planted socket appear twice in the probe order — the same path
/// probed again ten entries later, into a listener that may already be gone,
/// which is a five-second wait for a connection that is no longer there.
/// The directories are created from the real temp dir before the variables
/// move.
struct Sandbox {
    _env: EnvGuard,
    base: PathBuf,
    sockets: PathBuf,
}

impl Sandbox {
    /// Fresh directories, the second one deliberately empty: "no Discord at
    /// all" is expressed by planting nothing in either.
    fn new(tag: &str) -> Self {
        let env = EnvGuard::new();
        let base = std::env::temp_dir().join(format!("wwrpc-ipc-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sockets = base.join("sockets");
        let empty = base.join("empty");
        for dir in [&sockets, &empty] {
            std::fs::create_dir_all(dir).expect("the sandbox dir is created");
        }
        env.root(XDG, &sockets);
        env.root(TMPDIR, &empty);
        Self {
            _env: env,
            base,
            sockets,
        }
    }

    /// The path the stand-in serves, first in the client's probe order.
    fn socket(&self) -> PathBuf {
        self.sockets.join(SOCKET)
    }

    /// Bind that path; a stand-in takes it from here.
    fn listen(&self) -> UnixListener {
        UnixListener::bind(self.socket()).expect("the sandbox socket is bindable")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Read one whole frame off a stand-in's socket and hand back its body, so
/// the client sees its handshake arrive the way a real server sends it.
fn read_frame(peer: &mut UnixStream) -> String {
    let mut header = [0u8; 8];
    peer.read_exact(&mut header)
        .expect("the handshake frame arrives whole");
    let len = u32::from_le_bytes(header[4..8].try_into().expect("an 8-byte header"));
    let mut body = vec![0u8; len as usize];
    peer.read_exact(&mut body)
        .expect("the handshake body arrives whole");
    String::from_utf8_lossy(&body).into_owned()
}

/// Check that the handshake announced [`APP_ID`] the way Discord reads it:
/// as a string, even though the client holds the id as a number.
fn expect_handshake(body: &str) {
    assert!(
        body.contains(&format!(r#""client_id":"{APP_ID}""#)),
        "the handshake announces the application id as a string: {body}"
    );
}

/// One reply frame carrying `body`, shaped like every frame Discord sends.
fn frame(body: &str) -> Vec<u8> {
    let mut reply = Vec::with_capacity(8 + body.len());
    reply.extend_from_slice(&1u32.to_le_bytes());
    reply.extend_from_slice(&(body.len() as u32).to_le_bytes());
    reply.extend_from_slice(body.as_bytes());
    reply
}

/// The answer a handshake is accepted with: an event frame announcing the
/// connection is ready.
fn accepted() -> Vec<u8> {
    frame(r#"{"cmd":"DISPATCH","data":{"evt":"READY"},"evt":"READY"}"#)
}

/// The answer a handshake is refused with: an error event carrying Discord's
/// own code and its own words.
fn refused() -> Vec<u8> {
    frame(r#"{"cmd":"ERROR","evt":"ERROR","data":{"code":4000,"message":"Invalid Client ID"}}"#)
}

/// The answer in a shape this client has never seen, and neither an error
/// event nor an error code.
fn unfamiliar() -> Vec<u8> {
    frame(r#"{"cmd":"DISPATCH","evt":"ANNOUNCED","data":{"shape":"unseen"}}"#)
}

/// Stand-in Discord that accepts a connection and hangs up without
/// answering: the handshake frame goes out and the reply never comes.
fn accept_then_hang_up(listener: UnixListener) -> JoinHandle<()> {
    thread::spawn(move || {
        let (peer, _) = listener.accept().expect("the client connects");
        drop(peer);
    })
}

/// Stand-in Discord that answers the handshake with `body` and then closes
/// the connection completely. The frame is owned by the thread, which has to
/// outlive this call.
fn answer_with(listener: UnixListener, body: Vec<u8>) -> JoinHandle<()> {
    thread::spawn(move || {
        let (mut peer, _) = listener.accept().expect("the client connects");
        expect_handshake(&read_frame(&mut peer));
        let _ = peer.write_all(&body);
    })
}

/// Stand-in Discord that answers the handshake, then closes only its
/// writing side and holds the reading side open until `release` is dropped.
///
/// To the client this is a reply that stops halfway: its own writes keep
/// landing, and its next read finds end of stream where a frame should
/// start.
fn answer_then_half_close(listener: UnixListener) -> (JoinHandle<()>, Sender<()>) {
    // `release` is the sender the caller keeps and drops; `held` is the
    // receiver this thread waits on, so the reading side stays open until
    // then.
    let (release, held) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut peer, _) = listener.accept().expect("the client connects");
        expect_handshake(&read_frame(&mut peer));
        peer.write_all(&accepted()).expect("the reply is written");
        peer.flush().expect("the reply is flushed");
        peer.shutdown(Shutdown::Write)
            .expect("the writing side closes");
        let _ = held.recv();
    });
    (server, release)
}

/// A socket that connects and never answers the handshake is not a
/// connection: the client must come out of the attempt reporting failure,
/// and must not be connected afterwards. The handshake is the one exchange
/// the client cannot do without — a reply that never arrives is a failure,
/// not something to paper over and blame on the next write.
#[test]
fn test_handshake_failure_leaves_disconnected() {
    // The direct half of the contract: nothing on this stream ever answers,
    // so the handshake must fail instead of returning quietly.
    let (mut socket, peer) = UnixStream::pair().expect("the socket pair is created");
    drop(peer);
    assert!(
        handshake(&mut socket, APP_ID_NUMBER).is_err(),
        "a handshake nobody answers cannot succeed"
    );

    let sandbox = Sandbox::new("handshake");
    let server = accept_then_hang_up(sandbox.listen());
    let mut client = IpcClient::new(APP_ID);
    let connected = client.connect();
    server.join().expect("the stand-in finishes");

    assert!(
        connected.is_err(),
        "a handshake that never completes cannot connect: {connected:?}"
    );
    assert!(
        !client.is_connected(),
        "a client whose handshake failed must not report itself connected"
    );
}

/// A connection that dies under a live client must leave that client
/// disconnected, so the next tick reconnects instead of writing into a
/// socket nothing is listening on.
#[test]
fn test_send_error_on_activity_closes_stream() {
    let sandbox = Sandbox::new("hang-up");
    let server = answer_with(sandbox.listen(), accepted());
    let mut client = IpcClient::new(APP_ID);
    client
        .connect()
        .expect("the stand-in answers the handshake");
    server.join().expect("the stand-in hangs up");
    assert!(
        client.is_connected(),
        "a completed handshake is a connection"
    );

    let published = client.set_activity(PID, json!({"details": "Checking In"}));

    assert!(
        published.is_err(),
        "the peer is gone, so the activity cannot be acknowledged: {published:?}"
    );
    assert!(
        !client.is_connected(),
        "a client that could not deliver its activity must not report itself connected"
    );
}

/// A reply that stops halfway cannot be told apart from one that is merely
/// late, and it leaves the read cursor inside a body: the client must drop
/// the socket rather than keep calling itself connected and read the bytes
/// that follow as if they were a frame header.
#[test]
fn test_reply_error_on_activity_closes_stream() {
    let sandbox = Sandbox::new("half-close");
    let (server, release) = answer_then_half_close(sandbox.listen());
    let mut client = IpcClient::new(APP_ID);
    client
        .connect()
        .expect("the stand-in answers the handshake");
    assert!(
        client.is_connected(),
        "a completed handshake is a connection"
    );

    let published = client.set_activity(PID, json!({"details": "Checking In"}));
    drop(release);
    server.join().expect("the stand-in finishes");

    assert!(
        published.is_err(),
        "the reply ends where a frame should still start: {published:?}"
    );
    assert!(
        !client.is_connected(),
        "an unreadable reply leaves the framing out of step, so the client must reconnect"
    );
}

/// A reply that says the handshake failed must fail it, in Discord's own
/// words. An application id Discord will not accept is otherwise diagnosed
/// only by a presence that never arrives, and the operator is left guessing
/// which of the two ends is broken.
#[test]
fn test_error_reply_fails_the_handshake_with_discords_reason() {
    let sandbox = Sandbox::new("refused");
    let server = answer_with(sandbox.listen(), refused());
    let mut client = IpcClient::new(APP_ID);

    let connected = client.connect();
    server.join().expect("the stand-in finishes");

    let complaint = connected
        .expect_err("an error frame cannot be a completed handshake")
        .to_string();
    assert!(
        complaint.contains("Invalid Client ID"),
        "the refusal must carry Discord's own reason: {complaint}"
    );
    assert!(
        !client.is_connected(),
        "a refused handshake must not report itself connected"
    );
}

/// A reply in a shape this client has never seen must still be accepted.
/// This is the half that protects every working connection: a check that
/// demanded today's reply shape would turn the first answer Discord changes
/// into a broken connection, and a test saying so is what stops someone
/// tightening it later.
#[test]
fn test_unfamiliar_reply_shape_still_connects() {
    let sandbox = Sandbox::new("unfamiliar");
    let server = answer_with(sandbox.listen(), unfamiliar());
    let mut client = IpcClient::new(APP_ID);

    let connected = client.connect();
    server.join().expect("the stand-in finishes");

    assert!(
        connected.is_ok(),
        "an unfamiliar reply is not a refusal: {connected:?}"
    );
    assert!(
        client.is_connected(),
        "a handshake nobody refused is a connection"
    );
}

/// An application id that is not a number cannot be announced in a
/// handshake, so connecting must say so by name instead of failing as if
/// Discord were absent — even when a socket is waiting there and would
/// otherwise answer. A loop that dials again every tick must get the same
/// complaint every time: a cause the user can act on, not a new one per
/// attempt.
#[test]
fn test_non_numeric_app_id_fails_before_any_socket() {
    let sandbox = Sandbox::new("app-id");
    let server = accept_then_hang_up(sandbox.listen());
    let mut client = IpcClient::new("not-a-number");

    let mut complaints = Vec::new();
    for _ in 0..3 {
        let attempt = client.connect();
        assert!(
            attempt.is_err(),
            "an id that is not a number cannot complete a handshake: {attempt:?}"
        );
        let complaint = attempt.unwrap_err().to_string();
        assert!(
            complaint.contains("--app-id"),
            "the failure must name the flag that fixes it: {complaint}"
        );
        complaints.push(complaint);
    }
    assert!(
        complaints.windows(2).all(|pair| pair[0] == pair[1]),
        "every retry must report the same cause: {complaints:?}"
    );
    assert!(
        !client.is_connected(),
        "a client that never handshaked must not report itself connected"
    );

    // The client never dialed, so the stand-in is still parked in `accept`.
    // One throwaway connection releases it instead of leaving a thread
    // parked for the life of this binary.
    UnixStream::connect(sandbox.socket()).expect("the stand-in is still reachable");
    server.join().expect("the stand-in finishes");
}

/// A client that finds nothing at all must say so in the terms a user can
/// act on: Discord is not running, which is a different problem from a
/// Discord that answered the connection and then stopped.
#[test]
fn test_no_socket_anywhere_reports_discord_absent() {
    let _sandbox = Sandbox::new("absent");
    let mut client = IpcClient::new(APP_ID);

    let connected = client.connect();

    assert!(
        connected.is_err(),
        "no planted socket means no connection: {connected:?}"
    );
    let complaint = connected.unwrap_err().to_string();
    assert!(
        complaint.contains("no Discord IPC socket found"),
        "an empty probe must not be reported as a refused handshake: {complaint}"
    );
    assert!(
        !client.is_connected(),
        "a client that never handshaked must not report itself connected"
    );
}
