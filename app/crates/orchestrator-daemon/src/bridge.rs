//! The mobile bridge, hosted BY THE DAEMON.
//!
//! ## Why it lives here and not in the GUI
//!
//! The feature's whole purpose is reading your sessions while you are away from
//! the Mac. Both GUI-hosted designs fail at exactly that: `boot.rs` documents that
//! closing the main window drops the Orchestrator entity while the PROCESS keeps
//! running (gpui never sets `applicationShouldTerminateAfterLastWindowClosed` and
//! nothing calls `cx.quit()`), so anything owned by that entity dies on ⌘W — the
//! most ordinary gesture there is before walking away from a desk. There is also
//! no reliable quit hook to do better with: this repo registers zero
//! `on_app_quit` handlers, and `applicationWillTerminate:` never fires on SIGKILL.
//!
//! The daemon already detaches with `setsid` and outlives the app. Hosting the
//! bridge here means ⌘W, app quit, a GUI crash and Force Quit are all no-ops for
//! the phone, and this module needs no shutdown machinery for any of them.
//!
//! It also deletes the retire hazard rather than managing it. An external
//! `kod-bridge` attaches as a client, which risks retiring the very daemon it
//! serves; in here there is no `Client::attach` at all. And it is cheaper: an
//! attached client makes the daemon spawn a coalescer that wakes every 16ms and
//! clones a full `GridSnapshot` per dirty session, all of which the phone throws
//! away because v0 renders no terminal. The feeder below reads `infos()` at 250ms.
//!
//! ## THE MIGRATION TRIGGER — read this before adding input support
//!
//! The case for putting a network parser inside the process that owns every PTY
//! rests entirely on the phone being a READ-ONLY peer: `Caps::v0().input` is
//! false, the inbound message type is deserialize-only with three variants, and
//! `peer_allowed` rejects any non-loopback/non-tailnet peer *before* the token is
//! read. A connection thread holds only `&Hub` and `&str` and never has a
//! `SessionHost` in scope.
//!
//! The day `Caps.input` becomes true, the phone becomes an untrusted WRITER into
//! this process and that argument expires. If you are reading this because you are
//! adding input support — stop, and move the bridge out into a supervised child
//! process with kernel isolation first.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use orchestrator_bridge::wire::WireSession;
use orchestrator_bridge::ws::{self, Config, Hub, Started};
use orchestrator_host::protocol::BridgeStatus;
use orchestrator_host::SessionHost;

/// How often the feeder republishes the session list.
///
/// Unconditional and idempotent by construction: `Hub::upsert` returns early on
/// an identical `WireSession` and `Hub::reset` emits a diff, so a tick where
/// nothing changed sends every phone exactly zero bytes. That is why this can be
/// a dumb poll instead of an event subscription.
const FEED_INTERVAL: Duration = Duration::from_millis(250);

/// Installed only by `run_default`, so the `serve_client` test entry points never
/// grow a network listener as a side effect of running the daemon's unit tests.
static BRIDGE: OnceLock<Supervisor> = OnceLock::new();

struct Running {
    /// Kept to answer "is this the same config?" without re-parsing.
    cfg: Config,
    stop: Arc<AtomicBool>,
    hub: Arc<Hub>,
    started: Started,
    endpoints: Vec<String>,
    since_ms: u64,
}

pub struct Supervisor {
    host: Arc<SessionHost>,
    state: Mutex<Option<Running>>,
    /// Whether this daemon has EVER been told a config. A daemon is storage-free,
    /// so a fresh (or retired-then-respawned) one genuinely does not know, and
    /// that must not read as "the user turned it off".
    configured: AtomicBool,
    /// When the current stopped state began, so an Off state can still show since.
    since_ms: Mutex<u64>,
}

/// Hand the daemon its session host so the bridge can feed from it. Called once,
/// from `run_default`.
pub fn install(host: Arc<SessionHost>) {
    let _ = BRIDGE.set(Supervisor {
        host,
        state: Mutex::new(None),
        configured: AtomicBool::new(false),
        since_ms: Mutex::new(now_ms()),
    });
}

/// Apply a bridge configuration. Returns what the GUI should display, on the same
/// round-trip as the click that caused it.
pub fn apply(on: bool, port: u16, bind: &str, token: &str) -> BridgeStatus {
    let Some(sup) = BRIDGE.get() else {
        return BridgeStatus::unavailable(
            "this daemon was started without a bridge (it is running in a test or \
             single-command mode)",
        );
    };
    sup.configured.store(true, Ordering::SeqCst);

    // ALWAYS stop first, for any change at all. A port or bind change needs new
    // listeners, and a token change MUST invalidate the phones holding the old
    // one — a "regenerate" that leaves the previous credential working is not a
    // regeneration, it is a second valid key.
    sup.stop_locked();

    if !on {
        *sup.since_ms.lock().unwrap_or_else(|e| e.into_inner()) = now_ms();
        return sup.status_locked();
    }

    let cfg = match Config::from_parts(token.to_string(), port, bind) {
        Ok(c) => c,
        Err(e) => return sup.failed(e),
    };
    // A FRESH hub every start, never a reused one: the epoch is what tells a phone
    // its cached (epoch, sid) view is void, and a restart is exactly when that
    // cache must be dropped.
    let hub = Arc::new(Hub::new(ws::mint_epoch()));
    let stop = Arc::new(AtomicBool::new(false));
    let started = match ws::serve_with(&cfg, Arc::clone(&hub), Arc::clone(&stop)) {
        Ok(s) => s,
        Err(e) => return sup.failed(e),
    };
    let endpoints = started.endpoints.clone();

    // The feeder. It shares the listeners' stop flag, so one store tears down the
    // whole bridge and there is no way to leave a feeder running against a hub
    // nobody serves.
    {
        let host = Arc::clone(&sup.host);
        let hub = Arc::clone(&hub);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                hub.reset(host.infos().iter().map(WireSession::from).collect());
                std::thread::sleep(FEED_INTERVAL);
            }
        });
    }

    let since_ms = now_ms();
    *sup.state.lock().unwrap_or_else(|e| e.into_inner()) = Some(Running {
        cfg,
        stop,
        hub,
        started,
        endpoints,
        since_ms,
    });
    sup.status_locked()
}

/// What the bridge is doing right now, for the Settings status line.
pub fn status() -> BridgeStatus {
    match BRIDGE.get() {
        Some(sup) => sup.status_locked(),
        None => BridgeStatus::unavailable(
            "this daemon was started without a bridge (it is running in a test or \
             single-command mode)",
        ),
    }
}

impl Supervisor {
    /// Stop the listeners and the feeder, and hang up on every connected phone.
    ///
    /// The JOIN is not optional. Each accept thread owns its `TcpListener`, so the
    /// port stays bound until it exits — skip this and an immediate re-enable (or
    /// a port change back and forth) fails with EADDRINUSE for no visible reason.
    fn stop_locked(&self) {
        let taken = self.state.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(r) = taken {
            r.stop.store(true, Ordering::Relaxed);
            // Clear the subscriber table first so each connection's next drain
            // returns None and it walks its EXISTING teardown — which sends a real
            // WebSocket Close. A phone then shows "disconnected" instead of
            // silently hanging until its own idle timeout.
            r.hub.disconnect_all();
            r.started.join();
        }
    }

    fn failed(&self, why: String) -> BridgeStatus {
        BridgeStatus {
            running: false,
            endpoints: Vec::new(),
            clients: 0,
            error: Some(why),
            configured: self.configured.load(Ordering::SeqCst),
            since_ms: *self.since_ms.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }

    fn status_locked(&self) -> BridgeStatus {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match st.as_ref() {
            Some(r) => BridgeStatus {
                running: true,
                endpoints: r.endpoints.clone(),
                // Read from the hub, not from a counter we maintain: the hub is
                // what the connections actually register with, so this cannot
                // drift from reality the way separate bookkeeping would.
                clients: r.hub.sub_count() as u32,
                error: None,
                configured: true,
                since_ms: r.since_ms,
            },
            None => BridgeStatus {
                running: false,
                endpoints: Vec::new(),
                clients: 0,
                error: None,
                configured: self.configured.load(Ordering::SeqCst),
                since_ms: *self.since_ms.lock().unwrap_or_else(|e| e.into_inner()),
            },
        }
    }
}

/// Silence the unused-field warning while keeping `cfg` for the debugger and for
/// the "did anything actually change?" question a future revision will want.
impl Running {
    #[allow(dead_code)]
    fn port(&self) -> u16 {
        self.cfg.port
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without `install`, every entry point must answer honestly rather than
    /// panicking on a missing global — the daemon's own unit tests run in exactly
    /// this state, and so does any single-command invocation.
    #[test]
    fn an_uninstalled_bridge_reports_unavailable_instead_of_panicking() {
        // NOTE: this test must not call `install`, and no other test in this module
        // may either — BRIDGE is a process-wide OnceLock and setting it would leak
        // into every other test in this binary.
        let s = status();
        assert!(!s.running);
        assert!(s.error.is_some(), "an unavailable bridge must say why");
    }

    #[test]
    fn an_unavailable_bridge_never_reads_as_a_user_chosen_off() {
        use orchestrator_host::protocol::BridgePhase;
        // The distinction the `configured` flag exists for: "off" is a choice the
        // user made; this is not.
        assert_eq!(status().phase(), BridgePhase::Failed);
    }
}
