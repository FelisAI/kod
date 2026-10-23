//! Release smoke test only (scripts/release-smoke.sh): write "crashed" session
//! rows — `hosted_session` alive=1, exactly what a reboot leaves behind — into a
//! throwaway store, so the app under test offers to restore them.
//!
//!     KOD_STORE_DIR=/Users/Shared/kod-smoke/store \
//!       cargo run -p orchestrator-store --example seed_restore -- \
//!       claude:<cli-session-id>:<cwd> codex:<cli-session-id>:<cwd>
//!
//! Unlike seed_demo it REFUSES to run without `$KOD_STORE_DIR`: the fallback
//! would be the real application database, and a fake crash there would be
//! offered on the user's next launch.
use orchestrator_store::Store;

fn main() {
    let Some(dir) = std::env::var_os("KOD_STORE_DIR").filter(|d| !d.is_empty()) else {
        eprintln!("KOD_STORE_DIR is unset — refusing to seed the real store");
        std::process::exit(2);
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("store.db");
    println!("seeding: {}", path.display());
    let s = Store::open(&path).unwrap();
    // No background summaries: the app under test sees the user's real
    // transcripts, and every summary is a model call.
    s.set_setting("summaries_on", "0").unwrap();
    for arg in std::env::args().skip(1) {
        let mut it = arg.splitn(3, ':');
        let (Some(kind), Some(id), Some(cwd)) = (it.next(), it.next(), it.next()) else {
            eprintln!("expected <kind>:<cli-session-id>:<cwd>, got {arg}");
            std::process::exit(2);
        };
        s.record_session(id, &format!("path:{cwd}"), kind, cwd, None).unwrap();
        println!("  crashed {kind} {id} in {cwd}");
    }
}
