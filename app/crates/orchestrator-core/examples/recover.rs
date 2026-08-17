use orchestrator_core::scan::recoverable_sessions;
fn main() {
    let s = recoverable_sessions(7, 12);
    for r in &s {
        let kind = if r.is_codex { "codex" } else { "claude" };
        let mb = r.bytes as f64 / 1048576.0;
        println!("[{kind}] start={} end={} {}msgs {:.2}MB  {}",
            r.started_secs, r.ended_secs, r.turns, mb,
            r.cwd.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
    }
}
