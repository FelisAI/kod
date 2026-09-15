//! What Recover can see. Optional args are `<claude|codex>=<dir>` profile
//! accounts — a session started under a profile writes to that profile's own
//! root, not ~/.claude or ~/.codex, so without them it is invisible to this scan.
//!
//!     cargo run -p orchestrator-core --example recover -- codex=~/.codex-team
use orchestrator_core::cli::{cli_homes, kind_from_label};
use orchestrator_core::scan::recoverable_sessions;
fn main() {
    let args: Vec<(String, String)> = std::env::args()
        .skip(1)
        .filter_map(|a| a.split_once('=').map(|(k, d)| (k.to_string(), d.to_string())))
        .collect();
    let homes = cli_homes(
        args.iter()
            .filter_map(|(k, d)| kind_from_label(k).map(|k| (k, Some(d.as_str())))),
    );
    let s = recoverable_sessions(7, 12, &homes);
    for r in &s {
        let kind = if r.is_codex { "codex" } else { "claude" };
        let mb = r.bytes as f64 / 1048576.0;
        println!("[{kind}] start={} end={} {}msgs {:.2}MB  {}",
            r.started_secs, r.ended_secs, r.turns, mb,
            r.cwd.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
    }
}
