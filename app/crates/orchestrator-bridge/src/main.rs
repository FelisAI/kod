//! `kod-bridge <probe|serve> <socket>`.
//!
//! * `probe` — attach to a daemon, mirror it, print what it says, exit. Slice
//!   0's proof: evidence that the bridge can be a well-behaved daemon client,
//!   which is the thing every later slice sits on.
//! * `serve` — the same attach, plus the phone-facing WebSocket server
//!   (`ws::serve`) on 127.0.0.1.
//!
//! BOTH SUBCOMMANDS RUN THE SAME PRE-FLIGHT, and that is the point of the shape
//! of `main`: the default-socket refusal and the retire check are executed once,
//! before the dispatch, so a subcommand cannot be added that quietly skips them.
//! `serve` is the more dangerous of the two — it holds the attach open for as
//! long as a phone might want it — so it is emphatically not the one to exempt.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use orchestrator_bridge::client::{retire_risk, Client, RetireRisk};
use orchestrator_bridge::mirror::{Change, Mirror};
use orchestrator_host::protocol::ServerMsg;

/// The socket `orchestrator-daemon` would pick for this user — i.e. the daemon
/// that is probably running RIGHT NOW with real work in it.
fn probable_real_socket() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("orchestrator")
        .join("daemon.sock")
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, path) = match args.as_slice() {
        [c, p] if c == "probe" || c == "serve" => (c.clone(), PathBuf::from(p)),
        _ => {
            eprintln!("usage: kod-bridge <probe|serve> <socket-path>");
            eprintln!();
            eprintln!("  probe   attach, print the session list, exit.");
            eprintln!("  serve   attach and serve the phone protocol.");
            eprintln!("          KOD_BRIDGE_TOKEN  required, no default.");
            eprintln!("          KOD_BRIDGE_PORT   default 8787.");
            eprintln!("          KOD_BRIDGE_BIND   unset = loopback only. Set it to this");
            eprintln!("                            machine's Tailscale address (`tailscale ip -4`)");
            eprintln!("                            to let your phone connect; loopback stays");
            eprintln!("                            bound too. Any other address is REFUSED —");
            eprintln!("                            v0 has no TLS, so a LAN or wildcard bind");
            eprintln!("                            would put the token in the clear.");
            eprintln!();
            eprintln!("There is deliberately NO default path. The default would be your own");
            eprintln!("running daemon, and attaching a freshly-built binary to it can RETIRE");
            eprintln!("it — killing every live agent session. Name the socket you mean.");
            return ExitCode::from(2);
        }
    };

    // The guard that matters. A newly-built client attaching to the daemon that
    // owns your real work is precisely the retire case, and it is silent until
    // the sessions are already gone.
    if path == probable_real_socket() && std::env::var("KOD_BRIDGE_ALLOW_DEFAULT").is_err() {
        eprintln!("refusing: {} is this user's DEFAULT daemon socket.", path.display());
        eprintln!();
        eprintln!("That daemon is probably hosting live agent sessions. A client built from a");
        eprintln!("newer binary makes it retire, and the sessions die with it. Point this at a");
        eprintln!("sandbox daemon instead, or set KOD_BRIDGE_ALLOW_DEFAULT=1 if you are certain.");
        return ExitCode::from(3);
    }

    // The pre-flight. A matching wire version does NOT save you here: the daemon
    // retires on `rebuilt` alone, so the only way to know an attach is safe is to
    // compare its binary against when it started.
    let daemon_bin = std::env::var("KOD_BRIDGE_DAEMON_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.join("orchestrator-daemon")))
                .unwrap_or_default()
        });
    match retire_risk(&path, &daemon_bin) {
        RetireRisk::Safe => {}
        RetireRisk::WouldRetire => {
            eprintln!("refusing: attaching would RETIRE this daemon.");
            eprintln!();
            eprintln!("  daemon binary {}", daemon_bin.display());
            eprintln!("  is NEWER than  {}", path.display());
            eprintln!();
            eprintln!("The socket's mtime is when the daemon started, so a newer binary means it");
            eprintln!("is running code that no longer exists on disk — `binary_was_rebuilt()`.");
            eprintln!("Attaching makes it exit and every live session dies. Restart the daemon");
            eprintln!("from the current build first.");
            return ExitCode::from(4);
        }
        RetireRisk::Unknown => {
            eprintln!("refusing: cannot tell whether attaching would retire the daemon.");
            eprintln!("  daemon binary {} (readable: {})", daemon_bin.display(), daemon_bin.exists());
            eprintln!("  socket        {} (readable: {})", path.display(), path.exists());
            eprintln!();
            eprintln!("Not knowing is not the same as knowing it is safe. Point");
            eprintln!("KOD_BRIDGE_DAEMON_BIN at the binary this daemon launched from.");
            return ExitCode::from(5);
        }
    }

    // Only now, with both guards passed, does anything touch the daemon.
    let outcome = match cmd.as_str() {
        "probe" => probe(&path),
        _ => orchestrator_bridge::ws::serve(&path),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bridge: {e}");
            ExitCode::FAILURE
        }
    }
}

fn probe(path: &Path) -> Result<(), String> {
    let (mut client, welcome) = Client::attach(path).map_err(|e| e.to_string())?;
    let mut mirror = Mirror::default();
    mirror.apply(&welcome);
    if let ServerMsg::Welcome { wire_version, .. } = &welcome {
        println!("attached · wire {wire_version} · {} sessions", mirror.sessions.len());
    }

    // Pump the attach snapshot only. Live streaming is the next slice; stopping
    // at ReplayDone keeps this a probe rather than a process that sits on a
    // socket accumulating frames nobody reads.
    let mut grids = 0usize;
    loop {
        let msg = client.next().map_err(|e| e.to_string())?;
        match mirror.apply(&msg) {
            Some(Change::Grid(_)) => grids += 1,
            Some(Change::ReplayDone) => break,
            _ => {}
        }
    }

    println!("replay complete · {grids} grids\n");
    let mut live: Vec<_> = mirror.live();
    live.sort_by_key(|s| (s.project_slug.clone(), s.id.0));
    for s in &live {
        let grid = mirror.grids.get(&s.id);
        println!(
            "  {:<22} {:<10} {:<8} {}",
            trim(&s.project_slug, 22),
            s.phase.label(),
            grid.map(|g| format!("{}x{}", g.rows.first().map(|r| r.len()).unwrap_or(0), g.rows.len()))
                .unwrap_or_else(|| "-".into()),
            trim(s.last_message.trim(), 60),
        );
    }
    if live.is_empty() {
        println!("  (no live sessions)");
    }
    Ok(())
}

fn trim(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}
