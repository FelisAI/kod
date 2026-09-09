//! What Recover can see. Optional args are the CODEX_HOME dirs of profiled
//! codex accounts — a session started under a profile writes its rollouts
//! there, not to ~/.codex, so without them it is invisible to this scan.
//!
//!     cargo run -p orchestrator-core --example recover -- ~/.codex-team
use orchestrator_core::scan::{codex_sessions_root, recoverable_sessions_in};
use std::path::{Path, PathBuf};
fn main() {
    let roots: Vec<PathBuf> = std::env::args()
        .skip(1)
        .map(|d| codex_sessions_root(Some(Path::new(&d))))
        .collect();
    let s = recoverable_sessions_in(7, 12, &roots);
    for r in &s {
        let kind = if r.is_codex { "codex" } else { "claude" };
        let mb = r.bytes as f64 / 1048576.0;
        println!("[{kind}] start={} end={} {}msgs {:.2}MB  {}",
            r.started_secs, r.ended_secs, r.turns, mb,
            r.cwd.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
    }
}
