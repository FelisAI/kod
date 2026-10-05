//! Render a handoff packet for a REAL transcript (docs/028 §10) — the check that
//! the parsers hold on a live session, not just on fixtures. Reads files and runs
//! git; spawns no CLI. Run manually:
//!
//!   cargo run --release -p orchestrator-host --example handoff_packet -- \
//!       <codex|claude> <transcript.jsonl> <cwd> <out-dir> [source-label] [claude|codex]
//!
//! Prints the counts and the time each stage took.

use std::path::PathBuf;
use std::time::Instant;

use orchestrator_host::handoff::{self, PacketRequest};
use orchestrator_host::CliKind;

fn kind(s: &str) -> CliKind {
    match s {
        "codex" => CliKind::Codex,
        "claude" => CliKind::Claude,
        other => panic!("unknown CLI {other:?}: use codex or claude"),
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 4 {
        eprintln!("usage: handoff_packet <codex|claude> <transcript> <cwd> <out-dir> [source-label] [target]");
        std::process::exit(2);
    }
    let source = kind(&a[0]);
    let transcript = PathBuf::from(&a[1]);
    let t0 = Instant::now();
    let log = handoff::read_log(source, &transcript).expect("read transcript");
    let parsed = t0.elapsed();
    println!(
        "parsed in {parsed:.2?}: {} turns · {} user messages · {} commands ({} failed) · {} compactions · last working turn {:?}",
        log.turns.len(),
        log.user_messages(),
        log.commands(),
        log.failed_commands(),
        log.compactions,
        log.last_working_turn().map(|t| t.n)
    );
    let req = PacketRequest {
        source_kind: source,
        transcript: Some(transcript),
        cwd: PathBuf::from(&a[2]),
        source_label: a.get(4).cloned().unwrap_or_else(|| "default account".to_string()),
        target_kind: a.get(5).map(|s| kind(s)).unwrap_or(CliKind::Claude),
        confirm_first: true,
        earlier_packet: None,
    };
    let t1 = Instant::now();
    let p = handoff::write_packet(&req, &PathBuf::from(&a[3])).expect("write packet");
    println!("packet in {:.2?} (parse + git + render) → {}", t1.elapsed(), p.dir.display());
    for f in ["handoff.md", "commands.md", "prompt.md"] {
        let n = std::fs::metadata(p.dir.join(f)).map(|m| m.len()).unwrap_or(0);
        println!("  {f:<12} {:>9.1} KB", n as f64 / 1024.0);
    }
}
