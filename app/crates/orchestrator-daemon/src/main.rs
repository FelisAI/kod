//! The session daemon binary (docs/018). The GUI fork+execs this; it owns the
//! PTYs and serves over the well-known socket until an explicit `Quit`.

fn main() -> std::io::Result<()> {
    // Before any thread. Forked by a GUI that already adopted the user's
    // environment, this is a no-op (the marker is inherited); started any other
    // way, the sessions this daemon hosts still get the user's PATH.
    orchestrator_host::login_env::adopt();
    orchestrator_daemon::run_default()
}
