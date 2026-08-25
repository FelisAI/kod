//! DEV ONLY — prove a phone's typing actually reaches an agent session's PTY.
//!
//! The refusal path is easy to test against a shell. The ACCEPT path needs a
//! session whose `CliKind` is Claude or Codex, and spawning a real agent costs
//! credits and an authenticated home. `SpawnSpec.program` is arbitrary, so this
//! spawns a session the daemon KINDS as Claude while actually running `cat` —
//! which echoes whatever is typed straight back into the grid. Real PTY, real
//! kind check, no credits, and the echo is the proof the bytes landed.
//!
//!   cargo run -p orchestrator-bridge --example verify_input -- <socket> <marker>

use orchestrator_bridge::client::Client;
use orchestrator_host::protocol::{ClientRole, Command, ServerMsg};
use orchestrator_host::pty::SpawnSpec;
use orchestrator_host::session::CliKind;
use std::path::PathBuf;

fn main() {
    let mut a = std::env::args().skip(1);
    let sock = PathBuf::from(a.next().expect("usage: verify_input <socket> <marker>"));
    let marker = a.next().expect("usage: verify_input <socket> <marker>");
    if !sock.to_string_lossy().contains("koddemo") {
        eprintln!("refusing: {} is not a sandbox socket", sock.display());
        std::process::exit(3);
    }

    let (mut c, _welcome) = Client::attach(&sock, ClientRole::Full).expect("attach");
    let spec = SpawnSpec {
        program: "cat".into(),
        args: vec![],
        cwd: std::env::temp_dir(),
        env: vec![],
        rows: 24,
        cols: 80,
        effort: String::new(),
        initial_prompt: String::new(),
    };
    c.send(Command::Spawn {
        project_slug: "demo:agentish".into(),
        kind: CliKind::Claude,
        spec,
    })
    .expect("send");

    println!("spawned a Claude-kinded session running `cat`; watching for {marker:?}");
    for _ in 0..4000 {
        match c.next() {
            Ok(ServerMsg::Event(ev)) => {
                if let orchestrator_host::protocol::EventKind::Grid(g) = &ev.kind {
                    for line in g.plain_lines() {
                        if line.contains(&marker) {
                            println!("ECHOED IN THE PTY: {}", line.trim());
                            return;
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("stream ended: {e}");
                return;
            }
        }
    }
    eprintln!("marker never appeared");
    std::process::exit(1);
}
