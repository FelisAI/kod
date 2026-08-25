//! The mobile bridge: a CHILD PROCESS this daemon starts, supervises and kills.
//!
//! ## Why a child, and why the daemon owns it
//!
//! Two constraints pull in opposite directions and this is the shape that
//! satisfies both.
//!
//! It cannot live in the GUI. `boot.rs` documents that closing the main window
//! drops the Orchestrator entity while the process keeps running, so anything the
//! GUI owned would die on ⌘W — the most ordinary gesture before walking away from
//! a desk, and the exact moment you start wanting your phone. There is no
//! reliable quit hook to do better with either. The daemon detaches with `setsid`
//! and outlives the app, so hosting it here means ⌘W, quit, crash and Force Quit
//! are all no-ops for the phone.
//!
//! It cannot live IN this process either, now that phones can type. An earlier
//! revision ran the listener in-daemon and that was defensible only while the
//! phone was a read-only peer; the note it carried said, in as many words, to move
//! the bridge out before adding input. Input is here. A network parser sharing an
//! address space with every live PTY has no boundary between "the bridge decided
//! not to type into a shell" and "the bridge cannot type into a shell", and only
//! the second one is a security property.
//!
//! So: a child process, attaching back over the unix socket as
//! `ClientRole::Phone`. The daemon refuses that role everything except
//! `PhoneInput`/`PhoneKey`, and refuses those against shells, resolving the
//! session kind from its own state. The rule is now enforced across a process
//! boundary by the side that owns the PTYs, which is the only place it means
//! anything.
//!
//! ## The token never appears in argv
//!
//! `ps` is world-readable on macOS: every argument of every process is visible to
//! every user on the machine. The token is a bearer credential for reading — and
//! now writing to — your sessions, so it goes in the child's ENVIRONMENT, which is
//! not. The socket path is the only argument.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command as OsCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use orchestrator_host::protocol::BridgeStatus;

/// Installed only by `run_default`, so the unit-test entry points never grow a
/// network listener as a side effect of running the daemon's own tests.
static BRIDGE: OnceLock<Supervisor> = OnceLock::new();

struct Running {
    child: Child,
    endpoints: Vec<String>,
    since_ms: u64,
}

pub struct Supervisor {
    socket: std::path::PathBuf,
    state: Mutex<Option<Running>>,
    /// Whether this daemon has EVER been told a config. A daemon is storage-free,
    /// so a fresh or respawned one genuinely does not know, and that must read as
    /// "waiting for Kod" rather than as a user-chosen Off.
    configured: AtomicBool,
    /// Last failure, kept so a child that died is not silently reported as Off.
    error: Mutex<Option<String>>,
    /// Phones currently connected, as last reported by the child.
    clients: Mutex<u32>,
    since_ms: Mutex<u64>,
}

pub fn install(socket: std::path::PathBuf) {
    let _ = BRIDGE.set(Supervisor {
        socket,
        state: Mutex::new(None),
        configured: AtomicBool::new(false),
        error: Mutex::new(None),
        clients: Mutex::new(0),
        since_ms: Mutex::new(now_ms()),
    });
}

/// The child binary, found beside this one.
///
/// `current_exe`'s directory is the right anchor in both layouts without knowing
/// which we are in: `target/debug/orchestrator-daemon` in development and
/// `Kod.app/Contents/MacOS/orchestrator-daemon` when shipped. Anything based on
/// the working directory would be wrong in both.
fn bridge_binary() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot locate this daemon's own binary: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "this daemon's binary has no parent directory".to_string())?;
    let p = dir.join("kod-bridge");
    if !p.exists() {
        // Loud and specific. A missing sibling binary is a PACKAGING failure, and
        // the least diagnosable version of it is a toggle that silently does
        // nothing — so name the path we looked at.
        return Err(format!(
            "the mobile bridge helper is missing from this install (looked for {}). \
             If you built Kod yourself, `cargo build -p orchestrator-bridge`.",
            p.display()
        ));
    }
    Ok(p)
}

/// Apply a configuration. Returns what the GUI should display, on the same
/// round-trip as the click that caused it.
pub fn apply(on: bool, port: u16, bind: &str, token: &str) -> BridgeStatus {
    let Some(sup) = BRIDGE.get() else {
        return BridgeStatus::unavailable(
            "this daemon was started without a bridge (it is running in a test or \
             single-command mode)",
        );
    };
    sup.configured.store(true, Ordering::SeqCst);
    // ALWAYS stop first, for any change at all: a port or bind change needs new
    // listeners, and a token change MUST invalidate the phones holding the old
    // one. A "regenerate" that leaves the previous credential working is not a
    // regeneration, it is a second valid key.
    sup.stop();

    if !on {
        *sup.error.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *sup.since_ms.lock().unwrap_or_else(|e| e.into_inner()) = now_ms();
        return sup.status();
    }
    match sup.start(port, bind, token) {
        Ok(()) => {}
        Err(e) => {
            *sup.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
        }
    }
    sup.status()
}

pub fn status() -> BridgeStatus {
    match BRIDGE.get() {
        Some(sup) => sup.status(),
        None => BridgeStatus::unavailable(
            "this daemon was started without a bridge (it is running in a test or \
             single-command mode)",
        ),
    }
}

/// The child reports its connected-phone count; the settings line shows it.
pub fn note_clients(n: u32) {
    if let Some(sup) = BRIDGE.get() {
        *sup.clients.lock().unwrap_or_else(|e| e.into_inner()) = n;
    }
}

impl Supervisor {
    fn start(&self, port: u16, bind: &str, token: &str) -> Result<(), String> {
        let bin = bridge_binary()?;
        let mut child = OsCommand::new(&bin)
            .arg("serve")
            .arg(&self.socket)
            // Environment, not argv — see the module note on `ps`.
            .env("KOD_BRIDGE_TOKEN", token)
            .env("KOD_BRIDGE_PORT", port.to_string())
            .env("KOD_BRIDGE_BIND", bind)
            // The bridge refuses the user's default socket unless told, because a
            // hand-run one attaching to the real daemon is the retire hazard. Here
            // the daemon IS that daemon and is starting the child itself, so the
            // guard has nothing left to protect against.
            .env("KOD_BRIDGE_ALLOW_DEFAULT", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start the mobile bridge: {e}"))?;

        // Read the child's OWN first line rather than re-deriving the address from
        // the config: it prints only after both listeners bound and the attach
        // succeeded, so a status line built from it cannot claim something is
        // reachable that is not.
        let endpoints = match child.stdout.take() {
            Some(out) => first_line_endpoints(out),
            None => Vec::new(),
        };
        if endpoints.is_empty() {
            // It printed nothing, which means it failed before binding. Its stderr
            // is a real sentence written for this case, so surface that rather
            // than a generic failure.
            let why = child
                .stderr
                .take()
                .map(|e| {
                    let mut s = String::new();
                    let _ = BufReader::new(e).read_line(&mut s);
                    s.trim().to_string()
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "the mobile bridge exited without starting".to_string());
            let _ = child.kill();
            let _ = child.wait();
            return Err(why);
        }

        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Some(Running {
            child,
            endpoints,
            since_ms: now_ms(),
        });
        *self.error.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.clients.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        Ok(())
    }

    /// Kill the child and REAP it.
    ///
    /// The wait is not optional: without it the child becomes a zombie, and more
    /// importantly the port is not free until the process is gone, so an
    /// immediate re-enable (or a port change back and forth) fails to bind for no
    /// visible reason.
    fn stop(&self) {
        let taken = self.state.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut r) = taken {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
        *self.clients.lock().unwrap_or_else(|e| e.into_inner()) = 0;
    }

    fn status(&self) -> BridgeStatus {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Ask the OS, do not trust our own bookkeeping. A child that crashed is
        // still `Some` in this struct until someone looks, and reporting a dead
        // bridge as running is the exact lie this pane exists to prevent.
        let alive = match st.as_mut() {
            Some(r) => match r.child.try_wait() {
                Ok(None) => true,
                _ => false,
            },
            None => false,
        };
        if !alive && st.is_some() {
            *st = None;
            *self.error.lock().unwrap_or_else(|e| e.into_inner()) =
                Some("the mobile bridge stopped unexpectedly".to_string());
        }
        let configured = self.configured.load(Ordering::SeqCst);
        let error = self.error.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match st.as_ref() {
            Some(r) => BridgeStatus {
                running: true,
                endpoints: r.endpoints.clone(),
                clients: *self.clients.lock().unwrap_or_else(|e| e.into_inner()),
                error: None,
                configured: true,
                since_ms: r.since_ms,
            },
            None => BridgeStatus {
                running: false,
                endpoints: Vec::new(),
                clients: 0,
                error,
                configured,
                since_ms: *self.since_ms.lock().unwrap_or_else(|e| e.into_inner()),
            },
        }
    }
}

/// Pull the bound addresses out of the bridge's startup banner.
///
/// The banner is one line and looks like:
///   `bridge · wire 24 · epoch <hex> · listening on 127.0.0.1:8787 and 100.x:8787 (tailnet) · 3 sessions`
pub(crate) fn parse_endpoints(line: &str) -> Vec<String> {
    let Some(rest) = line.split("listening on ").nth(1) else {
        return Vec::new();
    };
    let rest = rest.split(" · ").next().unwrap_or(rest);
    rest.split(" and ")
        .map(|s| s.replace("(tailnet)", "").trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn first_line_endpoints(out: std::process::ChildStdout) -> Vec<String> {
    let mut r = BufReader::new(out);
    let mut line = String::new();
    match r.read_line(&mut line) {
        Ok(0) | Err(_) => Vec::new(),
        Ok(_) => parse_endpoints(&line),
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
    use orchestrator_host::protocol::BridgePhase;

    /// Without `install`, every entry point answers honestly rather than
    /// panicking on a missing global — the daemon's own tests run in that state.
    /// NOTE: no test in this module may call `install`; BRIDGE is a process-wide
    /// OnceLock and setting it would leak into every other test in this binary.
    #[test]
    fn an_uninstalled_bridge_reports_unavailable_instead_of_panicking() {
        let s = status();
        assert!(!s.running);
        assert!(s.error.is_some(), "an unavailable bridge must say why");
        assert_eq!(s.phase(), BridgePhase::Failed, "and must not read as a chosen Off");
    }

    #[test]
    fn endpoints_come_from_the_childs_own_banner() {
        let line = "bridge · wire 24 · epoch abc · listening on 127.0.0.1:8787 and \
                    100.101.102.103:8787 (tailnet) · 3 sessions";
        assert_eq!(
            parse_endpoints(line),
            vec!["127.0.0.1:8787".to_string(), "100.101.102.103:8787".to_string()]
        );
    }

    #[test]
    fn a_loopback_only_banner_yields_one_endpoint() {
        let line = "bridge · wire 24 · epoch abc · listening on 127.0.0.1:8787 · 0 sessions";
        assert_eq!(parse_endpoints(line), vec!["127.0.0.1:8787".to_string()]);
    }

    /// Anything that is not the banner must yield NOTHING, so `start` treats it as
    /// "it never bound" rather than inventing an endpoint from a warning line.
    #[test]
    fn a_line_that_is_not_the_banner_yields_no_endpoints() {
        assert!(parse_endpoints("refusing: that is your DEFAULT daemon socket.").is_empty());
        assert!(parse_endpoints("").is_empty());
        assert!(parse_endpoints("listening on").is_empty());
    }

    #[test]
    fn the_helper_binary_is_looked_for_beside_this_one() {
        // Whatever the answer, it must be an absolute path next to current_exe and
        // never a bare name resolved through PATH — a $PATH lookup would let any
        // writable directory on it supply the process we hand the token to.
        if let Ok(p) = bridge_binary() {
            assert!(p.is_absolute());
            assert_eq!(p.file_name().unwrap(), "kod-bridge");
            assert_eq!(
                p.parent().unwrap(),
                std::env::current_exe().unwrap().parent().unwrap()
            );
        }
    }
}
