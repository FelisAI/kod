//! Attaching to the daemon, and the one hazard that makes this dangerous.
//!
//! ## The retire hazard — and what does NOT protect you from it
//!
//! `attach_gate` is:
//!
//! ```text
//! WireGate::Accept if rebuilt => AttachGate::Retire
//! ```
//!
//! and the daemon's own test pins it: `attach_gate(7, 7, true) == Retire`.
//! **A MATCHING wire version still retires the daemon if its binary was rebuilt
//! since it launched** — and retiring means exit, taking every live agent
//! session with it.
//!
//! So linking `orchestrator_host::protocol::WIRE_VERSION` (which this crate does,
//! and should) buys protection against a *different*, lesser hazard: announcing a
//! version the daemon rejects. It buys NOTHING against the rebuild case. An
//! earlier version of this comment claimed otherwise, which is exactly the kind
//! of false assurance that gets sessions killed.
//!
//! The real defences, all of them structural:
//!   1. [`retire_risk`] — a pre-flight that refuses to connect when the daemon
//!      binary is newer than the socket, i.e. when attaching WOULD retire.
//!   2. Exactly ONE attach per process lifetime. There is no reconnect timer
//!      anywhere in this crate, because a retry loop against a rebuilt daemon is
//!      a loop that kills a daemon, notices it died, and kills the next one.
//!   3. The default-socket refusal in `main`.
//!
//! ## Why there is no default socket path
//!
//! `daemon::default_socket_path()` would resolve to whichever daemon is running
//! for this user — in development that is the REAL one, owning real sessions.
//! Attaching a freshly-built binary to it is exactly the retire case above. So
//! the path is a required argument with no default: connecting to the wrong
//! daemon should require typing its name.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::SystemTime;

use orchestrator_host::protocol::{
    read_frame, write_frame, ClientMsg, Command, ServerMsg, WIRE_VERSION,
};

/// A live attachment to a daemon.
pub struct Client {
    stream: UnixStream,
    next_request_id: u64,
}

/// What went wrong attaching, kept separate from `io::Error` because the
/// version case is not a transport failure and must never be retried blindly.
#[derive(Debug)]
pub enum AttachError {
    Io(io::Error),
    /// The daemon speaks a different wire. Retrying cannot help, and hammering
    /// it is how you turn one bad attach into a retire loop.
    VersionMismatch { ours: u32, daemon: u32 },
    /// The daemon's first frame was not `Welcome`.
    Unexpected(String),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::VersionMismatch { ours, daemon } => write!(
                f,
                "wire mismatch: bridge speaks {ours}, daemon speaks {daemon} — \
                 rebuild both from the same tree"
            ),
            Self::Unexpected(s) => write!(f, "unexpected first frame: {s}"),
        }
    }
}

impl From<io::Error> for AttachError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl Client {
    /// Attach to the daemon at `path` and complete the handshake.
    ///
    /// Returns the initial session list from `Welcome`. The caller should then
    /// pump [`Client::next`] until `ReplayDone` to receive the opening grid per
    /// session, exactly as the desktop GUI does.
    pub fn attach(path: &Path) -> Result<(Self, ServerMsg), AttachError> {
        let mut stream = UnixStream::connect(path)?;
        // The ONLY version we ever announce. See the module note.
        write_frame(
            &mut stream,
            &ClientMsg::Hello {
                wire_version: WIRE_VERSION,
            },
        )?;
        let first: ServerMsg = read_frame(&mut stream)?;
        match first {
            ServerMsg::Welcome { .. } => Ok((
                Self {
                    stream,
                    next_request_id: 1,
                },
                first,
            )),
            ServerMsg::VersionMismatch { daemon_version } => Err(AttachError::VersionMismatch {
                ours: WIRE_VERSION,
                daemon: daemon_version,
            }),
            other => Err(AttachError::Unexpected(format!("{other:?}"))),
        }
    }

    /// Block for the next message from the daemon.
    pub fn next(&mut self) -> io::Result<ServerMsg> {
        read_frame(&mut self.stream)
    }

    /// Send a command. Returns its `request_id` so a reply can be correlated.
    ///
    /// Nothing here decides WHICH commands are allowed — that policy lives at
    /// the network edge, where the untrusted input is. Putting an allowlist here
    /// too would be two places to keep in step, and the one that mattered would
    /// be the one that was forgotten.
    pub fn send(&mut self, command: Command) -> io::Result<u64> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        write_frame(&mut self.stream, &ClientMsg::Request { request_id, command })?;
        Ok(request_id)
    }
}

/// Whether attaching right now would retire the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireRisk {
    /// The daemon launched from the binary that is on disk now — a normal attach.
    Safe,
    /// The binary is NEWER than the running daemon. Attaching triggers
    /// `AttachGate::Retire` and every live session dies.
    WouldRetire,
    /// Could not tell (a path is missing, or the filesystem has no mtime). Treated
    /// as unsafe by callers: not knowing is not the same as knowing it is fine.
    Unknown,
}

/// Compare a daemon binary's mtime against its socket's.
///
/// WHY THE SOCKET IS A PROXY FOR "WHEN THE DAEMON STARTED": the socket file is
/// created by `UnixListener::bind` at startup, so its mtime IS the daemon's launch
/// time — no process introspection, no ps, no platform APIs. If the binary is newer
/// than that, the daemon is running code that no longer exists on disk, which is
/// precisely `binary_was_rebuilt()`.
pub fn compare_mtimes(binary: Option<SystemTime>, socket: Option<SystemTime>) -> RetireRisk {
    match (binary, socket) {
        (Some(b), Some(s)) if b > s => RetireRisk::WouldRetire,
        (Some(_), Some(_)) => RetireRisk::Safe,
        _ => RetireRisk::Unknown,
    }
}

/// The pre-flight. Call this BEFORE `Client::attach`.
pub fn retire_risk(socket: &Path, daemon_binary: &Path) -> RetireRisk {
    let m = |p: &Path| std::fs::metadata(p).ok().and_then(|m| m.modified().ok());
    compare_mtimes(m(daemon_binary), m(socket))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const T0: SystemTime = SystemTime::UNIX_EPOCH;
    fn t(secs: u64) -> Option<SystemTime> {
        Some(T0 + Duration::from_secs(secs))
    }

    #[test]
    fn a_binary_newer_than_the_socket_would_retire() {
        // The daemon started at 100 and someone ran cargo build at 200. Attaching
        // now kills every live session.
        assert_eq!(compare_mtimes(t(200), t(100)), RetireRisk::WouldRetire);
    }

    #[test]
    fn a_daemon_started_after_its_binary_is_safe() {
        assert_eq!(compare_mtimes(t(100), t(200)), RetireRisk::Safe);
        // exactly equal is safe: the daemon launched from this very binary.
        assert_eq!(compare_mtimes(t(100), t(100)), RetireRisk::Safe);
    }

    #[test]
    fn not_knowing_is_not_the_same_as_safe() {
        // Callers must treat Unknown as refuse. A missing binary or a filesystem
        // without mtimes must never read as "go ahead".
        assert_eq!(compare_mtimes(None, t(100)), RetireRisk::Unknown);
        assert_eq!(compare_mtimes(t(100), None), RetireRisk::Unknown);
        assert_eq!(compare_mtimes(None, None), RetireRisk::Unknown);
        assert_ne!(compare_mtimes(None, None), RetireRisk::Safe);
    }
}
