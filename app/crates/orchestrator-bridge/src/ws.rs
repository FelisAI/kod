//! The phone-facing WebSocket server.
//!
//! ## Shape
//!
//! Three pieces, split so that everything worth testing is testable with
//! nothing connected:
//!
//!   * [`Protocol`] — the per-connection state machine. Bytes in, decisions out.
//!     No socket, no hub, no clock. Auth lives here.
//!   * [`Hub`] — the shared session view + the fan-out. Owns the epoch and the
//!     per-`(epoch, sid)` rev counters. No socket.
//!   * [`serve`] / [`conn`] — the only code that touches a file descriptor, and
//!     deliberately thin enough to read in one sitting.
//!
//! That split is not tidiness. The alternative — a protocol that only exists
//! inside an accept loop — is a protocol whose auth and framing rules can only
//! be exercised by pointing a real client at a real bridge attached to a real
//! daemon, and in this crate a wrong attach RETIRES the daemon and kills every
//! live session (`client.rs`). So the socket layer is kept boring on purpose.
//!
//! ## Threading
//!
//! Blocking, thread-per-connection. `tungstenite`'s sync API, not tokio: the
//! bridge's other half is already a blocking unix-socket client, and the entire
//! expected load is a handful of phones belonging to one person. An async
//! runtime here would buy nothing and colour every function.
//!
//! A connection thread must both read (ping) and write (deltas), so instead of a
//! second thread per connection it polls: a short read timeout on the socket
//! turns the blocking read into "check for a frame", and the outbound queue is
//! drained on every turn of the same loop.
//!
//! ## Getting a phone's typing to the daemon
//!
//! A connection thread has a `Hub` and a token and no way to reach the daemon at
//! all, and it must not simply be handed the daemon connection: that is one
//! request/reply stream, and two threads writing to it interleave frames into
//! something neither end can parse. So the path is one-way per thread:
//!
//!   * every CONNECTION thread holds a clone of `Sender<PhoneRequest>`. It posts
//!     a [`Command`] plus a one-shot and waits on that one-shot. It never touches
//!     the daemon socket.
//!   * the SENDER thread owns the WRITE half and blocks on the channel — never on
//!     the daemon stream. Input therefore leaves the instant it is posted, no
//!     matter how quiet the daemon is.
//!   * the PUMP thread owns the READ half and blocks on it — never on the
//!     channel. Session updates keep flowing while any number of inputs are in
//!     flight, so a phone that spams input competes for the SENDER's attention
//!     and not the pump's.
//!
//! The single-threaded version fails whichever way you point it. Drain the
//! channel after each daemon message and a keystroke sits unsent until the daemon
//! happens to speak — and an all-idle daemon says NOTHING, which is precisely the
//! moment someone picks up their phone and types. Wait on the channel instead and
//! the session stream stops. Splitting the socket is what removes the choice; it
//! is also why the daemon connection is opened here rather than through
//! [`crate::client::Client`] — see [`attach_split`].
//!
//! ## Binding
//!
//! Loopback always; a Tailscale address as an explicit opt-in; nothing else,
//! ever.
//!
//! An earlier version of this note claimed Tailscale "terminates as loopback"
//! and bound 127.0.0.1 only. That is false, and it made the phone case
//! impossible: an SSH tunnel does terminate as loopback, but Tailscale gives the
//! Mac its own 100.x address on a utun interface, and a loopback-bound listener
//! REFUSES those connections. Measured, not assumed — connecting to the tailnet
//! address gave ECONNREFUSED while 127.0.0.1 accepted.
//!
//! So the policy is a range check, not a blanket ban. v0 has one shared bearer
//! token and no TLS:
//!
//! * loopback — safe by construction, and what an SSH tunnel presents.
//! * a tailnet address (100.64.0.0/10) — WireGuard already authenticates the
//!   device and encrypts the hop, so the token is not crossing anything in the
//!   clear. This is the case the phone actually needs.
//! * anything else — REFUSED. 0.0.0.0 or a LAN address would put a plaintext
//!   bearer token on café wifi, which is the downgrade the old note feared.
//!
//! When a tailnet bind is configured the bridge listens on BOTH it and loopback,
//! so the simulator and the phone can be connected at the same time.

use std::collections::{BTreeMap, HashMap};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

use orchestrator_host::protocol::{
    read_frame, write_frame, ClientMsg, ClientRole, Command, CommandReply, PhoneKey, ServerMsg,
    WIRE_VERSION,
};
use orchestrator_host::session::SessionId;

use crate::client::AttachError;
use crate::mirror::{Change, Mirror};
use crate::wire::{
    decode_frame, BridgeMsg, Caps, PhoneMsg, WireSession, MAX_FRAME, PROTO,
};

/// The port the phone dials when nothing says otherwise.
pub const DEFAULT_PORT: u16 = 8787;

/// How long a connection may sit between reads before the loop checks its
/// outbound queue. Short enough that a phase change reaches the phone promptly,
/// long enough that an idle connection is not a spin loop.
const POLL: Duration = Duration::from_millis(100);

/// The same idea one layer up: the listener is non-blocking so the accept thread
/// can look at the stop flag between tries. It is also the worst case between
/// "stop" and the port being free, so it is short — a GUI that stops and
/// restarts the bridge waits this long, once.
const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// A socket that has connected but not yet said `hello` is holding a thread
/// while UNAUTHENTICATED. It gets this long, TOTAL, to produce one — a per-read
/// timeout would not be enough, because a peer that dribbles a websocket ping
/// every nine seconds would renew it forever and never authenticate.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// An ESTABLISHED connection that has produced nothing for this long is gone,
/// whatever TCP still believes. Nothing else reaps a half-open connection: a
/// phone that walks out of wifi never sends a FIN, so without this its thread
/// and its [`Hub`] subscription live forever and `sub_count` counts corpses.
///
/// 60s is deliberately LOOSER than the phone's own rule — the shipped client
/// pings every 20s and gives up on silence after 45s
/// (`ios/Kod/Net/BridgeClient.swift`) — so a live phone refreshes this three
/// times over, and a link the phone itself would have abandoned is reaped here
/// too rather than lingering.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// A peer that stops READING must not pin a connection thread inside a write
/// either. Applies to both phases; the phone's traffic is tiny, so anything this
/// slow is a dead link.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on a single client's unsent backlog. A phone that stops reading (a
/// tunnel that black-holes, a screen-locked device on a dead network) must not
/// grow the bridge's memory without limit, and it must not be allowed to slow
/// the daemon pump either — so it is DISCONNECTED rather than waited on. It
/// resyncs from a fresh snapshot when it comes back, which is exactly what the
/// epoch/rev design is for.
const MAX_BACKLOG: usize = 4096;

/// How many phones may be connected at once. One person owns every device that
/// can reach this listener, so eight is generous; the cap exists because each
/// connection costs a thread and an unbounded accept loop is a free denial of
/// service for anything that can open a socket.
///
/// THIS IS ONLY SAFE BECAUSE OF [`IDLE_TIMEOUT`]. A cap over a leak is worse
/// than no cap: without a reaper the eight slots fill with connections that
/// died on a train, nothing ever gives one back, and the bridge is locked out
/// permanently — the user sees "cannot connect" from a server that reports
/// itself healthy. The counter is decremented when the connection thread exits,
/// panic included.
const MAX_CONNS: usize = 8;

/// `tungstenite`'s own ceiling. It sits ABOVE [`MAX_FRAME`] on purpose: the
/// protocol limit is enforced by us (in [`decode_frame`]) so that an oversized
/// frame can be answered with `err` and the connection KEPT, per the contract. A
/// tungstenite `Capacity` error, by contrast, is fatal to the connection — so it
/// is positioned as a memory backstop for a peer that ignores the answer, not as
/// the protocol rule.
const HARD_FRAME_CEILING: usize = MAX_FRAME * 2;

// --------------------------------------------------------------------- config

/// Where the phone-facing listener is allowed to live.
///
/// Deliberately NOT a bare `IpAddr`: the whole safety argument is that only two
/// kinds of address are acceptable, so the type refuses to represent a third.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bind {
    /// 127.0.0.1 only. The default, and what an SSH tunnel presents.
    Loopback,
    /// Loopback AND this tailnet address, for a phone on the same tailnet.
    Tailnet(Ipv4Addr),
}

/// Tailscale hands out addresses from the CGNAT block 100.64.0.0/10 — that is
/// the second octet running 64..=127, NOT "anything starting with 100". 100.0.x
/// and 100.128.x are ordinary public addresses and must not be mistaken for a
/// tailnet.
pub fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            a == 100 && (64..=127).contains(&b)
        }
        // Tailscale's IPv6 range is fd7a:115c:a1e0::/48.
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// Resolve `KOD_BRIDGE_BIND`. Unset means loopback; anything set must be a
/// tailnet address, and every other value is refused WITH THE REASON — a silent
/// downgrade to loopback would leave the user staring at a phone that cannot
/// connect and no explanation on the terminal.
pub fn parse_bind(raw: Option<String>) -> Result<Bind, String> {
    let Some(raw) = raw else { return Ok(Bind::Loopback) };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Bind::Loopback);
    }
    if raw.eq_ignore_ascii_case("loopback") || raw == "127.0.0.1" {
        return Ok(Bind::Loopback);
    }
    let ip: Ipv4Addr = raw.parse().map_err(|_| {
        format!(
            "KOD_BRIDGE_BIND={raw} is not an IPv4 address. Set it to your Tailscale \
             address (`tailscale ip -4`), or leave it unset for loopback."
        )
    })?;
    if !is_tailnet(IpAddr::V4(ip)) {
        return Err(format!(
            "KOD_BRIDGE_BIND={ip} is not a Tailscale address (100.64.0.0/10). The bridge \
             refuses it: v0 authenticates with one shared bearer token and has no TLS, so \
             binding a LAN or wildcard address would put that token in the clear on whatever \
             network you are on. Use `tailscale ip -4`, or an SSH tunnel to loopback."
        ));
    }
    Ok(Bind::Tailnet(ip))
}

/// Whether a connected peer is allowed to speak the protocol at all.
///
/// This runs BEFORE the token is read, so a peer that should not be able to
/// reach the bridge never gets to present (or brute-force) a credential.
pub fn peer_allowed(peer: IpAddr, bind: Bind) -> bool {
    if peer.is_loopback() {
        return true;
    }
    matches!(bind, Bind::Tailnet(_)) && is_tailnet(peer)
}

/// Everything `serve` reads from the environment, resolved up front so a
/// misconfigured bridge fails before it attaches to anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub token: String,
    pub port: u16,
    pub bind: Bind,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::parse(
            std::env::var("KOD_BRIDGE_TOKEN").ok(),
            std::env::var("KOD_BRIDGE_PORT").ok(),
            std::env::var("KOD_BRIDGE_BIND").ok(),
        )
    }

    /// The pure half, so the rules are tested without mutating process env
    /// (which is global and races every other test in the binary).
    ///
    /// THERE IS NO DEFAULT TOKEN and no fallback. A default would be a published
    /// credential; an empty one would be no credential at all. Both are refusals.
    pub fn parse(
        token: Option<String>,
        port: Option<String>,
        bind: Option<String>,
    ) -> Result<Self, String> {
        let token = token.ok_or_else(|| {
            "KOD_BRIDGE_TOKEN is not set. The bridge will not start without one: there is \
             deliberately no default, because a default token is a published token."
                .to_string()
        })?;
        if token.trim().is_empty() {
            return Err("KOD_BRIDGE_TOKEN is empty, which is the same as no authentication at \
                        all. Set a real secret."
                .to_string());
        }
        let port = match port {
            None => DEFAULT_PORT,
            Some(s) => s
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("KOD_BRIDGE_PORT is not a port number: {s:?}"))?,
        };
        if port == 0 {
            return Err("KOD_BRIDGE_PORT must not be 0".to_string());
        }
        let bind = parse_bind(bind)?;
        Ok(Self { token, port, bind })
    }

    /// The same rules for a caller that already holds the values AS VALUES — a
    /// Settings window — rather than as environment strings.
    ///
    /// It exists for the WORDS, not for the rules. `parse`'s errors name
    /// `KOD_BRIDGE_*` variables: exactly right on a terminal, and wrong in a
    /// window, where a user who has never set an environment variable in their
    /// life reads "KOD_BRIDGE_PORT must not be 0" as a bug in the app rather
    /// than as a note about the field they just typed in.
    ///
    /// The bind rule itself is NOT restated — [`parse_bind`] is called, so there
    /// remains exactly one definition of which addresses this bridge will
    /// listen on and this path cannot drift away from it.
    pub fn from_parts(token: String, port: u16, bind: &str) -> Result<Self, String> {
        // Not trimmed, only tested: a token is a credential compared byte for
        // byte against what the phone sends, and silently editing one because it
        // was pasted with a trailing space would be a login that fails with no
        // visible reason.
        if token.trim().is_empty() {
            return Err("Enter an access token. It is the only thing between your sessions and \
                        anyone who can reach this Mac, so the bridge will not start without one \
                        — and there is deliberately no default, because a default token is a \
                        published token."
                .to_string());
        }
        if port == 0 {
            return Err("0 is not a port. Use 8787 unless you have a reason not to, and set the \
                        same number in the phone app."
                .to_string());
        }
        // The reason is discarded on purpose: "not an IPv4 address" and "not a
        // Tailscale address" have the SAME fix, and one sentence that says what
        // to type beats two that say what was wrong.
        let bind = parse_bind(Some(bind.to_string())).map_err(|_| {
            format!(
                "“{bind}” is not an address this bridge will listen on. Leave it empty for \
                 loopback — an SSH tunnel or the simulator — or paste this Mac's Tailscale \
                 address (`tailscale ip -4`, something like 100.x.y.z). Nothing else is \
                 accepted: there is no TLS yet, so a LAN or wildcard address would put your \
                 token in the clear on whatever network you happen to be on."
            )
        })?;
        Ok(Self { token, port, bind })
    }
}

// ----------------------------------------------------------------------- auth

/// Compare a presented token against the real one WITHOUT leaking where they
/// first differ.
///
/// The obvious `a == b` returns on the first differing byte, which turns a
/// remote guess into a byte-at-a-time oracle: an attacker keeps whichever guess
/// was measurably slower and walks the secret out one character at a time. So:
/// no early return, the loop length depends only on the LENGTHS (which the frame
/// size already leaks anyway), and the length check is folded in with `&` rather
/// than `&&` so it cannot short-circuit either.
///
/// `eq_scan` returns the number of bytes it compared purely so
/// `token_comparison_scans_every_byte_wherever_they_differ` can PROVE the absence
/// of an early exit, rather than asserting it in a comment.
fn eq_scan(a: &[u8], b: &[u8]) -> (bool, usize) {
    let n = a.len().max(b.len());
    let mut diff: u8 = 0;
    for i in 0..n {
        let x = if i < a.len() { a[i] } else { 0 };
        let y = if i < b.len() { b[i] } else { 0 };
        diff |= x ^ y;
    }
    ((diff == 0) & (a.len() == b.len()), n)
}

/// Constant-time (in the CONTENT of the inputs) token equality.
pub fn token_eq(presented: &str, real: &str) -> bool {
    eq_scan(presented.as_bytes(), real.as_bytes()).0
}

// ------------------------------------------------------------------- protocol

/// What the connection loop should do with one inbound frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing to say; keep listening.
    Idle,
    /// Send these, then keep the connection. Every recoverable error is one of
    /// these — an unknown message type is not a reason to hang up on a phone.
    Send(Vec<BridgeMsg>),
    /// Send these (always exactly one `hello_err`), then close.
    Fail(Vec<BridgeMsg>),
    /// The hello passed. The caller must now register with the [`Hub`] and send
    /// `hello_ok` followed by the ONE snapshot — [`Protocol`] cannot mint those
    /// itself because it has neither the clock nor the session view, and that is
    /// what keeps it pure.
    Accept,
    /// Ask the daemon, then answer the phone with what it said.
    ///
    /// [`Protocol`] stops here on purpose: it has no channel and no socket, so it
    /// decides only that this is a well-formed ask and hands over the two things
    /// the daemon needs. Whether the ask is ALLOWED is not answered anywhere in
    /// this file — the daemon resolves the session's kind from its own state and
    /// refuses shells, dead sessions and unknown ids.
    Ask { rid: u64, sid: u64, ask: PhoneAsk },
}

/// The two things a phone may ask for, after the wire layer has vetted the shape.
///
/// Separate from [`Command`] so [`Step`] can stay comparable in tests (`Command`
/// is a serde wire type and derives no `PartialEq`), and so the sid → `SessionId`
/// step happens in exactly one place: [`command_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhoneAsk {
    Input(String),
    Key(PhoneKey),
}

/// The daemon command one ask becomes.
///
/// A `sid` and a verb, and nothing describing the session. That is the shape the
/// whole security story rests on: the phone cannot claim a shell is a claude
/// session, because it is never asked what the session is.
pub fn command_for(sid: u64, ask: PhoneAsk) -> Command {
    let id = SessionId(sid);
    match ask {
        PhoneAsk::Input(text) => Command::PhoneInput { id, text },
        PhoneAsk::Key(key) => Command::PhoneKey { id, key },
    }
}

/// The per-connection state machine.
pub struct Protocol {
    token: String,
    /// Until this is true the ONLY thing that may leave the connection is a
    /// `hello_err`. The contract's "the bridge sends nothing before a successful
    /// hello" is this one flag, checked in one place.
    established: bool,
}

impl Protocol {
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into(), established: false }
    }

    pub fn established(&self) -> bool {
        self.established
    }

    /// Feed one inbound TEXT frame.
    pub fn on_text(&mut self, bytes: &[u8]) -> Step {
        let msg = match decode_frame(bytes) {
            Ok(m) => m,
            Err(e) => return self.reject(e.code(), e.message()),
        };
        match (self.established, msg) {
            // ---- handshake ----
            (false, PhoneMsg::Hello { proto, token }) => {
                if proto != PROTO {
                    return self.reject(
                        "bad_proto",
                        format!("this bridge speaks proto {PROTO}, not {proto}"),
                    );
                }
                if !token_eq(&token, &self.token) {
                    // No detail. "wrong length" or "wrong prefix" would be a
                    // free hint, and the phone cannot act on it anyway.
                    return self.reject("unauthorized", "bad token");
                }
                self.established = true;
                Step::Accept
            }
            (false, _) => self.reject("expected_hello", "the first message must be hello"),

            // ---- established ----
            (true, PhoneMsg::Ping) => Step::Send(vec![BridgeMsg::Pong]),
            (true, PhoneMsg::Hello { .. }) => Step::Send(vec![BridgeMsg::err(
                "already_hello",
                "this connection already completed its handshake",
            )]),
            (true, PhoneMsg::Input { sid, text, rid }) => {
                Step::Ask { rid, sid, ask: PhoneAsk::Input(text) }
            }
            (true, PhoneMsg::Key { sid, key, rid }) => match key.to_daemon() {
                Some(key) => Step::Ask { rid, sid, ask: PhoneAsk::Key(key) },
                // Answered HERE rather than forwarded: the daemon has no word for
                // a key this build does not know, and mapping it to the nearest
                // one it does would press something the user never pressed. It is
                // an `input_result`, not an `err`, because the phone is waiting
                // for the fate of a specific sid.
                None => Step::Send(vec![BridgeMsg::input_refused(
                    rid,
                    sid,
                    "this bridge does not know that key",
                )]),
            },
            // The no-lockstep rule: answer, do NOT hang up. A phone one version
            // ahead sending a message this build has never heard of is a normal
            // event, not a protocol violation.
            (true, PhoneMsg::Unknown) => {
                Step::Send(vec![BridgeMsg::err("unknown_type", "unrecognized message type")])
            }
        }
    }

    /// Feed one inbound BINARY frame. v0 is JSON text only.
    pub fn on_binary(&mut self) -> Step {
        self.reject("binary_unsupported", "v0 speaks json text frames only")
    }

    /// Turn an error into the right message for the current phase: before the
    /// handshake it is a fatal `hello_err`, after it a recoverable `err`.
    fn reject(&self, code: &str, message: impl Into<String>) -> Step {
        if self.established {
            Step::Send(vec![BridgeMsg::err(code, message)])
        } else {
            Step::Fail(vec![BridgeMsg::hello_err(code, message)])
        }
    }
}

// ------------------------------------------------------------------------ hub

/// One subscribed phone, from the hub's side.
struct Sub {
    id: u64,
    tx: Sender<BridgeMsg>,
    /// How many messages are queued and unread. See [`MAX_BACKLOG`].
    backlog: Arc<AtomicUsize>,
}

struct HubState {
    sessions: BTreeMap<u64, WireSession>,
    /// Per-sid rev. Entries are NEVER removed, not even on `gone`: rev is
    /// contracted to be monotonic per `(epoch, sid)`, and a counter that
    /// restarted when a session disappeared would let a stale upsert the phone
    /// still holds beat the new one.
    revs: HashMap<u64, u64>,
    subs: Vec<Sub>,
    next_sub: u64,
}

/// The shared session view and the fan-out to connected phones.
pub struct Hub {
    /// Minted ONCE per bridge attach and immutable thereafter — hence outside
    /// the mutex. The phone keys its cache on `(epoch, sid)` and flushes the lot
    /// when it changes, which is what makes a bridge restart safe: rev counters
    /// start over, but they start over under a new epoch, so nothing the phone
    /// still holds can be confused for the new state.
    epoch: String,
    /// POISONING IS IGNORED at every lock site below
    /// (`unwrap_or_else(|e| e.into_inner())`). Every mutation here is a single
    /// map insert/remove or one `Vec` retain, so a panic cannot leave a
    /// half-updated invariant for the next caller to trip over — there is
    /// nothing for the poison to protect. What propagating it DOES buy is a
    /// bridge where one panicking connection thread silently kills the daemon
    /// pump and every other phone, while the process stays up and reports
    /// itself healthy.
    state: Mutex<HubState>,
}

/// A registered phone's end of the fan-out.
pub struct ClientHandle {
    pub id: u64,
    /// The snapshot to send immediately after `hello_ok`, captured under the
    /// same lock that registered this client — so no delta can slip into the gap
    /// between "what I was told" and "what I am now subscribed to".
    pub snapshot: Vec<WireSession>,
    rx: Receiver<BridgeMsg>,
    backlog: Arc<AtomicUsize>,
}

impl ClientHandle {
    /// Take everything queued. `None` means the hub dropped us (backlog blown)
    /// and the connection should close.
    pub fn drain(&self) -> Option<Vec<BridgeMsg>> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(m) => {
                    self.backlog.fetch_sub(1, Ordering::Relaxed);
                    out.push(m);
                }
                Err(TryRecvError::Empty) => return Some(out),
                Err(TryRecvError::Disconnected) => return None,
            }
        }
    }
}

impl Hub {
    pub fn new(epoch: impl Into<String>) -> Self {
        Self {
            epoch: epoch.into(),
            state: Mutex::new(HubState {
                sessions: BTreeMap::new(),
                revs: HashMap::new(),
                subs: Vec::new(),
                next_sub: 1,
            }),
        }
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// Register a phone and hand it the snapshot atomically.
    pub fn attach_client(&self) -> ClientHandle {
        let (tx, rx) = mpsc::channel();
        let backlog = Arc::new(AtomicUsize::new(0));
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let id = st.next_sub;
        st.next_sub += 1;
        let snapshot = st.sessions.values().cloned().collect();
        st.subs.push(Sub { id, tx, backlog: Arc::clone(&backlog) });
        ClientHandle { id, snapshot, rx, backlog }
    }

    pub fn detach_client(&self, id: u64) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.subs.retain(|s| s.id != id);
    }

    /// The `sessions` snapshot frame for a freshly-registered client.
    pub fn snapshot_msg(&self, sessions: Vec<WireSession>) -> BridgeMsg {
        BridgeMsg::Sessions { epoch: self.epoch.clone(), sessions }
    }

    /// The `hello_ok` frame.
    pub fn hello_ok(&self) -> BridgeMsg {
        BridgeMsg::HelloOk {
            proto: PROTO,
            epoch: self.epoch.clone(),
            server_time: now_ms(),
            caps: Caps::v0(),
        }
    }

    /// Upsert a session and broadcast it. Returns the rev that was emitted, or
    /// `None` if nothing was emitted.
    ///
    /// AN IDENTICAL SESSION IS NOT RE-EMITTED. The daemon repaints on a
    /// `dirty` counter that ticks for reasons the phone cannot see (a grid
    /// frame, a scroll), and forwarding those would be spending a phone's radio
    /// on "nothing changed". rev therefore counts EMISSIONS, not daemon events —
    /// which is all the contract asks of it.
    pub fn upsert(&self, session: WireSession) -> Option<u64> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.sessions.get(&session.sid) == Some(&session) {
            return None;
        }
        let sid = session.sid;
        let rev = st.revs.entry(sid).or_insert(0);
        *rev += 1;
        let rev = *rev;
        st.sessions.insert(sid, session.clone());
        let msg = BridgeMsg::Session { epoch: self.epoch.clone(), rev, session };
        broadcast(&mut st, msg);
        Some(rev)
    }

    /// Drop a session and broadcast `gone`.
    pub fn gone(&self, sid: u64) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.sessions.remove(&sid).is_none() {
            return;
        }
        let msg = BridgeMsg::Gone { epoch: self.epoch.clone(), sid };
        broadcast(&mut st, msg);
    }

    /// Replace the whole session view.
    ///
    /// Emitted as a DIFF (upserts + gones) rather than a second `sessions`
    /// frame, because the contract allows exactly one snapshot per connection —
    /// re-snapshotting mid-stream would be a message the phone is not expecting
    /// and, worse, one with no rev to order it against.
    pub fn reset(&self, sessions: Vec<WireSession>) {
        let fresh: BTreeMap<u64, WireSession> = sessions.into_iter().map(|s| (s.sid, s)).collect();
        let stale: Vec<u64> = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.sessions.keys().filter(|k| !fresh.contains_key(k)).copied().collect()
        };
        for sid in stale {
            self.gone(sid);
        }
        for s in fresh.into_values() {
            self.upsert(s);
        }
    }

    /// Current sessions, for tests and for a caller that wants the view.
    pub fn sessions(&self) -> Vec<WireSession> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).sessions.values().cloned().collect()
    }

    /// The rev currently held for `sid`, if any.
    pub fn rev_of(&self, sid: u64) -> Option<u64> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).revs.get(&sid).copied()
    }

    /// Cut every subscribed phone loose.
    ///
    /// No second channel and no new teardown path: dropping the hub's end of a
    /// queue is precisely what makes that connection's next `drain` return
    /// `None`, which is the "backlog blown" case the loop already hangs up on.
    /// Each phone reconnects and resyncs from a fresh snapshot, which is what
    /// the epoch/rev design is for.
    pub fn disconnect_all(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).subs.clear();
    }

    /// How many phones are subscribed RIGHT NOW.
    ///
    /// A live count, not a total: a subscription lasts until its connection
    /// thread detaches. That is only a number worth showing a user because
    /// [`IDLE_TIMEOUT`] reaps the connections that stopped existing without
    /// saying so — before it, this counted corpses.
    pub fn sub_count(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).subs.len()
    }
}

/// Fan one message out, dropping any subscriber that has stopped reading.
fn broadcast(st: &mut HubState, msg: BridgeMsg) {
    st.subs.retain(|sub| {
        if sub.backlog.load(Ordering::Relaxed) >= MAX_BACKLOG {
            return false;
        }
        if sub.tx.send(msg.clone()).is_err() {
            return false;
        }
        sub.backlog.fetch_add(1, Ordering::Relaxed);
        true
    });
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A fresh epoch: 128 random bits, hex. Prefers `/dev/urandom` and falls back to
/// clock+pid — uniqueness is what the epoch needs, not unpredictability (it is
/// never a credential; the token is).
pub fn mint_epoch() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        let seed = now_ms() ^ ((std::process::id() as u64) << 32);
        let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        for chunk in b.chunks_mut(8) {
            x ^= x >> 30;
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x ^= x >> 27;
            x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^= x >> 31;
            for (i, slot) in chunk.iter_mut().enumerate() {
                *slot = (x >> (i * 8)) as u8;
            }
        }
    }
    b.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ----------------------------------------------------------- the daemon link

/// How long a connection thread waits for the daemon's answer to one ask.
///
/// The daemon answers `PhoneInput` in microseconds — a unix socket, one scan of
/// `infos()`, one write to a PTY — so this is a HANG DETECTOR, not a budget. It
/// is bounded at all because the alternative is a thread parked forever on a
/// daemon that stopped answering, holding one of [`MAX_CONNS`] slots and a phone
/// that is being told nothing.
const DAEMON_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// One phone's ask, on its way to the daemon, carrying the channel it will be
/// answered on.
pub struct PhoneRequest {
    pub command: Command,
    /// The ONE answer. A `sync_channel(1)` rather than a `channel()` so that
    /// delivering it can never block the sender thread: the buffer always has
    /// room for the single message, and a phone that has already given up shows
    /// up as a send error rather than as a wait.
    pub reply: SyncSender<PhoneOutcome>,
}

/// What came back — the daemon's verdict, or the reason there isn't one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhoneOutcome {
    Ok,
    /// Refused, or never answered. The string is shown to a user, and where the
    /// daemon supplied one it is the daemon's OWN sentence: this process did not
    /// make the decision and must not paraphrase it.
    Refused(String),
}

impl PhoneOutcome {
    fn from_reply(reply: CommandReply) -> Self {
        match reply {
            CommandReply::Ok => Self::Ok,
            CommandReply::Error(e) => Self::Refused(e),
            // `PhoneInput`/`PhoneKey` answer only `Ok` or `Error`, so anything
            // else means this bridge and the daemon disagree about what was
            // asked. Reporting that beats reading an unknown shape as success.
            _ => Self::Refused("Kod answered something this bridge does not understand".into()),
        }
    }
}

/// The asks that have been written to the daemon and not yet answered, keyed by
/// the `request_id` they went out under.
///
/// Small in every case that matters. The daemon answers every request it reads
/// (`dispatch_checked` always produces a `CommandReply`, a panicking command
/// included), and a connection thread waits for its own answer before reading
/// another frame — so this normally holds at most one entry per connected phone,
/// [`MAX_CONNS`] of them. It can grow past that only against a daemon that
/// accepts requests and answers none, at one abandoned entry per phone per
/// [`DAEMON_REPLY_TIMEOUT`], and `fail_all` empties it the moment that link
/// actually breaks.
#[derive(Default)]
pub struct Pending {
    /// Poisoning is ignored here for the same reason it is on [`Hub`]: every
    /// mutation is one map insert or remove, so there is no half-updated
    /// invariant for the poison to protect, and propagating it would let one
    /// panicking connection thread silently stop every phone's input from ever
    /// being answered again.
    waiting: Mutex<HashMap<u64, SyncSender<PhoneOutcome>>>,
}

impl Pending {
    fn register(&self, request_id: u64, reply: SyncSender<PhoneOutcome>) {
        self.waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(request_id, reply);
    }

    /// Hand one answer to whoever is waiting on it. A send error means that phone
    /// already gave up (or hung up), which is not an error here — the entry is
    /// gone either way, which is what keeps the map from growing.
    fn answer(&self, request_id: u64, outcome: PhoneOutcome) {
        let waiter = self
            .waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_id);
        if let Some(tx) = waiter {
            let _ = tx.send(outcome);
        }
    }

    /// Tell everyone still waiting at once, when the link they were waiting on
    /// dies. Without this each of them sits out the whole [`DAEMON_REPLY_TIMEOUT`]
    /// to learn something already known.
    fn fail_all(&self, why: &str) {
        let waiters: Vec<_> = self
            .waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, tx)| tx)
            .collect();
        for tx in waiters {
            let _ = tx.send(PhoneOutcome::Refused(why.to_string()));
        }
    }
}

/// Post one ask and wait for its answer. Called on a CONNECTION thread, which is
/// the only thread allowed to block on a phone's behalf.
fn ask_daemon(input: &Sender<PhoneRequest>, command: Command) -> PhoneOutcome {
    let (tx, rx) = mpsc::sync_channel(1);
    if input.send(PhoneRequest { command, reply: tx }).is_err() {
        return PhoneOutcome::Refused("the bridge is not connected to Kod".into());
    }
    match rx.recv_timeout(DAEMON_REPLY_TIMEOUT) {
        Ok(outcome) => outcome,
        Err(RecvTimeoutError::Timeout) => PhoneOutcome::Refused("Kod did not answer".into()),
        Err(RecvTimeoutError::Disconnected) => {
            PhoneOutcome::Refused("the bridge is not connected to Kod".into())
        }
    }
}

/// The ONLY thread that ever writes to the daemon socket.
///
/// It blocks on the phone channel and never on the daemon stream, which is what
/// makes an ask leave immediately even when the daemon has been silent for an
/// hour. Its answers come back on the read half, to [`pump`].
fn send_loop(mut writer: UnixStream, requests: Receiver<PhoneRequest>, pending: &Pending) {
    let mut next_request_id: u64 = 1;
    for req in requests {
        let request_id = next_request_id;
        next_request_id += 1;
        // Registered BEFORE the write. The daemon is a local socket and the pump
        // is a separate thread, so the reply can be read back before `write_frame`
        // has even returned; registering afterwards would drop that reply on the
        // floor and leave the phone waiting out its whole timeout.
        pending.register(request_id, req.reply);
        if write_frame(
            &mut writer,
            &ClientMsg::Request { request_id, command: req.command },
        )
        .is_err()
        {
            break;
        }
    }
    // Either the link broke or the last phone went away. Anyone still waiting is
    // told now rather than one timeout at a time.
    pending.fail_all("the bridge lost its connection to Kod");
}

/// Attach to the daemon and split the connection into a read half and a write
/// half.
///
/// WHY THIS IS NOT `Client::attach`. [`crate::client::Client`] owns its
/// `UnixStream` privately, and its only blocking primitive is `next()`, which has
/// no timeout — so a thread holding a `Client` can block on the daemon stream or
/// watch the phone input channel, never both, and there is no way from here to
/// obtain a second handle on the same connection. Everything that made this file
/// safe is unchanged: the same `WIRE_VERSION` from the same linked crate, the
/// same `ClientRole::Phone`, the same [`AttachError`] vocabulary and its wording,
/// and `main`'s retire pre-flight still runs before `serve` is ever called. The
/// one duplicated thing is the handshake, and it is duplicated where the compiler
/// notices: `ClientMsg::Hello` is a struct variant, so a field added to it fails
/// to build here as well as there.
fn attach_split(socket: &Path) -> Result<(UnixStream, UnixStream, ServerMsg), AttachError> {
    let mut stream = UnixStream::connect(socket)?;
    write_frame(
        &mut stream,
        &ClientMsg::Hello {
            wire_version: WIRE_VERSION,
            // PHONE, not Full. This process is the one a network peer talks to, so
            // it holds the smallest capability that still does the job: the daemon
            // refuses it anything but typing into an agent session, whatever this
            // process is later persuaded to ask for.
            role: ClientRole::Phone,
        },
    )?;
    let first: ServerMsg = read_frame(&mut stream)?;
    match first {
        ServerMsg::Welcome { .. } => {
            let writer = stream.try_clone()?;
            Ok((stream, writer, first))
        }
        ServerMsg::VersionMismatch { daemon_version } => Err(AttachError::VersionMismatch {
            ours: WIRE_VERSION,
            daemon: daemon_version,
        }),
        other => Err(AttachError::Unexpected(format!("{other:?}"))),
    }
}

// ---------------------------------------------------------------- the sockets

/// A running listener set: what it is reachable on, and the threads that must be
/// joined to stop it.
pub struct Started {
    /// The addresses actually bound, loopback first. Read back from the
    /// listeners rather than re-derived from the [`Config`], so what a caller
    /// displays is what the kernel handed out.
    pub endpoints: Vec<String>,
    /// The accept threads. Each OWNS its `TcpListener`, so the port is not free
    /// until they have exited — setting the stop flag is half of a shutdown and
    /// [`Started::join`] is the other half. Skip it and the next bind on the
    /// same port races the old listener and loses with `EADDRINUSE`.
    pub accept_threads: Vec<JoinHandle<()>>,
}

impl Started {
    /// Wait for the accept threads to notice the stop flag and drop their
    /// listeners. A panicked accept thread is not worth propagating into a
    /// caller that is already shutting down: the listener is dropped either way,
    /// which is the thing being waited for.
    pub fn join(self) {
        for t in self.accept_threads {
            let _ = t.join();
        }
    }
}

/// Bind and start accepting WITHOUT blocking; the caller owns the shutdown.
///
/// This is the half of [`serve`] a GUI can drive. It deliberately knows nothing
/// about the daemon — it takes a [`Hub`] that is already populated, or not,
/// rather than attaching to anything — so starting and stopping the phone
/// listener from a settings toggle can never retire a daemon (`client.rs`).
///
/// `input` is how a connection thread reaches the daemon: it is CLONED per
/// connection, never shared, because a `Sender` clone is the only handle in this
/// design that two threads may hold at once. The daemon socket itself is on the
/// far end, touched by exactly one thread (see the module note).
///
/// Stopping: set `stop`, then [`Started::join`].
pub fn serve_with(
    cfg: &Config,
    hub: Arc<Hub>,
    stop: Arc<AtomicBool>,
    input: Sender<PhoneRequest>,
) -> Result<Started, String> {
    // Loopback is always bound, even when a tailnet address is configured: the
    // simulator and an SSH tunnel both arrive on 127.0.0.1, and losing them the
    // moment the phone is set up would be a bad trade.
    let mut listeners = vec![TcpListener::bind((Ipv4Addr::LOCALHOST, cfg.port))
        .map_err(|e| format!("cannot bind 127.0.0.1:{}: {e}", cfg.port))?];
    if let Bind::Tailnet(ip) = cfg.bind {
        listeners.push(TcpListener::bind((ip, cfg.port)).map_err(|e| {
            format!(
                "cannot bind {ip}:{}: {e}. Is that still this machine's Tailscale \
                 address? Check `tailscale ip -4`.",
                cfg.port
            )
        })?);
    }

    let token = Arc::new(cfg.token.clone());
    let bind = cfg.bind;
    // Shared by BOTH accept threads when a tailnet address is bound, which is
    // why the gate below is a read-modify-write and not a load then an add.
    let conns = Arc::new(AtomicUsize::new(0));
    let mut endpoints = Vec::with_capacity(listeners.len());
    let mut accept_threads = Vec::with_capacity(listeners.len());
    for listener in listeners {
        let addr = listener
            .local_addr()
            .map_err(|e| format!("bound a listener with no address: {e}"))?
            .to_string();
        // Without this the accept below blocks forever and the stop flag is
        // never read again — there is no other way to interrupt a blocking
        // accept from inside the process.
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("cannot poll the listener on {addr}: {e}"))?;
        endpoints.push(addr);
        let hub = Arc::clone(&hub);
        let token = Arc::clone(&token);
        let stop = Arc::clone(&stop);
        let conns = Arc::clone(&conns);
        let input = input.clone();
        accept_threads.push(std::thread::spawn(move || {
            // Leaving this loop DROPS the listener, and that — not the flag — is
            // what frees the port.
            while !stop.load(Ordering::Relaxed) {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    // Sleeping on the hard errors too, not just WouldBlock:
                    // EMFILE returns instantly, so a bare `continue` would spin
                    // a core for as long as the process is out of descriptors.
                    Err(_) => {
                        std::thread::sleep(ACCEPT_POLL);
                        continue;
                    }
                };
                // MEASURED ON THIS MAC, IN C — NOT ASSUMED: on Darwin the
                // socket returned by accept() INHERITS the listener's
                // O_NONBLOCK, and the listener just above is non-blocking. So
                // every accepted socket arrives non-blocking, and on one
                // `set_read_timeout` silently degrades to a no-op: the
                // measurement was a read that should have waited 1000 ms
                // returning EAGAIN in 0.0 ms, then taking 1000.7 ms once the
                // flag was cleared. `conn` would get WouldBlock on its first
                // read and treat it as an expired hello deadline — so without
                // this line EVERY phone is dropped the instant it connects, and
                // silently, by a bridge that still reports itself listening.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                // Belt and braces over the bind. The kernel already limits who
                // can reach these sockets, but if a bind ever widens by
                // accident this refuses the peer BEFORE it can present — or
                // guess at — the bearer token.
                if !stream.peer_addr().map(|a| peer_allowed(a.ip(), bind)).unwrap_or(false) {
                    continue;
                }
                // Over the cap the socket is DROPPED here, which closes it: a
                // phone sees a failed connect and retries on its backoff, rather
                // than a hung dial with no answer.
                if conns
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                        (n < MAX_CONNS).then_some(n + 1)
                    })
                    .is_err()
                {
                    continue;
                }
                let hub = Arc::clone(&hub);
                let token = Arc::clone(&token);
                let stop = Arc::clone(&stop);
                let conns = Arc::clone(&conns);
                let input = input.clone();
                std::thread::spawn(move || {
                    // The slot must come back even if `conn` panics, or every
                    // panic would ratchet MAX_CONNS down by one until the bridge
                    // refuses everything. `conn` owns its stream and its handle
                    // outright, so there is nothing an unwind can leave visibly
                    // half-built — hence AssertUnwindSafe rather than a redesign.
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        conn(stream, &hub, &token, &stop, &input)
                    }));
                    conns.fetch_sub(1, Ordering::SeqCst);
                });
            }
        }));
    }
    Ok(Started { endpoints, accept_threads })
}

/// Run the bridge: bind, attach, serve until the daemon goes away.
///
/// ORDER MATTERS. Config is validated and the listener is bound BEFORE
/// `Client::attach` — that is `serve_with`, which binds before it returns — so a
/// missing token or a busy port fails without ever having touched the daemon.
/// Attaching is the one irreversible act in this process (see `client.rs` on
/// retire), so it goes last.
pub fn serve(socket: &Path) -> Result<(), String> {
    let cfg = Config::from_env()?;
    let hub = Arc::new(Hub::new(mint_epoch()));
    // The CLI has no stop path — the process IS the lifetime — so this is only
    // ever set on the way out, to retire the listeners with the daemon they
    // were serving.
    let stop = Arc::new(AtomicBool::new(false));
    // Unbounded, and it does not need a bound: a connection thread waits for its
    // own answer before reading another frame, so at most MAX_CONNS asks can be
    // in flight no matter how hard the phones type.
    let (input_tx, input_rx) = mpsc::channel::<PhoneRequest>();
    let started = serve_with(&cfg, Arc::clone(&hub), Arc::clone(&stop), input_tx)?;

    // `?` here would return with the accept threads still looping and still
    // holding the port — and, worse, still ACCEPTING phones and serving them an
    // empty Hub for a daemon we never reached. Every exit from this function past
    // the bind has to go through the same teardown, so the fallible part is
    // wrapped and the teardown runs once at the end.
    let outcome = serve_attached(socket, &cfg, &hub, input_rx);
    stop.store(true, Ordering::Relaxed);
    started.join();
    outcome
}

/// The part of `serve` that can fail after the listeners are up. Split out so its
/// error path cannot skip the caller's stop-and-join.
fn serve_attached(
    socket: &Path,
    cfg: &Config,
    hub: &Arc<Hub>,
    requests: Receiver<PhoneRequest>,
) -> Result<(), String> {
    let (mut reader, writer, welcome) = attach_split(socket).map_err(|e| e.to_string())?;
    let mut mirror = Mirror::default();
    mirror.apply(&welcome);

    // A phone that connects in the window between the bind and this line gets an
    // empty snapshot and then the whole list as upserts — which is a state the
    // epoch/rev contract already covers, and the price of never attaching to a
    // daemon before knowing the port is ours.
    hub.reset(wire_sessions(&mirror));

    if let ServerMsg::Welcome { wire_version, .. } = &welcome {
        let where_ = match cfg.bind {
            Bind::Loopback => format!("127.0.0.1:{}", cfg.port),
            Bind::Tailnet(ip) => format!("127.0.0.1:{p} and {ip}:{p} (tailnet)", p = cfg.port),
        };
        println!(
            "bridge · wire {wire_version} · epoch {} · listening on {where_} · {} sessions",
            hub.epoch(),
            mirror.sessions.len()
        );
    }

    // The write half goes to its own thread and never comes back here. It ends
    // when the link breaks or when the last `Sender` clone is dropped — which
    // `Started::join` guarantees, since every clone lives on an accept or
    // connection thread.
    let pending = Arc::new(Pending::default());
    {
        let pending = Arc::clone(&pending);
        std::thread::spawn(move || send_loop(writer, requests, &pending));
    }

    pump(&mut reader, &mut mirror, hub, &pending)
}

/// Fold daemon messages into the mirror, publish what moved, and hand each reply
/// to the phone waiting on it.
///
/// This thread blocks on the daemon stream and on NOTHING else — no channel, no
/// phone, no lock held across a read. That is what makes "a phone that spams
/// input cannot stall session updates" true rather than hoped for: input never
/// reaches this loop at all, it goes out on [`send_loop`], and all that arrives
/// here is a reply frame to be routed.
///
/// Grid frames and timeline events are deliberately dropped: the phone shows no
/// terminal, and forwarding a viewport per tick would be the entire bandwidth
/// budget spent on something it cannot render.
pub fn pump(
    reader: &mut UnixStream,
    mirror: &mut Mirror,
    hub: &Hub,
    pending: &Pending,
) -> Result<(), String> {
    loop {
        let msg: ServerMsg = read_frame(reader).map_err(|e| e.to_string())?;
        if let ServerMsg::Reply { request_id, reply } = msg {
            pending.answer(request_id, PhoneOutcome::from_reply(reply));
            continue;
        }
        match mirror.apply(&msg) {
            Some(Change::Reset) => hub.reset(wire_sessions(mirror)),
            Some(Change::Info(id)) => {
                if let Some(info) = mirror.sessions.get(&id) {
                    hub.upsert(WireSession::from(info));
                }
            }
            Some(Change::Closed(id)) => hub.gone(id.0),
            _ => {}
        }
    }
}

fn wire_sessions(mirror: &Mirror) -> Vec<WireSession> {
    mirror.sessions.values().map(WireSession::from).collect()
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(HARD_FRAME_CEILING))
        .max_frame_size(Some(HARD_FRAME_CEILING))
}

/// One connection, start to finish.
fn conn(
    stream: TcpStream,
    hub: &Hub,
    token: &str,
    stop: &AtomicBool,
    input: &Sender<PhoneRequest>,
) {
    let _ = stream.set_nodelay(true);
    // The HTTP upgrade gets a bounded blocking read: a socket that connects and
    // then says nothing must not pin a thread forever. A real client sends the
    // whole request immediately, so an interrupted handshake is treated as a
    // failed one rather than resumed.
    if stream.set_read_timeout(Some(HELLO_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
    {
        return;
    }
    let Ok(mut ws) = tungstenite::accept_with_config(stream, Some(ws_config())) else {
        return;
    };

    let mut proto = Protocol::new(token);
    let mut handle: Option<ClientHandle> = None;

    // Phase 1: the handshake, on a TOTAL deadline (see HELLO_TIMEOUT).
    let deadline = Instant::now() + HELLO_TIMEOUT;
    loop {
        if handle.is_some() {
            break;
        }
        // Checked between reads, so a peer that says nothing at all still holds
        // this thread for up to HELLO_TIMEOUT after a stop. It holds no
        // listener, so it delays nothing that matters — and it is not owed a
        // close frame either, having never established anything.
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return;
        }
        match ws.read() {
            Ok(Message::Text(s)) => match proto.on_text(s.as_bytes()) {
                Step::Accept => {
                    // Register FIRST, then send: the snapshot and the
                    // subscription are taken under one lock, so a delta that
                    // lands mid-handshake is queued rather than lost.
                    let h = hub.attach_client();
                    // Tell the daemon the count changed. It lost its direct view
                    // of this table when the bridge became a child process, and a
                    // settings line that reads "no phone connected" while one is
                    // connected is the kind of quiet lie this whole feature is
                    // built to avoid. Fire-and-forget: a failure here must never
                    // cost the phone its connection.
                    let _ = ask_daemon(
                        input,
                        Command::PhoneClients { n: hub.sub_count() as u32 },
                    );
                    let mut frames = vec![hub.hello_ok()];
                    frames.push(hub.snapshot_msg(h.snapshot.clone()));
                    if !send_all(&mut ws, &frames) {
                        hub.detach_client(h.id);
                        return;
                    }
                    handle = Some(h);
                }
                Step::Fail(frames) => {
                    let _ = send_all(&mut ws, &frames);
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    return;
                }
                // Unreachable while `Protocol` answers everything pre-hello with
                // `Fail`, and it stays a hang-up rather than a fallthrough on
                // purpose: this is the arm a future `Step::Ask` would land in if
                // the handshake gate were ever loosened, and the safe reading of
                // "a command from a connection that never authenticated" is to
                // drop the socket, not to carry the command to the daemon.
                _ => return,
            },
            Ok(Message::Binary(_)) => {
                if let Step::Fail(frames) = proto.on_binary() {
                    let _ = send_all(&mut ws, &frames);
                }
                let _ = ws.close(None);
                let _ = ws.flush();
                return;
            }
            Ok(Message::Close(_)) => return,
            // Ping/Pong/Frame before hello: tungstenite answers pings itself on
            // flush, and neither is a protocol message, so neither counts as
            // "the bridge sent something before hello".
            Ok(_) => {}
            Err(e) if would_block(&e) => return, // the hello deadline expired
            Err(_) => return,
        }
    }
    // Unreachable: phase 1 only leaves the loop by returning or by setting this.
    let Some(handle) = handle else { return };

    // Phase 2: read pings, write deltas, until either end stops.
    let _ = ws.get_ref().set_read_timeout(Some(POLL));
    // Proof of life, for IDLE_TIMEOUT. It starts HERE rather than at accept so
    // that a slow handshake is not charged against the phone's first minute.
    let mut last_rx = Instant::now();
    loop {
        // A stopped server hangs up POLITELY, within one POLL: the teardown
        // below sends a close frame, and a phone that saw only a bare TCP FIN
        // would sit out its reconnect backoff before finding out.
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let read = ws.read();
        // ANY frame is proof of life, including the websocket Ping/Pong that
        // tungstenite answers by itself — a phone in the background may send
        // nothing else for minutes, and reaping it would be reaping a live link.
        if read.is_ok() {
            last_rx = Instant::now();
        }
        match read {
            Ok(Message::Text(s)) => match proto.on_text(s.as_bytes()) {
                Step::Send(frames) => {
                    if !send_all(&mut ws, &frames) {
                        break;
                    }
                }
                Step::Fail(frames) => {
                    let _ = send_all(&mut ws, &frames);
                    break;
                }
                // The round trip to the daemon happens on THIS thread, so this
                // phone's own deltas queue in the hub (bounded by MAX_BACKLOG)
                // for as long as it takes. Nothing else waits: other phones have
                // their own threads, and the daemon stream has the pump's. The
                // alternative — carrying on reading while an answer is
                // outstanding — would let a phone stack up asks whose replies it
                // then has to match by hand, for a wait measured in microseconds.
                Step::Ask { rid, sid, ask } => {
                    let frame = match ask_daemon(input, command_for(sid, ask)) {
                        PhoneOutcome::Ok => BridgeMsg::input_ok(rid, sid),
                        PhoneOutcome::Refused(why) => BridgeMsg::input_refused(rid, sid, why),
                    };
                    if !send_all(&mut ws, &[frame]) {
                        break;
                    }
                }
                _ => {}
            },
            Ok(Message::Binary(_)) => {
                if let Step::Send(frames) = proto.on_binary() {
                    if !send_all(&mut ws, &frames) {
                        break;
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) if would_block(&e) => {}
            Err(_) => break,
        }

        // The reaper. TCP will hold a half-open socket for minutes — a phone
        // that changed networks never sends a FIN at all — so silence is the
        // only evidence there is, and without this its thread and its hub
        // subscription outlive the connection forever.
        if last_rx.elapsed() >= IDLE_TIMEOUT {
            break;
        }

        // `drain` returning None means the hub cut us loose (backlog blown).
        // `send_all` flushes even with nothing to send, which is also what
        // pushes out the pong tungstenite queued for any websocket-level ping.
        let Some(frames) = handle.drain() else { break };
        if !send_all(&mut ws, &frames) {
            break;
        }
    }

    hub.detach_client(handle.id);
    let _ = ask_daemon(input, Command::PhoneClients { n: hub.sub_count() as u32 });
    let _ = ws.close(None);
    let _ = ws.flush();
}

/// A read that returned nothing because the poll timeout expired, not because
/// the connection broke.
fn would_block(e: &tungstenite::Error) -> bool {
    matches!(
        e,
        tungstenite::Error::Io(io)
            if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted)
    )
}

fn send_all(ws: &mut WebSocket<TcpStream>, frames: &[BridgeMsg]) -> bool {
    for f in frames {
        if ws.write(Message::Text(f.to_frame().into())).is_err() {
            return false;
        }
    }
    ws.flush().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Cli, WirePhase};
    use orchestrator_host::host::SessionInfo;
    use orchestrator_host::protocol::{EventKind, ServerEvent};
    use orchestrator_host::session::{CliKind, Phase};

    fn session(sid: u64, phase: WirePhase) -> WireSession {
        WireSession {
            sid,
            cli: Cli::Claude,
            project: "kod".into(),
            title: "t".into(),
            phase,
            phase_since: 1,
            alive: true,
            can_input: true,
            last_message: String::new(),
            pending_headline: None,
            trouble: None,
            limit_hit: false,
            limit_percent: None,
            limit_reset: None,
        }
    }

    fn hello(token: &str) -> Vec<u8> {
        format!(r#"{{"t":"hello","proto":2,"token":"{token}"}}"#).into_bytes()
    }

    /// A phone-input channel with nothing on the far end — for the tests that
    /// are about the listener rather than about typing. Dropping the receiver
    /// here is deliberate: any ask would take the "not connected to Kod" path,
    /// and none of those tests type.
    fn no_daemon() -> Sender<PhoneRequest> {
        mpsc::channel().0
    }

    fn code(m: &BridgeMsg) -> &str {
        match m {
            BridgeMsg::Err { code, .. } | BridgeMsg::HelloErr { code, .. } => code,
            _ => panic!("not an error frame: {m:?}"),
        }
    }

    // ---- config ----

    #[test]
    fn a_missing_or_empty_token_refuses_to_start() {
        assert!(Config::parse(None, None, None).is_err());
        assert!(Config::parse(Some(String::new()), None, None).is_err());
        assert!(Config::parse(Some("   ".into()), None, None).is_err());
    }

    #[test]
    fn the_port_defaults_to_8787_and_must_be_a_port() {
        assert_eq!(Config::parse(Some("k".into()), None, None).unwrap().port, 8787);
        assert_eq!(Config::parse(Some("k".into()), Some("9001".into()), None).unwrap().port, 9001);
        assert!(Config::parse(Some("k".into()), Some("no".into()), None).is_err());
        assert!(Config::parse(Some("k".into()), Some("0".into()), None).is_err());
    }

    // ---- bind policy ----

    #[test]
    fn tailnet_range_is_100_64_slash_10_not_anything_starting_with_100() {
        let v4 = |s: &str| IpAddr::V4(s.parse().unwrap());
        assert!(is_tailnet(v4("100.64.0.1")));
        assert!(is_tailnet(v4("100.101.102.103")));
        assert!(is_tailnet(v4("100.127.255.254")));
        // Just outside the block on both sides — these are ordinary PUBLIC
        // addresses, and treating them as a tailnet would be the whole bug.
        assert!(!is_tailnet(v4("100.63.255.255")));
        assert!(!is_tailnet(v4("100.128.0.1")));
        assert!(!is_tailnet(v4("100.0.0.1")));
        assert!(!is_tailnet(v4("192.168.1.10")));
        assert!(!is_tailnet(v4("127.0.0.1")));
        assert!(is_tailnet("fd7a:115c:a1e0::1".parse().unwrap()));
        assert!(!is_tailnet("fd00::1".parse().unwrap()));
    }

    #[test]
    fn bind_defaults_to_loopback_and_accepts_only_a_tailnet_address() {
        assert_eq!(parse_bind(None).unwrap(), Bind::Loopback);
        assert_eq!(parse_bind(Some("".into())).unwrap(), Bind::Loopback);
        assert_eq!(parse_bind(Some("  ".into())).unwrap(), Bind::Loopback);
        assert_eq!(parse_bind(Some("loopback".into())).unwrap(), Bind::Loopback);
        assert_eq!(parse_bind(Some("127.0.0.1".into())).unwrap(), Bind::Loopback);
        assert_eq!(
            parse_bind(Some("100.101.102.103".into())).unwrap(),
            Bind::Tailnet("100.101.102.103".parse().unwrap())
        );
    }

    #[test]
    fn bind_refuses_wildcard_lan_and_junk_rather_than_downgrading_silently() {
        // Each of these would put a plaintext bearer token on a network. The
        // failure must be LOUD: silently falling back to loopback would leave a
        // user with a phone that cannot connect and no reason on screen.
        for bad in ["0.0.0.0", "192.168.1.10", "10.0.0.4", "8.8.8.8", "nonsense", "::1"] {
            let err = parse_bind(Some(bad.into())).unwrap_err();
            assert!(!err.is_empty(), "{bad} was accepted, or refused without a reason");
        }
    }

    #[test]
    fn a_tailnet_peer_is_refused_while_the_bridge_is_loopback_only() {
        let tail: IpAddr = "100.101.102.103".parse().unwrap();
        let lan: IpAddr = "192.168.1.10".parse().unwrap();
        let local: IpAddr = "127.0.0.1".parse().unwrap();

        // Loopback bind: only loopback peers, whatever they claim to be.
        assert!(peer_allowed(local, Bind::Loopback));
        assert!(!peer_allowed(tail, Bind::Loopback));
        assert!(!peer_allowed(lan, Bind::Loopback));

        // Tailnet bind: loopback still works (simulator, SSH tunnel), tailnet
        // peers are admitted, and a LAN peer is still refused even though the
        // process now holds a non-loopback listener.
        let b = Bind::Tailnet("100.101.102.103".parse().unwrap());
        assert!(peer_allowed(local, b));
        assert!(peer_allowed(tail, b));
        assert!(!peer_allowed(lan, b));
    }

    #[test]
    fn config_threads_the_bind_through_from_the_environment() {
        assert_eq!(Config::parse(Some("k".into()), None, None).unwrap().bind, Bind::Loopback);
        let cfg = Config::parse(Some("k".into()), None, Some("100.90.1.2".into())).unwrap();
        assert_eq!(cfg.bind, Bind::Tailnet("100.90.1.2".parse().unwrap()));
        // A bad bind must fail the WHOLE config, not be dropped on the floor.
        assert!(Config::parse(Some("k".into()), None, Some("0.0.0.0".into())).is_err());
    }

    // ---- auth ----

    #[test]
    fn token_comparison_is_correct() {
        assert!(token_eq("hunter2", "hunter2"));
        assert!(!token_eq("hunter2", "hunter3"));
        assert!(!token_eq("hunter", "hunter2"), "a prefix is not a match");
        assert!(!token_eq("hunter22", "hunter2"), "an extension is not a match");
        assert!(!token_eq("", "hunter2"));
        assert!(!token_eq("hunter2", ""));
        assert!(token_eq("", ""));
        // multi-byte: comparison is over bytes, so this must not panic or match.
        assert!(!token_eq("héllo", "hello"));
    }

    #[test]
    fn token_comparison_scans_every_byte_wherever_they_differ() {
        // The structural proof that there is no early return. If `eq_scan` ever
        // bailed on the first mismatch, these two counts would differ by ~4095
        // and the comparison would be a byte-at-a-time timing oracle.
        let real = vec![b'x'; 4096];
        let mut first_byte_wrong = real.clone();
        first_byte_wrong[0] = b'y';
        let mut last_byte_wrong = real.clone();
        last_byte_wrong[4095] = b'y';

        let (eq_a, scanned_a) = eq_scan(&first_byte_wrong, &real);
        let (eq_b, scanned_b) = eq_scan(&last_byte_wrong, &real);
        assert!(!eq_a && !eq_b);
        assert_eq!(scanned_a, scanned_b);
        assert_eq!(scanned_a, 4096);
        // A length mismatch scans the longer of the two, so the loop count
        // never depends on content either.
        assert_eq!(eq_scan(b"ab", &real).1, 4096);
    }

    // ---- handshake ----

    #[test]
    fn the_right_token_is_accepted() {
        let mut p = Protocol::new("s3cret");
        assert_eq!(p.on_text(&hello("s3cret")), Step::Accept);
        assert!(p.established());
    }

    #[test]
    fn a_wrong_token_is_unauthorized_and_closes() {
        let mut p = Protocol::new("s3cret");
        let step = p.on_text(&hello("guess"));
        let Step::Fail(frames) = &step else { panic!("expected a close, got {step:?}") };
        assert_eq!(frames.len(), 1);
        assert_eq!(code(&frames[0]), "unauthorized");
        assert!(matches!(frames[0], BridgeMsg::HelloErr { .. }));
        assert!(!p.established(), "a rejected hello must not establish");
    }

    #[test]
    fn a_wrong_proto_is_rejected() {
        for wrong in [0u32, 3, 9] {
            let mut p = Protocol::new("k");
            let frame = format!(r#"{{"t":"hello","proto":{wrong},"token":"k"}}"#);
            let step = p.on_text(frame.as_bytes());
            let Step::Fail(frames) = &step else { panic!("proto {wrong} was accepted") };
            assert_eq!(code(&frames[0]), "bad_proto");
        }
    }

    #[test]
    fn a_proto_1_phone_is_turned_away_rather_than_half_understood() {
        // The cost of the PROTO bump, pinned rather than left to be discovered in
        // the field. proto 1 WAS the read-only contract — `caps.input` documented
        // as permanently false, no `input_result` on the wire — so a phone built
        // against it has no code that could read the answer to an input attempt.
        // It gets `bad_proto`, i.e. one clear "update the app", instead of a
        // session in which this bridge advertises a composer whose answers that
        // phone would silently drop.
        let mut p = Protocol::new("k");
        let step = p.on_text(br#"{"t":"hello","proto":1,"token":"k"}"#);
        let Step::Fail(frames) = &step else { panic!("a proto-1 phone was admitted") };
        assert_eq!(code(&frames[0]), "bad_proto");
        assert!(!p.established());
    }

    #[test]
    fn nothing_but_hello_err_is_ever_sent_before_a_successful_hello() {
        // The contract's "the bridge sends NOTHING before a successful hello",
        // swept over every junk a socket can produce.
        let junk: Vec<Vec<u8>> = vec![
            br#"{"t":"ping"}"#.to_vec(),
            br#"{"t":"whatever"}"#.to_vec(),
            b"{not json".to_vec(),
            br#"{"t":"hello","proto":9,"token":"k"}"#.to_vec(),
            hello("wrong"),
            vec![b'{'; MAX_FRAME + 1],
            // THE TWO NEW VERBS BELONG IN THIS SWEEP. They are the first
            // messages that can reach a PTY, so "the first message must be
            // hello" has to hold for them exactly as it does for a ping — an
            // unauthenticated socket that could type would make the token
            // decorative.
            br#"{"t":"input","sid":1,"text":"rm -rf /"}"#.to_vec(),
            br#"{"t":"key","sid":1,"key":"enter"}"#.to_vec(),
        ];
        for frame in junk {
            let mut p = Protocol::new("right");
            let step = p.on_text(&frame);
            let Step::Fail(frames) = &step else {
                panic!("pre-hello junk must close, got {step:?}")
            };
            assert_eq!(frames.len(), 1);
            assert!(
                matches!(frames[0], BridgeMsg::HelloErr { .. }),
                "leaked a non-hello_err before the handshake: {:?}",
                frames[0]
            );
            assert!(!p.established());
        }
        // …and a binary frame, which does not even reach the json layer.
        let mut p = Protocol::new("right");
        let Step::Fail(frames) = p.on_binary() else { panic!("binary must close") };
        assert!(matches!(frames[0], BridgeMsg::HelloErr { .. }));
    }

    #[test]
    fn a_ping_before_hello_does_not_get_a_pong() {
        let mut p = Protocol::new("k");
        let step = p.on_text(br#"{"t":"ping"}"#);
        let Step::Fail(frames) = &step else { panic!("expected a close") };
        assert_eq!(code(&frames[0]), "expected_hello");
    }

    // ---- established ----

    fn established() -> Protocol {
        let mut p = Protocol::new("k");
        assert_eq!(p.on_text(&hello("k")), Step::Accept);
        p
    }

    #[test]
    fn ping_gets_pong() {
        let mut p = established();
        assert_eq!(p.on_text(br#"{"t":"ping"}"#), Step::Send(vec![BridgeMsg::Pong]));
    }

    #[test]
    fn an_unknown_type_gets_err_and_does_not_close() {
        let mut p = established();
        let step = p.on_text(br#"{"t":"approve","sid":1}"#);
        let Step::Send(frames) = &step else {
            panic!("an unknown type must NOT close the connection, got {step:?}")
        };
        assert_eq!(code(&frames[0]), "unknown_type");
        assert!(matches!(frames[0], BridgeMsg::Err { .. }), "must be err, not hello_err");
        // Still usable afterwards — that is the whole point.
        assert!(p.established());
        assert_eq!(p.on_text(br#"{"t":"ping"}"#), Step::Send(vec![BridgeMsg::Pong]));
    }

    // ---- established: typing ----

    #[test]
    fn an_input_frame_becomes_the_daemons_phone_command() {
        let mut p = established();
        assert_eq!(
            p.on_text(br#"{"t":"input","sid":7,"text":"run the tests"}"#),
            Step::Ask { rid: 0, sid: 7, ask: PhoneAsk::Input("run the tests".into()) }
        );
        // …and the command that carries it names the session and NOTHING about
        // what kind of session it is. That omission is the security property:
        // the daemon resolves the kind from its own state, so a phone cannot
        // dress a shell up as a claude session.
        let cmd = command_for(7, PhoneAsk::Input("run the tests".into()));
        let Command::PhoneInput { id, text } = cmd else { panic!("wrong command: {cmd:?}") };
        assert_eq!(id, SessionId(7));
        assert_eq!(text, "run the tests");
    }

    #[test]
    fn a_key_frame_becomes_the_daemons_phone_key() {
        let mut p = established();
        assert_eq!(
            p.on_text(br#"{"t":"key","sid":4,"key":"escape"}"#),
            Step::Ask { rid: 0, sid: 4, ask: PhoneAsk::Key(PhoneKey::Escape) }
        );
        let cmd = command_for(4, PhoneAsk::Key(PhoneKey::Escape));
        let Command::PhoneKey { id, key } = cmd else { panic!("wrong command: {cmd:?}") };
        assert_eq!(id, SessionId(4));
        assert_eq!(key, PhoneKey::Escape);
    }

    #[test]
    fn an_unknown_key_name_is_answered_and_never_reaches_the_daemon() {
        // A phone one version ahead pressing a key this build has no word for.
        // It must be an ANSWER — the phone is waiting on a specific sid — and it
        // must not become `Ask`, because there is no daemon key to ask for and
        // the nearest one is still a key the user did not press.
        let mut p = established();
        let step = p.on_text(br#"{"t":"key","sid":4,"key":"f7"}"#);
        let Step::Send(frames) = &step else {
            panic!("an unknown key must not close the connection or reach the daemon: {step:?}")
        };
        assert_eq!(frames.len(), 1);
        assert!(
            matches!(&frames[0], BridgeMsg::InputResult { sid: 4, ok: false, reason: Some(_), .. }),
            "expected a refused input_result for sid 4, got {:?}",
            frames[0]
        );
        // Still a usable connection afterwards.
        assert!(p.established());
        assert_eq!(p.on_text(br#"{"t":"ping"}"#), Step::Send(vec![BridgeMsg::Pong]));
    }

    #[test]
    fn typing_before_hello_is_refused_and_does_not_establish() {
        // The pre-hello rule, stated once more against the two verbs that can
        // reach a PTY. `nothing_but_hello_err_is_ever_sent_before_a_successful_hello`
        // sweeps the same frames; this one names the code, so a regression that
        // turned input into a keep-alive `err` — and therefore into a message an
        // unauthenticated socket could keep sending — is legible.
        for frame in [
            &br#"{"t":"input","sid":1,"text":"rm -rf /"}"#[..],
            &br#"{"t":"key","sid":1,"key":"enter"}"#[..],
        ] {
            let mut p = Protocol::new("k");
            let step = p.on_text(frame);
            let Step::Fail(frames) = &step else {
                panic!("input before hello must close, got {step:?}")
            };
            assert_eq!(code(&frames[0]), "expected_hello");
            assert!(matches!(frames[0], BridgeMsg::HelloErr { .. }));
            assert!(!p.established());
        }
    }

    #[test]
    fn an_oversized_frame_gets_err_and_does_not_close() {
        let mut p = established();
        let step = p.on_text(&vec![b'{'; MAX_FRAME + 1]);
        let Step::Send(frames) = &step else { panic!("expected a keep-alive err, got {step:?}") };
        assert_eq!(code(&frames[0]), "frame_too_large");
        assert_eq!(p.on_text(br#"{"t":"ping"}"#), Step::Send(vec![BridgeMsg::Pong]));
    }

    #[test]
    fn bad_json_gets_err_and_does_not_close() {
        let mut p = established();
        let step = p.on_text(b"{oops");
        let Step::Send(frames) = &step else { panic!("expected a keep-alive err") };
        assert_eq!(code(&frames[0]), "bad_json");
    }

    #[test]
    fn a_second_hello_is_refused_without_closing() {
        let mut p = established();
        let step = p.on_text(&hello("k"));
        let Step::Send(frames) = &step else { panic!("expected a keep-alive err") };
        assert_eq!(code(&frames[0]), "already_hello");
    }

    // ---- hub ----

    #[test]
    fn rev_is_monotonic_per_session() {
        let hub = Hub::new("e1");
        assert_eq!(hub.upsert(session(1, WirePhase::Idle)), Some(1));
        assert_eq!(hub.upsert(session(1, WirePhase::Busy)), Some(2));
        assert_eq!(hub.upsert(session(1, WirePhase::Awaiting)), Some(3));
        assert_eq!(hub.rev_of(1), Some(3));
    }

    #[test]
    fn rev_counts_per_session_not_globally() {
        let hub = Hub::new("e1");
        hub.upsert(session(1, WirePhase::Idle));
        hub.upsert(session(2, WirePhase::Idle));
        hub.upsert(session(2, WirePhase::Busy));
        assert_eq!(hub.rev_of(1), Some(1));
        assert_eq!(hub.rev_of(2), Some(2));
    }

    #[test]
    fn an_identical_upsert_is_not_re_emitted() {
        let hub = Hub::new("e1");
        let h = hub.attach_client();
        assert_eq!(hub.upsert(session(1, WirePhase::Idle)), Some(1));
        assert_eq!(hub.upsert(session(1, WirePhase::Idle)), None, "no change, no frame");
        assert_eq!(hub.rev_of(1), Some(1), "a suppressed emission must not burn a rev");
        assert_eq!(h.drain().unwrap().len(), 1);
    }

    #[test]
    fn rev_never_restarts_when_a_session_comes_back() {
        // A restarted counter would let a stale upsert the phone still holds
        // beat the newer one, and the phone would show the old phase forever.
        let hub = Hub::new("e1");
        hub.upsert(session(1, WirePhase::Idle));
        hub.upsert(session(1, WirePhase::Busy));
        hub.gone(1);
        assert_eq!(hub.rev_of(1), Some(2), "gone must not clear the counter");
        assert_eq!(hub.upsert(session(1, WirePhase::Idle)), Some(3));
    }

    #[test]
    fn a_client_gets_a_snapshot_then_only_deltas() {
        let hub = Hub::new("e1");
        hub.upsert(session(1, WirePhase::Idle));
        hub.upsert(session(2, WirePhase::Busy));

        let h = hub.attach_client();
        assert_eq!(h.snapshot.len(), 2, "the snapshot is what existed at registration");
        assert!(h.drain().unwrap().is_empty(), "nothing is queued before the first delta");

        hub.upsert(session(3, WirePhase::Awaiting));
        hub.gone(1);
        let got = h.drain().unwrap();
        assert_eq!(got.len(), 2);
        assert!(matches!(&got[0], BridgeMsg::Session { rev: 1, session, .. } if session.sid == 3));
        assert!(matches!(got[1], BridgeMsg::Gone { sid: 1, .. }));
    }

    #[test]
    fn a_session_registered_before_the_snapshot_is_never_sent_twice() {
        // The gap this closes: capture-then-subscribe would let a delta that
        // landed in between vanish, and subscribe-then-capture would duplicate
        // it. `attach_client` does both under one lock.
        let hub = Hub::new("e1");
        hub.upsert(session(1, WirePhase::Idle));
        let h = hub.attach_client();
        assert_eq!(h.snapshot[0].sid, 1);
        assert!(h.drain().unwrap().is_empty());
    }

    #[test]
    fn every_broadcast_frame_carries_the_epoch() {
        let hub = Hub::new("epoch-abc");
        let h = hub.attach_client();
        hub.upsert(session(1, WirePhase::Idle));
        hub.gone(1);
        let frames = h.drain().unwrap();
        assert_eq!(frames.len(), 2);
        for f in &frames {
            let v: serde_json::Value = serde_json::from_str(&f.to_frame()).unwrap();
            assert_eq!(v["epoch"], "epoch-abc", "unstamped frame: {f:?}");
        }
        // …as do the two frames the connection mints directly.
        for f in [hub.hello_ok(), hub.snapshot_msg(vec![])] {
            let v: serde_json::Value = serde_json::from_str(&f.to_frame()).unwrap();
            assert_eq!(v["epoch"], "epoch-abc");
        }
    }

    #[test]
    fn gone_for_an_unknown_session_says_nothing() {
        let hub = Hub::new("e1");
        let h = hub.attach_client();
        hub.gone(99);
        assert!(h.drain().unwrap().is_empty(), "a phone must not be told about a session it never had");
    }

    #[test]
    fn reset_is_published_as_a_diff_never_a_second_snapshot() {
        let hub = Hub::new("e1");
        hub.upsert(session(1, WirePhase::Idle));
        hub.upsert(session(2, WirePhase::Idle));
        let h = hub.attach_client();

        hub.reset(vec![session(2, WirePhase::Busy), session(3, WirePhase::Idle)]);

        let frames = h.drain().unwrap();
        assert!(
            frames.iter().all(|f| !matches!(f, BridgeMsg::Sessions { .. })),
            "the contract allows exactly one snapshot per connection"
        );
        assert!(frames.iter().any(|f| matches!(f, BridgeMsg::Gone { sid: 1, .. })));
        assert!(frames
            .iter()
            .any(|f| matches!(f, BridgeMsg::Session { session, .. } if session.sid == 3)));
        let sids: Vec<u64> = hub.sessions().iter().map(|s| s.sid).collect();
        assert_eq!(sids, vec![2, 3]);
    }

    #[test]
    fn a_detached_client_stops_receiving_and_reads_as_closed() {
        // Detaching drops the hub's end of the queue, so `drain` reports None
        // rather than an eternally-empty Some — which is what lets the
        // connection loop notice it has been dropped (backlog blown) and hang
        // up, instead of spinning on a queue that will never fill again.
        let hub = Hub::new("e1");
        let h = hub.attach_client();
        hub.detach_client(h.id);
        hub.upsert(session(1, WirePhase::Idle));
        assert_eq!(h.drain(), None);
        // Detaching one client must not disturb the others.
        let other = hub.attach_client();
        hub.upsert(session(2, WirePhase::Idle));
        assert_eq!(other.drain().unwrap().len(), 1);
    }

    #[test]
    fn a_phone_that_stops_reading_is_dropped_not_queued_forever() {
        // Backpressure: an unbounded queue behind a black-holed tunnel is a slow
        // memory leak that also drags the daemon pump. The client is cut instead
        // and resyncs from a fresh snapshot when it returns.
        let hub = Hub::new("e1");
        let h = hub.attach_client();
        // Alternate so every upsert is a real change and actually broadcasts.
        for _ in 0..MAX_BACKLOG {
            hub.upsert(session(1, WirePhase::Idle));
            hub.upsert(session(1, WirePhase::Busy));
        }
        assert!(h.drain().is_none(), "the hub must drop a client that never drains");
        // The hub itself is unharmed and still serving whoever is left.
        let fresh = hub.attach_client();
        hub.upsert(session(2, WirePhase::Idle));
        assert_eq!(fresh.drain().unwrap().len(), 1);
    }

    #[test]
    fn the_epoch_is_random_and_stable() {
        let a = mint_epoch();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, mint_epoch(), "two attaches must not share an epoch");
        let hub = Hub::new(a.clone());
        assert_eq!(hub.epoch(), a);
        hub.upsert(session(1, WirePhase::Idle));
        assert_eq!(hub.epoch(), a, "the epoch is minted once per attach, never per message");
    }

    // ---- hub: shutdown levers ----

    #[test]
    fn disconnecting_everyone_reads_as_a_close_to_each_connection() {
        // `disconnect_all` deliberately reuses the backlog-blown path rather
        // than inventing a second teardown: `drain` returning None is already
        // the connection loop's cue to hang up.
        let hub = Hub::new("e1");
        let a = hub.attach_client();
        let b = hub.attach_client();
        assert_eq!(hub.sub_count(), 2);
        hub.disconnect_all();
        assert_eq!(hub.sub_count(), 0);
        assert_eq!(a.drain(), None);
        assert_eq!(b.drain(), None);
        // The hub itself is still usable — this cuts phones loose, it does not
        // retire the fan-out.
        let fresh = hub.attach_client();
        hub.upsert(session(1, WirePhase::Idle));
        assert_eq!(fresh.drain().unwrap().len(), 1);
        assert_eq!(hub.sub_count(), 1);
    }

    #[test]
    fn a_poisoned_hub_lock_keeps_serving_every_other_phone() {
        // The regression: with `.expect("hub lock")`, ONE panic while holding
        // this mutex poisoned it, and every later lock — the daemon pump's
        // included — panicked in turn. The bridge kept reporting itself
        // listening while nothing was ever published again.
        let hub = Arc::new(Hub::new("e1"));
        let h = hub.attach_client();
        let poisoner = Arc::clone(&hub);
        let died = std::thread::spawn(move || {
            let _guard = poisoner.state.lock().unwrap();
            panic!("a connection thread died holding the lock");
        })
        .join();
        assert!(died.is_err(), "the helper thread was supposed to panic");

        hub.upsert(session(1, WirePhase::Idle));
        assert_eq!(h.drain().unwrap().len(), 1, "the fan-out died with the poisoned lock");
        assert_eq!(hub.sessions().len(), 1);
        assert_eq!(hub.sub_count(), 1);
    }

    // ---- config: the GUI-facing door ----

    #[test]
    fn from_parts_gives_the_same_verdicts_as_the_env_path_in_words_a_window_can_show() {
        // Same rules, both directions. If these ever disagree, one of the two
        // doors into the bridge is enforcing a policy the other does not.
        assert!(Config::from_parts("k".into(), 0, "").is_err());
        assert!(Config::parse(Some("k".into()), Some("0".into()), None).is_err());
        assert!(Config::from_parts(String::new(), 8787, "").is_err());
        assert!(Config::from_parts("   ".into(), 8787, "").is_err());
        for bad in ["0.0.0.0", "192.168.1.10", "10.0.0.4", "8.8.8.8", "nonsense", "::1"] {
            assert!(Config::from_parts("k".into(), 8787, bad).is_err(), "{bad} was accepted");
            assert!(parse_bind(Some(bad.into())).is_err(), "{bad} was accepted");
        }
        // …and the same acceptances.
        assert_eq!(
            Config::from_parts("k".into(), 8787, "").unwrap(),
            Config::parse(Some("k".into()), None, None).unwrap()
        );
        assert_eq!(
            Config::from_parts("k".into(), 9001, "100.101.102.103").unwrap().bind,
            Bind::Tailnet("100.101.102.103".parse().unwrap())
        );

        // The one thing that must NOT be shared: the phrasing. An error naming
        // an environment variable in a Settings window reads as a bug in the
        // app, because the user never set an environment variable.
        for err in [
            Config::from_parts(String::new(), 8787, "").unwrap_err(),
            Config::from_parts("k".into(), 0, "").unwrap_err(),
            Config::from_parts("k".into(), 8787, "0.0.0.0").unwrap_err(),
        ] {
            assert!(!err.contains("KOD_BRIDGE"), "GUI error blames an env var: {err}");
        }
    }

    // ---- the sockets: real listeners, real clients, on loopback ----

    /// A port with nothing on it. Binding :0 and reading back the kernel's
    /// choice is what keeps these tests off 8787 and off each other — the whole
    /// module runs in one binary, in parallel.
    fn free_port() -> u16 {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port()
    }

    fn cfg_on(port: u16) -> Config {
        Config { token: "s3cret".into(), port, bind: Bind::Loopback }
    }

    /// A connected client with the websocket handshake done. The read timeout is
    /// the difference between a FAILING test and a hung test run.
    fn dial(port: u16) -> Result<WebSocket<TcpStream>, String> {
        let tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).map_err(|e| e.to_string())?;
        tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let (ws, _) =
            tungstenite::client(format!("ws://127.0.0.1:{port}/"), tcp).map_err(|e| e.to_string())?;
        Ok(ws)
    }

    /// hello → hello_ok → the one snapshot: an established phone.
    fn establish(ws: &mut WebSocket<TcpStream>) {
        ws.send(Message::Text(String::from_utf8(hello("s3cret")).unwrap().into())).unwrap();
        for expected in ["hello_ok", "sessions"] {
            match ws.read().expect("the bridge hung up mid-handshake") {
                Message::Text(s) => {
                    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
                    assert_eq!(v["t"], expected, "got {s}");
                }
                other => panic!("expected {expected}, got {other:?}"),
            }
        }
    }

    /// Poll for something the server threads do on their own. A fixed sleep
    /// would have to be either flaky or slow, and promptness is the property
    /// under test.
    fn within(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < limit {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        done()
    }

    #[cfg(unix)]
    mod raw {
        use std::os::raw::c_int;
        // One libc symbol, declared rather than depended on: this crate has no
        // `libc` dependency and a flag check is not a reason to grow the tree.
        extern "C" {
            pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        }
        pub const F_GETFL: c_int = 3;
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        pub const O_NONBLOCK: c_int = 0x0004;
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        pub const O_NONBLOCK: c_int = 0o4000;
    }

    #[cfg(unix)]
    fn is_nonblocking(s: &TcpStream) -> bool {
        use std::os::unix::io::AsRawFd;
        let flags = unsafe { raw::fcntl(s.as_raw_fd(), raw::F_GETFL) };
        assert!(flags >= 0, "F_GETFL failed on the accepted socket");
        flags & raw::O_NONBLOCK != 0
    }

    #[cfg(unix)]
    #[test]
    fn an_accepted_socket_must_be_put_back_into_blocking_mode() {
        // MEASURED, NOT ASSUMED (in C, on this Mac, then reproduced here):
        // Darwin's accept() hands back a socket that INHERITED the listener's
        // O_NONBLOCK. The C run: the accepted fd reported O_NONBLOCK=1, a read
        // with SO_RCVTIMEO=1s returned EAGAIN after 0.0 ms, and the same read
        // took 1000.7 ms once the flag was cleared. In `conn` that instant
        // WouldBlock is indistinguishable from an expired hello deadline, so
        // every phone would be dropped the moment it connected.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        // Connects and then says NOTHING, like a socket mid-handshake.
        let _client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();

        let mut accepted = None;
        assert!(
            within(Duration::from_secs(2), || {
                match listener.accept() {
                    Ok((s, _)) => {
                        accepted = Some(s);
                        true
                    }
                    Err(_) => false,
                }
            }),
            "nothing was accepted"
        );
        let accepted = accepted.unwrap();
        assert!(
            is_nonblocking(&accepted),
            "the inheritance this fix exists for is gone — re-measure before deleting the \
             set_nonblocking(false) in serve_with"
        );

        // The consequence, which is the part that actually bit: a read timeout
        // on an inherited-nonblocking socket is a no-op.
        accepted.set_read_timeout(Some(Duration::from_millis(400))).unwrap();
        let mut buf = [0u8; 8];
        let start = Instant::now();
        let _ = std::io::Read::read(&mut &accepted, &mut buf);
        assert!(start.elapsed() < Duration::from_millis(200), "expected an instant WouldBlock");

        // The fix, and the proof it restores the timeout.
        accepted.set_nonblocking(false).unwrap();
        assert!(!is_nonblocking(&accepted));
        let start = Instant::now();
        let _ = std::io::Read::read(&mut &accepted, &mut buf);
        assert!(
            start.elapsed() >= Duration::from_millis(300),
            "set_read_timeout is still a no-op: the read returned in {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_phone_is_not_dropped_the_instant_it_connects() {
        // The same defect at the level a user would meet it: with the listener
        // non-blocking and no `set_nonblocking(false)` after accept, this
        // handshake fails every time and the bridge still claims to be
        // listening. It is the end-to-end guard on that one line.
        let port = free_port();
        let stop = Arc::new(AtomicBool::new(false));
        let hub = Arc::new(Hub::new("e1"));
        hub.upsert(session(1, WirePhase::Idle));
        let started = serve_with(&cfg_on(port), Arc::clone(&hub), Arc::clone(&stop), no_daemon()).unwrap();
        assert_eq!(started.endpoints, vec![format!("127.0.0.1:{port}")]);

        let mut ws = dial(port).expect("the websocket handshake never completed");
        establish(&mut ws);
        // Established means SUBSCRIBED, not merely upgraded.
        assert!(within(Duration::from_secs(2), || hub.sub_count() == 1));
        // And the connection is alive afterwards rather than dropped once the
        // snapshot is out: a delta minted now must arrive.
        hub.upsert(session(1, WirePhase::Busy));
        match ws.read().expect("a live connection stopped delivering") {
            Message::Text(s) => assert_eq!(
                serde_json::from_str::<serde_json::Value>(&s).unwrap()["t"],
                "session"
            ),
            other => panic!("expected a session delta, got {other:?}"),
        }

        stop.store(true, Ordering::Relaxed);
        started.join();
    }

    #[test]
    fn stopping_frees_the_port_for_the_very_next_start() {
        // What joining the accept threads buys. The flag alone does not release
        // anything: the thread owns the TcpListener, so until it has exited the
        // port is still bound and a GUI that toggled the bridge off and on again
        // would get EADDRINUSE from a server it had just stopped.
        let port = free_port();
        let stop = Arc::new(AtomicBool::new(false));
        let first = serve_with(&cfg_on(port), Arc::new(Hub::new("e1")), Arc::clone(&stop), no_daemon()).unwrap();
        // Prove it is really serving, so a rebind below cannot pass because the
        // first start quietly failed.
        establish(&mut dial(port).expect("the first server must accept"));

        stop.store(true, Ordering::Relaxed);
        first.join();

        let stop2 = Arc::new(AtomicBool::new(false));
        let second = serve_with(&cfg_on(port), Arc::new(Hub::new("e2")), Arc::clone(&stop2), no_daemon())
            .expect("rebinding the same port after a clean stop must work");
        establish(&mut dial(port).expect("the second server must accept"));
        stop2.store(true, Ordering::Relaxed);
        second.join();
    }

    #[test]
    fn a_stopped_bridge_hangs_up_on_its_phones_instead_of_leaving_them_hanging() {
        let port = free_port();
        let stop = Arc::new(AtomicBool::new(false));
        let hub = Arc::new(Hub::new("e1"));
        let started = serve_with(&cfg_on(port), Arc::clone(&hub), Arc::clone(&stop), no_daemon()).unwrap();
        let mut ws = dial(port).unwrap();
        establish(&mut ws);
        assert!(within(Duration::from_secs(2), || hub.sub_count() == 1));

        stop.store(true, Ordering::Relaxed);
        let start = Instant::now();
        let msg = ws.read().expect("no close frame: the phone was left hanging on a dead server");
        assert!(matches!(msg, Message::Close(_)), "expected a close frame, got {msg:?}");
        // The bound is one POLL plus scheduling. Two seconds is a HANG detector,
        // not a stopwatch — the failure this guards against is "never".
        assert!(start.elapsed() < Duration::from_secs(2), "close took {:?}", start.elapsed());
        // …and the subscription goes with it, so `sub_count` is not left
        // counting a connection that has been told to go away.
        assert!(within(Duration::from_secs(2), || hub.sub_count() == 0));

        started.join();
    }

    #[test]
    fn the_idle_reaper_stays_looser_than_the_phones_own_rule() {
        // Read off the shipped client (ios/Kod/Net/BridgeClient.swift):
        //     pingEvery   = 20  — it speaks three times a minute, unprompted
        //     idleTimeout = 45  — it abandons a link that goes quiet for 45s
        // The server's tolerance has to sit ABOVE both. Below 45s the bridge
        // starts hanging up on phones that are perfectly alive, and the user
        // watches an endless reconnect loop caused entirely by the fix meant to
        // clean up dead ones. This pins the relationship, not the number.
        let phone_pings_every = Duration::from_secs(20);
        let phone_gives_up_after = Duration::from_secs(45);
        assert!(
            IDLE_TIMEOUT > phone_gives_up_after,
            "the bridge would reap links the phone still considers healthy"
        );
        assert!(
            IDLE_TIMEOUT >= 3 * phone_pings_every,
            "a phone must be able to miss a ping without being hung up on"
        );
        // …and still be a small multiple of that, not the minutes TCP would
        // spend on a half-open socket — which is the leak this closes.
        assert!(IDLE_TIMEOUT <= 3 * phone_gives_up_after);
    }

    /// The reaper itself, in real time — hence `#[ignore]`, because it costs a
    /// whole [`IDLE_TIMEOUT`] to run:
    ///
    /// ```text
    /// cargo test -p orchestrator-bridge -- --ignored
    /// ```
    ///
    /// The test above pins the POLICY in the fast suite; this one pins the
    /// ENFORCEMENT, which cannot be observed any quicker without making the
    /// timeout injectable — and then the shipped value would be the one number
    /// no test ever exercised.
    #[test]
    #[ignore = "waits out the real IDLE_TIMEOUT (~60s)"]
    fn a_phone_that_goes_silent_is_reaped_rather_than_held_forever() {
        let port = free_port();
        let stop = Arc::new(AtomicBool::new(false));
        let hub = Arc::new(Hub::new("e1"));
        let started = serve_with(&cfg_on(port), Arc::clone(&hub), Arc::clone(&stop), no_daemon()).unwrap();
        let mut ws = dial(port).unwrap();
        establish(&mut ws);
        assert!(within(Duration::from_secs(2), || hub.sub_count() == 1));
        // Long enough to outlast the reaper, so a failure here is an assertion
        // and not the client giving up first.
        ws.get_ref().set_read_timeout(Some(IDLE_TIMEOUT + Duration::from_secs(30))).unwrap();

        // From here the client says NOTHING and never closes — the state a phone
        // that walked out of wifi leaves behind. TCP alone would hold this open
        // for minutes, with the thread and the subscription still on the books.
        let start = Instant::now();
        let msg = ws.read().expect("the connection was never reaped");
        assert!(matches!(msg, Message::Close(_)), "expected a close frame, got {msg:?}");
        assert!(start.elapsed() >= IDLE_TIMEOUT, "reaped EARLY, at {:?}", start.elapsed());
        assert!(
            start.elapsed() < IDLE_TIMEOUT + Duration::from_secs(5),
            "reaped late, at {:?}",
            start.elapsed()
        );
        assert!(
            within(Duration::from_secs(2), || hub.sub_count() == 0),
            "the subscription outlived the connection, which is the leak itself"
        );

        stop.store(true, Ordering::Relaxed);
        started.join();
    }

    #[test]
    fn the_ninth_connection_is_refused_and_the_slot_comes_back() {
        let port = free_port();
        let stop = Arc::new(AtomicBool::new(false));
        let hub = Arc::new(Hub::new("e1"));
        let started = serve_with(&cfg_on(port), Arc::clone(&hub), Arc::clone(&stop), no_daemon()).unwrap();

        let mut live: Vec<WebSocket<TcpStream>> = Vec::new();
        for i in 0..MAX_CONNS {
            let mut ws = dial(port).unwrap_or_else(|e| panic!("connection {i} was refused: {e}"));
            establish(&mut ws);
            live.push(ws);
        }
        // Established, so every slot is certainly taken: the counter is
        // incremented before the connection thread is even spawned.
        assert!(within(Duration::from_secs(2), || hub.sub_count() == MAX_CONNS));

        assert!(dial(port).is_err(), "the {}th connection was not refused", MAX_CONNS + 1);

        // The half that makes the cap safe: it is a gate, not a fuse. Hang one
        // up and the freed slot is usable — without the decrement, MAX_CONNS
        // would ratchet to zero and lock the user out of their own bridge.
        drop(live.pop());
        assert!(
            within(Duration::from_secs(3), || hub.sub_count() == MAX_CONNS - 1),
            "a dropped connection never gave its subscription back"
        );
        let mut readmitted = dial(port).expect("a freed slot must be reusable");
        establish(&mut readmitted);

        stop.store(true, Ordering::Relaxed);
        started.join();
    }

    // ---- the daemon link: a real socket, a stand-in daemon on the far end ----

    fn info(sid: u64) -> SessionInfo {
        SessionInfo {
            id: SessionId(sid),
            kind: CliKind::Claude,
            project_slug: "kod".into(),
            title: "kod — main".into(),
            phase: Phase::Idle,
            alive: true,
            pending: None,
            dirty: 0,
            cli_session_id: None,
            last_message: String::new(),
            phase_since_ms: 1,
            trouble: None,
            usage_limit: None,
        }
    }

    /// Wire up the two halves of a socketpair exactly as `serve_attached` does:
    /// a sender thread on the write half, a pump thread on the read half.
    ///
    /// The `Arc<Pending>` is handed back because `serve_attached` holds one too
    /// (it keeps `pending` alive for the whole life of `pump`). A test that let
    /// it drop would be testing a different arrangement: with the last `Arc`
    /// gone, every waiting `SyncSender` is dropped along with the map and the
    /// waiters wake on `Disconnected` — which would quietly stand in for the
    /// answers `fail_all` is supposed to send.
    fn linked(hub: &Arc<Hub>) -> (Sender<PhoneRequest>, UnixStream, Arc<Pending>) {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        let reader = ours.try_clone().expect("clone the read half");
        let (tx, rx) = mpsc::channel::<PhoneRequest>();
        let pending = Arc::new(Pending::default());
        {
            let pending = Arc::clone(&pending);
            std::thread::spawn(move || send_loop(ours, rx, &pending));
        }
        {
            let hub = Arc::clone(hub);
            let pending = Arc::clone(&pending);
            std::thread::spawn(move || {
                let mut reader = reader;
                let mut mirror = Mirror::default();
                let _ = pump(&mut reader, &mut mirror, &hub, &pending);
            });
        }
        (tx, theirs, pending)
    }

    #[test]
    fn the_daemon_link_round_trips_asks_from_several_phones_over_one_socket() {
        // THE PLUMBING, end to end, on a real socket. Three connection threads
        // post at once; one sender thread does every write; one pump thread does
        // every read; each answer finds the phone that asked for it.
        //
        // That only ONE thread writes the socket is proved structurally rather
        // than asserted: the daemon wire is length-prefixed bincode, so two
        // threads writing concurrently would interleave halves of two frames and
        // the far end's `read_frame` would fail or decode garbage. Three intact
        // requests arriving is that not happening.
        let hub = Arc::new(Hub::new("e1"));
        let (tx, sock, _pending) = linked(&hub);

        // The stand-in daemon says NOTHING until the asks arrive, which is the
        // other half of the design under test: a sender that waited for daemon
        // traffic before draining the channel would never get these out, and
        // every phone below would come back "Kod did not answer".
        //
        // It then answers IN REVERSE, so a reply matched by arrival order instead
        // of by request_id would go to the wrong phone.
        let daemon = std::thread::spawn(move || {
            let mut sock = sock;
            let mut asked = Vec::new();
            for _ in 0..3 {
                let msg: ClientMsg = read_frame(&mut sock).expect("a corrupt frame reached Kod");
                let ClientMsg::Request { request_id, command } = msg else {
                    panic!("the bridge sent something that was not a request")
                };
                let sid = match &command {
                    Command::PhoneInput { id, .. } | Command::PhoneKey { id, .. } => id.0,
                    other => panic!("a phone reached a command it may not send: {other:?}"),
                };
                asked.push((sid, request_id));
            }
            for (sid, request_id) in asked.iter().rev() {
                let reply = if *sid == 2 {
                    CommandReply::Error("that session has ended".into())
                } else {
                    CommandReply::Ok
                };
                write_frame(&mut sock, &ServerMsg::Reply { request_id: *request_id, reply })
                    .expect("reply");
            }
            asked.iter().map(|(sid, _)| *sid).collect::<Vec<_>>()
        });

        let phones: Vec<_> = [1u64, 2, 3]
            .into_iter()
            .map(|sid| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    (sid, ask_daemon(&tx, command_for(sid, PhoneAsk::Input(format!("hi {sid}")))))
                })
            })
            .collect();
        let mut got: Vec<(u64, PhoneOutcome)> =
            phones.into_iter().map(|h| h.join().expect("a phone thread panicked")).collect();
        got.sort_by_key(|(sid, _)| *sid);

        assert_eq!(got[0], (1, PhoneOutcome::Ok));
        assert_eq!(
            got[1],
            (2, PhoneOutcome::Refused("that session has ended".into())),
            "a refusal reached the wrong phone, or lost the daemon's own words"
        );
        assert_eq!(got[2], (3, PhoneOutcome::Ok));

        let mut asked = daemon.join().expect("the stand-in daemon panicked");
        asked.sort_unstable();
        assert_eq!(asked, vec![1, 2, 3], "an ask was lost or duplicated on the way to the daemon");
    }

    #[test]
    fn an_ask_the_daemon_never_answers_does_not_stall_session_updates() {
        // THE PROPERTY THE SPLIT EXISTS FOR. One input is outstanding and will
        // never be answered; session updates must keep arriving anyway, because
        // the pump thread blocks on the daemon socket and on nothing else. The
        // naive one-threaded pump — drain the channel, then read — fails this the
        // other way round: it would be the INPUT that waits, forever, on an idle
        // daemon that has nothing to say.
        let hub = Arc::new(Hub::new("e1"));
        let (tx, mut sock, _pending) = linked(&hub);

        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        tx.send(PhoneRequest {
            command: command_for(9, PhoneAsk::Input("are you there".into())),
            reply: reply_tx,
        })
        .expect("the sender thread is gone");

        // The stand-in daemon takes the ask, says nothing about it, and publishes
        // a session change instead.
        let msg: ClientMsg = read_frame(&mut sock).expect("the ask never arrived");
        assert!(matches!(msg, ClientMsg::Request { .. }));
        write_frame(
            &mut sock,
            &ServerMsg::Event(ServerEvent {
                seq: 1,
                session_id: SessionId(1),
                kind: EventKind::Info(info(1)),
            }),
        )
        .expect("event");

        assert!(
            within(Duration::from_secs(2), || hub.sessions().len() == 1),
            "the session stream stopped while an input was outstanding — the pump is \
             waiting on something other than the daemon socket"
        );
        // …and the phone is still waiting rather than being told something untrue.
        assert_eq!(reply_rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn a_dead_daemon_link_answers_everyone_waiting_instead_of_leaving_them_to_time_out() {
        let hub = Arc::new(Hub::new("e1"));
        // `_pending` is HELD for the whole test on purpose — see `linked`. Let it
        // drop and the waiter below wakes on a disconnected channel instead, so
        // the test would pass with `fail_all` deleted.
        let (tx, sock, _pending) = linked(&hub);
        // Reading one ask proves the link was live; dropping the far end then
        // breaks it under a phone that is already waiting.
        let asker = {
            let tx = tx.clone();
            std::thread::spawn(move || {
                ask_daemon(&tx, command_for(1, PhoneAsk::Input("hello".into())))
            })
        };
        let mut sock = sock;
        let _: ClientMsg = read_frame(&mut sock).expect("the ask never arrived");
        drop(sock);
        // The next write fails, `send_loop` gives up and tells everyone at once.
        // Without `fail_all` this returns only after DAEMON_REPLY_TIMEOUT, so the
        // assertion is really about promptness.
        let started = Instant::now();
        tx.send(PhoneRequest {
            command: command_for(2, PhoneAsk::Input("anyone".into())),
            reply: mpsc::sync_channel(1).0,
        })
        .expect("the sender thread is gone");
        let outcome = asker.join().expect("the phone thread panicked");
        assert!(
            matches!(outcome, PhoneOutcome::Refused(_)),
            "a phone waiting on a dead link was told {outcome:?}"
        );
        assert!(
            started.elapsed() < DAEMON_REPLY_TIMEOUT,
            "the waiter sat out the whole timeout instead of being told: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_phone_that_types_is_told_what_the_daemon_said() {
        // The whole path a keystroke takes, over a real websocket: phone → conn
        // thread → channel → (here) the daemon → back to that same phone.
        let port = free_port();
        let stop = Arc::new(AtomicBool::new(false));
        let hub = Arc::new(Hub::new("e1"));
        let (tx, rx) = mpsc::channel::<PhoneRequest>();
        let started =
            serve_with(&cfg_on(port), Arc::clone(&hub), Arc::clone(&stop), tx).unwrap();

        // Stands in for the daemon, on one thread, exactly where `send_loop`
        // would be. It refuses sid 2 in the daemon's own words.
        let daemon = std::thread::spawn(move || {
            let mut seen: Vec<(u64, String)> = Vec::new();
            for req in rx {
                let outcome = match &req.command {
                    Command::PhoneInput { id, text } => {
                        seen.push((id.0, text.clone()));
                        if id.0 == 2 {
                            PhoneOutcome::Refused(
                                "Kod does not let a phone type into a shell".into(),
                            )
                        } else {
                            PhoneOutcome::Ok
                        }
                    }
                    // Bookkeeping, not a capability: the bridge tells the daemon
                    // how many phones are connected so the desktop settings line
                    // can say so. Allowlisted for `Phone` in the daemon for the
                    // same reason it is accepted here — the worst a lying bridge
                    // achieves is a wrong number on its owner's own screen.
                    Command::PhoneClients { .. } => PhoneOutcome::Ok,
                    // Anything ELSE reaching the daemon would mean this bridge
                    // built a command out of something the wire layer vetted as
                    // input — the one thing it must never do. Notably SendKey and
                    // SpawnShell, which are arbitrary execution.
                    other => panic!("a phone reached a command it may not send: {other:?}"),
                };
                let _ = req.reply.send(outcome);
            }
            seen
        });

        let mut ws = dial(port).unwrap();
        establish(&mut ws);
        let say = |ws: &mut WebSocket<TcpStream>, s: &str| {
            ws.send(Message::Text(s.to_string().into())).unwrap();
        };
        let answer = |ws: &mut WebSocket<TcpStream>| -> serde_json::Value {
            match ws.read().expect("the bridge hung up on an input") {
                Message::Text(s) => serde_json::from_str(&s).unwrap(),
                other => panic!("expected a text frame, got {other:?}"),
            }
        };

        say(&mut ws, r#"{"t":"input","sid":1,"text":"run the tests"}"#);
        let v = answer(&mut ws);
        assert_eq!(v["t"], "input_result");
        assert_eq!(v["sid"], 1);
        assert_eq!(v["ok"], true);

        say(&mut ws, r#"{"t":"input","sid":2,"text":"rm -rf /"}"#);
        let v = answer(&mut ws);
        assert_eq!(v["sid"], 2);
        assert_eq!(v["ok"], false);
        assert_eq!(
            v["reason"], "Kod does not let a phone type into a shell",
            "the phone must be shown the DAEMON's reason, not this bridge's paraphrase"
        );

        // An unknown key is answered here and never posted — the stand-in daemon
        // would panic on a command it did not expect, and `seen` below proves it
        // was never asked.
        say(&mut ws, r#"{"t":"key","sid":1,"key":"f7"}"#);
        let v = answer(&mut ws);
        assert_eq!(v["t"], "input_result");
        assert_eq!(v["ok"], false);

        // …and the connection is perfectly usable afterwards.
        say(&mut ws, r#"{"t":"ping"}"#);
        assert_eq!(answer(&mut ws)["t"], "pong");

        stop.store(true, Ordering::Relaxed);
        started.join();
        drop(ws);
        let seen = daemon.join().expect("the stand-in daemon panicked");
        assert_eq!(
            seen,
            vec![(1, "run the tests".to_string()), (2, "rm -rf /".to_string())],
            "the bridge dropped an input, invented one, or forwarded the unknown key"
        );
    }
}

#[cfg(test)]
mod message_hygiene_tests {
    use super::Config;

    /// Every message from `from_parts` lands in a Settings window, so a run of
    /// whitespace is a visible defect. This exists because all three of these
    /// strings once shipped with 18–25 literal spaces mid-sentence: a multi-line
    /// Rust string literal keeps the newline AND the source indentation unless the
    /// line ends in a backslash, and no test that only checks `is_err()` can see it.
    #[test]
    fn window_facing_errors_contain_no_runs_of_whitespace() {
        let errs = [
            Config::from_parts(String::new(), 8787, "").unwrap_err(),
            Config::from_parts("t".into(), 0, "").unwrap_err(),
            Config::from_parts("t".into(), 8787, "192.168.1.10").unwrap_err(),
        ];
        for e in errs {
            assert!(!e.contains("  "), "double space in a user-facing error: {e:?}");
            assert!(!e.contains('\n'), "newline in a user-facing error: {e:?}");
            assert!(!e.contains("KOD_BRIDGE"), "names an env var the user never set: {e:?}");
        }
    }
}
