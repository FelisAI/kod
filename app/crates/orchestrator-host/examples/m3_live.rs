//! Live M3 verification (docs/014) — NOT a unit test (it spawns real claude, so
//! it burns plan quota; RULE ZERO keeps it out of `cargo test`). Run manually:
//!
//!   cargo run -p orchestrator-host --example m3_live
//!
//! Proves the SURFACING loop headlessly: spawn claude with hooks → send a prompt
//! that triggers a Write → the PermissionRequest hook raises a PendingDecision
//! carrying what the agent wants to do. Answering is deliberately not part of
//! the loop — the app has no answer channel; the human types into the real
//! dialog. Prints a pass/fail line per leg.

use std::time::{Duration, Instant};

use orchestrator_host::input::KeyInput;
use orchestrator_host::pty::SpawnSpec;
use orchestrator_host::SessionHost;

fn wait_until(mut f: impl FnMut() -> bool, ms: u64) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(ms) {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    f()
}

fn main() {
    let dir = std::env::temp_dir().join("orchestrator-m3-live");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let host = SessionHost::new();
    let mut spec = SpawnSpec::program("claude", &dir);
    spec.rows = 24;
    spec.cols = 100;
    let (id, _cli_id) = host.spawn_claude("m3-live", spec).expect("spawn claude");
    let session = host.get(id).unwrap();
    println!("spawned claude (hooks armed) in {}", dir.display());

    // Clear startup gates: a fresh dir shows the folder-trust dialog ("Quick
    // safety check … Enter to confirm"), and first launch may show a banner.
    // Send Enter until the composer ("? for shortcuts") appears.
    let composer_up = wait_until(
        || {
            let lines = session.snapshot().plain_lines();
            let trust = lines.iter().any(|l| l.contains("trust") || l.contains("safety check"));
            if trust {
                session.send_key(&KeyInput::Enter);
            }
            lines.iter().any(|l| l.contains("? for shortcuts") || l.contains("for shortcuts"))
        },
        20_000,
    );
    println!("startup gates cleared, composer up: {composer_up}");
    std::thread::sleep(Duration::from_millis(800));

    session.send_key(&KeyInput::Char(
        "Create a file named hello.txt containing the word ok. Do nothing else.".into(),
    ));
    std::thread::sleep(Duration::from_millis(900)); // let the composer ingest
    session.send_key(&KeyInput::Enter);
    println!("prompt sent; waiting for the permission dialog → PendingDecision…");

    // Self-heal: if the prompt is still sitting in the composer (Enter raced
    // the paste), press Enter again until it submits.
    let got = wait_until(
        || {
            if session.pending().is_some() {
                return true;
            }
            let lines = session.snapshot().plain_lines();
            let still_in_composer = lines.iter().any(|l| l.contains("❯ Create a file"));
            if still_in_composer {
                session.send_key(&KeyInput::Enter);
            }
            false
        },
        90_000,
    );
    if !got {
        println!("FAIL: no PendingDecision surfaced (hook didn't fire?). Grid tail:");
        for l in session.snapshot().plain_lines().iter().rev().take(8).rev() {
            println!("  | {l}");
        }
        return;
    }
    let p = session.pending().unwrap();
    println!("PASS: card raised — tool={} summary={:?}", p.tool_name, p.view.summary());
    // The body must carry what the agent wants to do — the card is all the user
    // gets before they answer the real dialog themselves.
    if p.view.summary().trim().is_empty() {
        println!("FAIL: the card is BLANK — nothing to read before deciding");
    } else {
        println!("PASS: the card shows the ask");
    }
    // The dialog is left UNANSWERED on purpose: claude is still sitting at it,
    // and only a human at the terminal may resolve it.
    println!("--- done (dialog left for the human); cleaning up ---");
    host.close(id);
    std::thread::sleep(Duration::from_millis(500));
    let _ = std::fs::remove_dir_all(&dir);
}
