//! DEV ONLY — spawn a handful of real shell sessions in a SANDBOX daemon so the
//! phone has something to render. Shells, not agents: real PTYs through the real
//! spawn path, but no API credits and nothing that can touch a repo.
//!
//! Deliberately refuses the default socket: seeding the daemon that owns the
//! user's actual claude sessions is never what anyone wants.
//!
//!   cargo run -p orchestrator-bridge --example seed_sessions -- <socket>

use orchestrator_bridge::client::Client;
use orchestrator_host::protocol::{ClientRole, Command};
use std::path::PathBuf;

fn main() {
    let sock = match std::env::args().nth(1) {
        Some(s) => PathBuf::from(s),
        None => {
            eprintln!("usage: seed_sessions <daemon.sock>");
            std::process::exit(2);
        }
    };
    if !sock.to_string_lossy().contains("koddemo") {
        eprintln!("refusing: {} is not a sandbox socket", sock.display());
        std::process::exit(3);
    }

    let (mut c, _hello) = Client::attach(&sock, ClientRole::Full).expect("attach");
    // Spread across a few project keys so the Projects tab has real categories to
    // group. Keys are synthetic on purpose — a sandbox daemon has no registry to
    // agree with, and hardcoding anyone's actual project paths here would put a
    // developer's directory layout in the repository.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));
    // Extra arguments are project slugs, so a sandbox can be given the SAME keys
    // its snapshot already has summaries for. Without that the rail looks empty
    // next to a full Standup: a project only appears there when it has a live
    // session or a stored path, and a snapshot restores neither.
    let args: Vec<String> = std::env::args().skip(2).collect();
    let owned: Vec<&str> = args.iter().map(String::as_str).collect();
    let plan: &[&str] = if owned.is_empty() {
        &["demo:alpha", "demo:alpha", "demo:beta", "demo:gamma"]
    } else {
        &owned
    };
    for slug in plan {
        let id = c
            .send(Command::SpawnShell {
                project_slug: (*slug).into(),
                cwd: cwd.clone(),
            })
            .expect("send");
        println!("requested shell in {slug} (req {id})");
    }
    // Drain briefly so the daemon actually processes the spawns before we drop.
    for _ in 0..40 {
        match c.next() {
            Ok(_) => {}
            Err(e) => {
                eprintln!("stream ended: {e}");
                break;
            }
        }
    }
    println!("seeded");
}
