//! The session daemon binary (docs/018). The GUI fork+execs this; it owns the
//! PTYs and serves over the well-known socket until an explicit `Quit`.

fn main() -> std::io::Result<()> {
    orchestrator_daemon::run_default()
}
