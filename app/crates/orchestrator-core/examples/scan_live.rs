//! Prints the REAL resolved registry from this machine (eyeball the gates).
use orchestrator_core::registry::resolve;
use orchestrator_core::scan::build_snapshot;
fn main() {
    let t = std::time::Instant::now();
    let snap = build_snapshot();
    let reg = resolve(&snap);
    eprintln!("scan: {} sources in {:?}", snap.sources.len(), t.elapsed());
    println!("{} project rows, {} candidates:\n", reg.rows.len(), reg.candidates.len());
    for r in &reg.rows {
        println!("• {:<22} [{}]  {}c/{}x  {}",
            r.display_name, r.canonical_key, r.claude_sessions, r.codex_sessions,
            if r.is_git { "git" } else { "non-git" });
        if let Some(d) = &r.did { println!("    DID: {}", d.lines().next().unwrap_or("").chars().take(80).collect::<String>()); }
    }
    println!("\ncandidates:");
    for c in &reg.candidates { println!("  ? {} ({:?})", c.proposed_path.display(), c.reason); }
}
