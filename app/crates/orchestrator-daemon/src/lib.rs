//! The session daemon server (docs/018 §5–§9). Owns `Arc<SessionHost>` and the
//! PTYs; serves the GUI over a Unix socket. A client disconnect drops NO
//! session — the daemon retains the host across client churn (the core
//! invariant, §2). Single attached client for minimal; the per-client writer
//! queue is the seam for multi-client later (§17 Q4).

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use orchestrator_host::protocol::{
    read_frame, write_frame, ClientMsg, Command, CommandReply, EventKind, ServerEvent, ServerMsg,
    WIRE_VERSION,
};
use orchestrator_host::session::SessionId;
use orchestrator_host::{SessionBackend, SessionHost};

pub mod bridge;
mod remote;
pub use remote::RemoteHost;

// ---- client side: discover-or-spawn (docs/018 §9, §13) ----

/// How the GUI's session backend is running — surfaced in the UI so the user
/// KNOWS whether sessions survive a restart (dogfooding: a silent in-process
/// fallback is a data-loss trap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMode {
    /// attached to the long-lived daemon — sessions survive GUI restarts.
    Daemon,
    /// in-process by explicit choice (`ORCH_NO_DAEMON`) — expected, not alarming.
    InProcessByChoice,
    /// in-process because the daemon could NOT be reached — sessions will be
    /// LOST on restart; warn loudly.
    InProcessFallback,
}

/// Get a session backend for the GUI: attach to a running daemon, spawn one if
/// absent, or run in-process (`ORCH_NO_DAEMON`, or on any daemon failure).
/// Returns the mode so the GUI can show it (docs/018 §3, §13).
/// What a daemon does when an attaching client's wire version differs from ours.
/// Pure so it's unit-tested directly. The asymmetry is the whole point: a NEWER
/// client means WE are the outdated binary → retire (exit) so it can respawn a
/// fresh same-wire daemon; an OLDER client is the stale one → reject it but keep
/// serving (never kill a good daemon for an old binary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireGate {
    Accept,
    RejectStale,
    RetireOutdated,
}

fn wire_gate(client: u32, daemon: u32) -> WireGate {
    match client.cmp(&daemon) {
        std::cmp::Ordering::Equal => WireGate::Accept,
        std::cmp::Ordering::Greater => WireGate::RetireOutdated,
        std::cmp::Ordering::Less => WireGate::RejectStale,
    }
}

/// The running binary's build identity = its file mtime. A `cargo build` rewrites
/// the executable, so a daemon launched from the PREVIOUS binary sees a newer
/// mtime on disk and self-retires on the next attach — dev no longer needs to
/// `pkill` after a rebuild. A plain GUI restart doesn't change the binary.
fn exe_mtime() -> Option<std::time::SystemTime> {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
}

/// The binary mtime when THIS daemon launched (set in `run_default`). Unset on the
/// `run()`-with-listener test path, which disables the rebuilt-binary check.
static LAUNCH_MTIME: std::sync::OnceLock<Option<std::time::SystemTime>> =
    std::sync::OnceLock::new();

/// True when the on-disk binary is newer than the one this daemon launched from.
fn binary_was_rebuilt() -> bool {
    matches!((LAUNCH_MTIME.get().copied().flatten(), exe_mtime()), (Some(launch), Some(now)) if now > launch)
}

/// What to do with an attaching client: fold the wire-version gate together with
/// "our binary was rebuilt". A rebuilt binary retires exactly like an outdated
/// wire — the attaching GUI then respawns a fresh daemon from the new build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachGate {
    Accept,
    Retire,
    Reject,
}

fn attach_gate(client_wire: u32, daemon_wire: u32, rebuilt: bool) -> AttachGate {
    match wire_gate(client_wire, daemon_wire) {
        WireGate::Accept if rebuilt => AttachGate::Retire,
        WireGate::Accept => AttachGate::Accept,
        WireGate::RetireOutdated => AttachGate::Retire,
        WireGate::RejectStale => AttachGate::Reject,
    }
}

pub fn connect_or_spawn() -> (Arc<dyn SessionBackend>, HostMode) {
    if std::env::var("ORCH_NO_DAEMON").is_ok() {
        eprintln!("[orchestrator] ORCH_NO_DAEMON — running the session host in-process");
        return (SessionHost::new(), HostMode::InProcessByChoice);
    }
    match try_attach() {
        Ok(remote) => {
            eprintln!("[orchestrator] attached to the session daemon");
            (remote, HostMode::Daemon)
        }
        Err(e) => {
            eprintln!("[orchestrator] daemon unavailable ({e}) — running in-process (sessions WON'T survive restart)");
            (SessionHost::new(), HostMode::InProcessFallback)
        }
    }
}

/// Connect, accommodating a wire-mismatched daemon. `Ok(Some)` = attached;
/// `Ok(None)` = no daemon (refused, or it RETIRED because we're newer → socket
/// freed) so the caller should spawn a fresh one; `Err(Unsupported)` = the
/// mismatch PERSISTS (the running daemon is newer → WE're the stale binary) →
/// in-process. ONLY the mismatch path sleeps; a matched daemon returns at once,
/// so steady-state attach cost is unchanged.
fn connect_or_let_retire(path: &Path) -> io::Result<Option<Arc<dyn SessionBackend>>> {
    match RemoteHost::connect(path) {
        Ok(r) => return Ok(Some(r)),
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {} // mismatch — handled below
        Err(_) => return Ok(None),                             // refused / not-found → no daemon
    }
    // A NEWER daemon stays up (we're stale → give up to in-process); an OUTDATED
    // one self-retires → poll until the socket frees, then return None to spawn.
    for _ in 0..20 {
        thread::sleep(Duration::from_millis(100));
        match RemoteHost::connect(path) {
            Ok(r) => return Ok(Some(r)),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {}
            Err(_) => return Ok(None), // retired → socket free → spawn fresh
        }
    }
    Err(io::Error::from(io::ErrorKind::Unsupported)) // persisted → we're the stale one
}

fn try_attach() -> io::Result<Arc<dyn SessionBackend>> {
    let path = default_socket_path();
    // usually a same-wire daemon is already up → attach immediately. On mismatch,
    // connect_or_let_retire waits for an outdated daemon to retire (or gives up to
    // in-process if the daemon is the newer one).
    if let Some(r) = connect_or_let_retire(&path)? {
        return Ok(r);
    }
    // none running (or it retired) → serialize the SPAWN with a lock so two GUIs
    // don't both fork a daemon (connect-first, lock only to guard the spawn race).
    let _lock = LockFile::acquire(&path.with_file_name("daemon.lock"))?;
    if let Some(r) = connect_or_let_retire(&path)? {
        return Ok(r);
    }
    spawn_daemon_process()?;
    for _ in 0..50 {
        thread::sleep(Duration::from_millis(100));
        match RemoteHost::connect(&path) {
            Ok(r) => return Ok(r),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => return Err(e),
            Err(_) => {}
        }
    }
    Err(io::Error::other("daemon did not come up within 5s"))
    // _lock drops here → spawn serialization released.
}

/// An `flock(LOCK_EX)` guard over the spawn lock; releases when the fd closes.
struct LockFile {
    _file: std::fs::File,
}

impl LockFile {
    fn acquire(path: &Path) -> io::Result<LockFile> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(path)?;
        // blocks until we hold the exclusive lock.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(LockFile { _file: file })
    }
}

/// Fork+exec the daemon binary, `setsid`-detached from the GUI's process group
/// so it survives the GUI exiting (docs/018 §9).
fn spawn_daemon_process() -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let bin = daemon_binary()?;
    let mut cmd = std::process::Command::new(bin);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setsid in the child between fork and exec — detaches the daemon
    // into its own session so a killpg of the GUI group never reaches it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?; // detached — do NOT wait
    Ok(())
}

/// The daemon binary: a sibling of the running GUI named `orchestrator-daemon`
/// (works for `cargo run` and a bundled install).
fn daemon_binary() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| io::Error::other("no exe dir"))?;
    let bin = dir.join("orchestrator-daemon");
    if bin.exists() {
        Ok(bin)
    } else {
        Err(io::Error::other(format!(
            "daemon binary not found at {}",
            bin.display()
        )))
    }
}

/// The well-known control socket (docs/018 §4): `$XDG_RUNTIME_DIR` or `$TMPDIR`
/// `/orchestrator/daemon.sock`. Fixed (not per-pid) so discover-or-spawn finds it.
pub fn default_socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("orchestrator");
    let _ = std::fs::create_dir_all(&dir);
    // 0700 — the socket is a local automation surface (spawn/keystroke), §con.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir.join("daemon.sock")
}

/// Bind the default socket (unlinking a stale one) and serve until a `Quit`.
/// Holds the `SessionHost` for the whole process lifetime.
pub fn run_default() -> io::Result<()> {
    // stamp the binary identity at launch so a later rebuild is detectable.
    let _ = LAUNCH_MTIME.set(exe_mtime());
    let path = default_socket_path();
    let _ = std::fs::remove_file(&path); // reclaim a stale socket (we hold the lock)
    let listener = UnixListener::bind(&path)?;
    let host = SessionHost::new();
    // Hand the bridge its feed source. Installed HERE and not in `run()` so the
    // unit-test entry points never grow a network listener as a side effect.
    // Nothing starts listening until a GUI pushes a config with `on = true`.
    bridge::install(Arc::clone(&host));
    // Detached sweep (1s): with no client attached, nothing else observes the
    // sessions — this keeps phase_since stamps TRUE across walk-away gaps
    // (design critique #4) and runs the dirty-gated reconcile/trouble scans.
    // Every 10th tick also reaps exited sessions (10s, not 3: the GUI's
    // clean-exit observer must SEE the dead session on a tick before it's
    // reaped, even if the app was briefly wedged — adversarial review).
    let sweeper = host.clone();
    std::thread::spawn(move || {
        let mut n = 0u32;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            sweeper.reconcile_pending();
            // codex limit self-poll (docs/019): self-throttled to ~10s inside, so
            // riding every 1s tick is free. This is why codex limits surface in
            // DEFAULT daemon mode — the GUI's trait call is a no-op here.
            sweeper.poll_codex_limits();
            // auto-continue-on-limit-reset (docs/019 slice 2): replay a blocked
            // session's held prompt when its window resets — ONLY when the global
            // flag is on (checked inside). Off-by-default; returns immediately when
            // disabled. Runs daemon-side so a walked-away session still resumes.
            sweeper.auto_continue_tick();
            n += 1;
            if n % 10 == 0 {
                sweeper.reap_dead();
            }
        }
    });
    let outcome = run(&listener, host);
    let _ = std::fs::remove_file(&path);
    outcome
}

/// Accept loop — serve each client in its OWN thread (so a second GUI attaching
/// while the first is served doesn't block its handshake into a timeout and then
/// spawn a competing daemon — codex; also lets multiple windows attach, Q4). The
/// host persists across reconnects. An explicit `Quit` exits the process.
pub fn run(listener: &UnixListener, host: Arc<SessionHost>) -> io::Result<()> {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let host = host.clone();
        thread::spawn(move || {
            // a client disconnect returns Ok(false) → just drop this thread (the
            // host keeps the sessions). Quit returns Ok(true) → tear down the
            // whole daemon (process death closes the PTY masters → kills all).
            if let Ok(true) = serve_client(stream, host) {
                std::process::exit(0);
            }
        });
    }
    Ok(())
}

/// Serve one attached client to disconnect (or `Quit`). Returns `Ok(true)` iff
/// the client asked the daemon to quit.
pub fn serve_client(stream: UnixStream, host: Arc<SessionHost>) -> io::Result<bool> {
    let mut reader = stream.try_clone()?;
    let writer_stream = stream;

    // one per-client writer task draining a queue (docs/018 §5): grid/phase/
    // decision events AND replies all flow through here, so ordering is exact
    // and a slow socket never blocks the host.
    let (tx, rx) = mpsc::channel::<ServerMsg>();
    let writer = thread::spawn(move || {
        let mut w = writer_stream;
        for msg in rx {
            if write_frame(&mut w, &msg).is_err() {
                break;
            }
        }
    });

    // handshake: wire-version gate. Bound the Hello read so a client that
    // connects but never speaks can't wedge the (single-client) accept loop
    // (review #6); cleared below so a legit idle client never times out.
    let _ = reader.set_read_timeout(Some(Duration::from_secs(5)));
    let hello: ClientMsg = match read_frame(&mut reader) {
        Ok(m) => m,
        Err(_) => {
            drop(tx);
            let _ = writer.join();
            return Ok(false);
        }
    };
    let client_wire = match &hello {
        ClientMsg::Hello { wire_version } => *wire_version,
        _ => 0, // first frame wasn't a Hello → incompatible; reject + keep serving
    };
    match attach_gate(client_wire, WIRE_VERSION, binary_was_rebuilt()) {
        AttachGate::Accept => {}
        gate => {
            let _ = tx.send(ServerMsg::VersionMismatch {
                daemon_version: WIRE_VERSION,
            });
            drop(tx);
            let _ = writer.join();
            // Retire → a NEWER client appeared OR our binary was rebuilt under us, so
            // we exit (Ok(true) → run() process::exits, freeing the socket for the
            // client to respawn a fresh daemon). Reject → an OLDER client; keep
            // serving the host (Ok(false), just drop this thread).
            return Ok(gate == AttachGate::Retire);
        }
    }

    // snapshot-replay (§7): Welcome carries the session infos; then one Grid per
    // session; then ReplayDone. `seed` also primes the coalescer so it emits only
    // CHANGES after (and a session NOT in the seed is genuinely new — review #3).
    let seq = Arc::new(AtomicU64::new(0));
    let seed = host.infos();
    let _ = tx.send(ServerMsg::Welcome {
        wire_version: WIRE_VERSION,
        infos: seed.clone(),
    });
    for info in &seed {
        if let Some(g) = host.snapshot(info.id) {
            send_event(&tx, &seq, info.id, EventKind::Grid(g));
        }
    }
    let _ = tx.send(ServerMsg::ReplayDone);

    // a legit attached client is idle between commands → block, don't time out.
    let _ = reader.set_read_timeout(None);

    // the 16ms coalescer (§6), seeded so it emits deltas only.
    let running = Arc::new(AtomicBool::new(true));
    let coalescer = {
        let (host, tx, seq, running) = (host.clone(), tx.clone(), seq.clone(), running.clone());
        thread::spawn(move || coalesce_loop(host, tx, seq, running, seed))
    };

    // command-reader (this thread): client → daemon, reply per request.
    let mut quit = false;
    loop {
        let msg: ClientMsg = match read_frame(&mut reader) {
            Ok(m) => m,
            Err(_) => break, // EOF / disconnect
        };
        if let ClientMsg::Request {
            request_id,
            command,
        } = msg
        {
            let is_quit = matches!(command, Command::Quit);
            // isolate a panicking verb so one bad command can't crash the daemon
            // and kill every other session (review #5).
            let reply =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(&host, command)))
                    .unwrap_or_else(|_| CommandReply::Error("command panicked".into()));
            let _ = tx.send(ServerMsg::Reply { request_id, reply });
            if is_quit {
                quit = true;
                break;
            }
        }
    }

    // teardown: stop the coalescer + writer; the HOST stays alive (detach drops
    // no session — §10).
    running.store(false, Ordering::SeqCst);
    let _ = coalescer.join();
    drop(tx);
    let _ = writer.join();
    Ok(quit)
}

/// Per-session last-sent state — the coalescer emits an event only on change.
/// `info` is the whole last-sent SessionInfo so we compare ALL fields (incl. the
/// FULL PendingDecision and alive) — not a lossy summary string (codex: a stale
/// decision body could make the user answer the wrong prompt).
struct Sent {
    dirty: u64,
    /// the client's timeline cursor — the max event seq it has been sent (#9).
    last_event_seq: u64,
    info: orchestrator_host::SessionInfo,
}

/// Two SessionInfos differ in a way the client must see (everything but `dirty`,
/// which drives Grid, not Info).
fn info_changed(a: &orchestrator_host::SessionInfo, b: &orchestrator_host::SessionInfo) -> bool {
    a.phase != b.phase
        || a.title != b.title
        || a.pending != b.pending
        || a.alive != b.alive
        || a.cli_session_id != b.cli_session_id
        || a.last_message != b.last_message
        || a.phase_since_ms != b.phase_since_ms
        || a.trouble != b.trouble
        || a.project_slug != b.project_slug // rebind must propagate (#10)
        || a.usage_limit != b.usage_limit // a limit-only edge on an idle session (#019)
}

fn coalesce_loop(
    host: Arc<SessionHost>,
    tx: mpsc::Sender<ServerMsg>,
    seq: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    seed: Vec<orchestrator_host::SessionInfo>,
) {
    // seed last_event_seq=0 so a freshly-attached client BACKFILLS the whole
    // surviving event ring on its first tick (Welcome carries infos, not events).
    let mut sent: HashMap<SessionId, Sent> = seed
        .into_iter()
        .map(|i| {
            (
                i.id,
                Sent {
                    dirty: i.dirty,
                    last_event_seq: 0,
                    info: i,
                },
            )
        })
        .collect();

    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(16));
        let infos = host.infos();
        let mut present = HashSet::new();
        for info in &infos {
            present.insert(info.id);
            match sent.get_mut(&info.id) {
                None => {
                    // NEW session (spawned after attach, review #3) — send the
                    // full Info so the client materializes it, then its grid, then
                    // backfill its whole event timeline.
                    send_event(&tx, &seq, info.id, EventKind::Info(info.clone()));
                    if let Some(g) = host.snapshot(info.id) {
                        send_event(&tx, &seq, info.id, EventKind::Grid(g));
                    }
                    let events = host.events_since(info.id, 0);
                    let last_event_seq = events.last().map(|e| e.seq).unwrap_or(0);
                    if !events.is_empty() {
                        send_event(&tx, &seq, info.id, EventKind::Events(events));
                    }
                    sent.insert(
                        info.id,
                        Sent {
                            dirty: info.dirty,
                            last_event_seq,
                            info: info.clone(),
                        },
                    );
                }
                Some(prev) => {
                    if info.dirty != prev.dirty {
                        if let Some(g) = host.snapshot(info.id) {
                            send_event(&tx, &seq, info.id, EventKind::Grid(g));
                        }
                        prev.dirty = info.dirty;
                    }
                    // any info-field change → re-send the whole SessionInfo.
                    if info_changed(info, &prev.info) {
                        send_event(&tx, &seq, info.id, EventKind::Info(info.clone()));
                        prev.info = info.clone();
                    }
                    // new timeline events since the client's cursor → send the
                    // delta. Gate on the cheap max-seq check so a quiet session
                    // skips the lock+filter+clone entirely (the common case).
                    if prev.last_event_seq < host.max_event_seq_for(info.id) {
                        let delta = host.events_since(info.id, prev.last_event_seq);
                        if let Some(last) = delta.last() {
                            prev.last_event_seq = last.seq;
                            send_event(&tx, &seq, info.id, EventKind::Events(delta));
                        }
                    }
                }
            }
        }
        // sessions that vanished (closed / reaped+removed) → one Closed event.
        let gone: Vec<SessionId> = sent
            .keys()
            .copied()
            .filter(|id| !present.contains(id))
            .collect();
        for id in gone {
            send_event(&tx, &seq, id, EventKind::Closed);
            sent.remove(&id);
        }
        // reconcile_pending runs daemon-side now (clears stale cards).
        host.reconcile_pending();
    }
}

fn send_event(
    tx: &mpsc::Sender<ServerMsg>,
    seq: &AtomicU64,
    session_id: SessionId,
    kind: EventKind,
) {
    let s = seq.fetch_add(1, Ordering::SeqCst);
    let _ = tx.send(ServerMsg::Event(ServerEvent {
        seq: s,
        session_id,
        kind,
    }));
}

fn dispatch(host: &SessionHost, cmd: Command) -> CommandReply {
    match cmd {
        // The bridge supervisor is a process global installed by `run_default`,
        // so these need nothing from `host` and the signature is unchanged. Both
        // are request/reply on purpose: a bind failure must come back on the same
        // click that caused it, not arrive later as a status the user has to
        // notice.
        Command::SetBridge {
            on,
            port,
            bind,
            token,
        } => CommandReply::Bridge(bridge::apply(on, port, &bind, &token)),
        Command::BridgeStatus => CommandReply::Bridge(bridge::status()),
        Command::SpawnClaude { project_slug, spec } => {
            match host.spawn_claude(project_slug, spec) {
                Ok((id, cli)) => CommandReply::Spawned {
                    id,
                    cli_session_id: Some(cli),
                },
                Err(e) => CommandReply::Error(e.to_string()),
            }
        }
        Command::ResumeClaude {
            project_slug,
            session_id,
            spec,
        } => match host.resume_claude(project_slug, &session_id, spec) {
            Ok(id) => CommandReply::Spawned {
                id,
                cli_session_id: Some(session_id),
            },
            Err(e) => CommandReply::Error(e.to_string()),
        },
        Command::ResumeCodex {
            project_slug,
            session_id,
            spec,
        } => match host.resume_codex(project_slug, &session_id, spec) {
            Ok(id) => CommandReply::Spawned {
                id,
                cli_session_id: Some(session_id),
            },
            Err(e) => CommandReply::Error(e.to_string()),
        },
        Command::Spawn {
            project_slug,
            kind,
            spec,
        } => match host.spawn(project_slug, kind, spec) {
            Ok(id) => CommandReply::Spawned {
                id,
                cli_session_id: None,
            },
            Err(e) => CommandReply::Error(e.to_string()),
        },
        Command::SpawnShell { project_slug, cwd } => match host.spawn_shell(project_slug, cwd) {
            Ok(id) => CommandReply::Spawned {
                id,
                cli_session_id: None,
            },
            Err(e) => CommandReply::Error(e.to_string()),
        },
        Command::SendKey { id, key } => {
            host.send_key(id, &key);
            CommandReply::Ok
        }
        Command::Scroll { id, delta } => {
            host.scroll(id, delta);
            CommandReply::Ok
        }
        Command::ScrollView { id, delta } => {
            host.scroll_view(id, delta);
            CommandReply::Ok
        }
        Command::Rebind { id, project_slug } => {
            host.rebind(id, &project_slug);
            CommandReply::Ok
        }
        Command::Search { id, query } => {
            let matches = host.search(id, &query);
            // host caps at SEARCH_CAP; total == len means "maybe exactly cap" —
            // the emulator doesn't count beyond the cap (cheapness), so total
            // simply mirrors len and the GUI shows "300+" AT the cap.
            let total = matches.len() as u32;
            CommandReply::Matches { matches, total }
        }
        Command::Resize { id, rows, cols } => {
            host.resize(id, rows, cols);
            CommandReply::Ok
        }
        Command::Close { id } => CommandReply::Bool(host.close(id)),
        Command::SetCliId { id, cli_session_id } => {
            host.set_cli_session_id(id, cli_session_id);
            CommandReply::Ok
        }
        Command::BackfillTranscript { id, path } => {
            host.backfill_transcript(id, std::path::Path::new(&path));
            CommandReply::Ok
        }
        Command::SetAutoContinue { on, fire_on_reset } => {
            host.set_auto_continue(on, fire_on_reset);
            CommandReply::Ok
        }
        Command::Quit => CommandReply::Ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator_host::pty::SpawnSpec;
    use orchestrator_host::session::CliKind;

    // RULE ZERO: only cat/printf in PTYs, never claude/codex. Drive the server
    // over an in-process socketpair (UnixStream::pair).
    fn drain_until<F: FnMut(&ServerMsg) -> bool>(
        reader: &mut UnixStream,
        mut done: F,
    ) -> Vec<ServerMsg> {
        reader
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut got = Vec::new();
        loop {
            match read_frame::<_, ServerMsg>(reader) {
                Ok(m) => {
                    let stop = done(&m);
                    got.push(m);
                    if stop {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        got
    }

    #[test]
    fn handshake_replay_spawn_and_grid() {
        let (client, server) = UnixStream::pair().unwrap();
        let host = SessionHost::new();
        // a pre-existing session so the replay has something to send.
        host.spawn(
            "p",
            CliKind::Shell,
            SpawnSpec::program("cat", std::env::temp_dir()),
        )
        .unwrap();
        let h = host.clone();
        let server_thread = thread::spawn(move || serve_client(server, h).unwrap());

        let mut c = client;
        write_frame(
            &mut c,
            &ClientMsg::Hello {
                wire_version: WIRE_VERSION,
            },
        )
        .unwrap();

        // Welcome + replay + ReplayDone.
        let mut rc = c.try_clone().unwrap();
        let replay = drain_until(&mut rc, |m| matches!(m, ServerMsg::ReplayDone));
        assert!(
            matches!(replay.first(), Some(ServerMsg::Welcome { infos, .. }) if infos.len() == 1)
        );
        assert!(replay
            .iter()
            .any(|m| matches!(m, ServerMsg::Event(e) if matches!(e.kind, EventKind::Grid(_)))));

        // a command → a correlated reply.
        write_frame(
            &mut c,
            &ClientMsg::Request {
                request_id: 42,
                command: Command::SpawnShell {
                    project_slug: "p".into(),
                    cwd: std::env::temp_dir(),
                },
            },
        )
        .unwrap();
        let got = drain_until(&mut rc, |m| {
            matches!(m, ServerMsg::Reply { request_id: 42, .. })
        });
        assert!(got.iter().any(|m| matches!(
            m,
            ServerMsg::Reply {
                request_id: 42,
                reply: CommandReply::Spawned { .. }
            }
        )));
        assert_eq!(host.infos().len(), 2);

        // REGRESSION (review #3): a session spawned AFTER attach must reach the
        // client as an Info event — else it would be invisible in the GUI.
        let new_id = host.infos().iter().map(|i| i.id).max().unwrap();
        let saw_info = drain_until(
            &mut rc,
            |m| matches!(m, ServerMsg::Event(e) if e.session_id == new_id && matches!(e.kind, EventKind::Info(_))),
        );
        assert!(
            saw_info.iter().any(|m| matches!(m, ServerMsg::Event(e) if matches!(&e.kind, EventKind::Info(i) if i.id == new_id))),
            "spawned-after-attach session never reached the client (review #3)"
        );

        // a key into the first session → a Grid event eventually (coalesced).
        let first = host.infos()[0].id;
        write_frame(
            &mut c,
            &ClientMsg::Request {
                request_id: 7,
                command: Command::SendKey {
                    id: first,
                    key: orchestrator_host::input::KeyInput::Char("hi-daemon\n".into()),
                },
            },
        )
        .unwrap();
        let saw_grid = drain_until(
            &mut rc,
            |m| matches!(m, ServerMsg::Event(e) if e.session_id == first && matches!(e.kind, EventKind::Grid(_))),
        );
        assert!(
            saw_grid
                .iter()
                .any(|m| matches!(m, ServerMsg::Event(e) if matches!(e.kind, EventKind::Grid(_)))),
            "no coalesced grid after a keystroke"
        );

        // disconnect must NOT kill the host's sessions (the core invariant).
        // BOTH client fds must close or the server never sees EOF (rc is a dup).
        drop(rc);
        drop(c);
        let _ = server_thread.join();
        assert_eq!(
            host.infos().len(),
            2,
            "client disconnect killed sessions — detach must drop nothing"
        );
    }

    #[test]
    fn wire_gate_retires_on_newer_rejects_on_older() {
        assert_eq!(wire_gate(7, 7), WireGate::Accept);
        assert_eq!(wire_gate(8, 7), WireGate::RetireOutdated); // client newer → we're outdated
        assert_eq!(wire_gate(6, 7), WireGate::RejectStale); // client older → reject, survive
    }

    #[test]
    fn attach_gate_retires_on_rebuilt_binary() {
        // same wire, binary NOT rebuilt → attach (a plain GUI restart keeps sessions).
        assert_eq!(attach_gate(7, 7, false), AttachGate::Accept);
        // same wire but our binary was rebuilt under us → retire (dev: no pkill needed).
        assert_eq!(attach_gate(7, 7, true), AttachGate::Retire);
        // a newer client still retires regardless of the binary check.
        assert_eq!(attach_gate(8, 7, false), AttachGate::Retire);
        // an older client is rejected — never kill a good daemon for a stale binary.
        assert_eq!(attach_gate(6, 7, false), AttachGate::Reject);
        assert_eq!(attach_gate(6, 7, true), AttachGate::Reject);
    }

    #[test]
    fn newer_client_retires_the_daemon() {
        // a NEWER GUI attaching means we're the outdated binary → reply mismatch
        // AND signal shutdown (Ok(true)) so it can respawn a fresh same-wire daemon.
        let (client, server) = UnixStream::pair().unwrap();
        let host = SessionHost::new();
        let st = thread::spawn(move || serve_client(server, host).unwrap());
        let mut c = client;
        write_frame(
            &mut c,
            &ClientMsg::Hello {
                wire_version: WIRE_VERSION + 1,
            },
        )
        .unwrap();
        let mut rc = c.try_clone().unwrap();
        let got = drain_until(&mut rc, |m| matches!(m, ServerMsg::VersionMismatch { .. }));
        assert!(
            matches!(got.first(), Some(ServerMsg::VersionMismatch { daemon_version }) if *daemon_version == WIRE_VERSION)
        );
        drop(c);
        assert!(st.join().unwrap(), "a newer client must retire the daemon");
    }

    #[test]
    fn stale_client_is_rejected_but_daemon_survives() {
        // an OLDER GUI must be rejected WITHOUT killing the (newer) daemon.
        let (client, server) = UnixStream::pair().unwrap();
        let host = SessionHost::new();
        let st = thread::spawn(move || serve_client(server, host).unwrap());
        let mut c = client;
        write_frame(
            &mut c,
            &ClientMsg::Hello {
                wire_version: WIRE_VERSION - 1,
            },
        )
        .unwrap();
        let mut rc = c.try_clone().unwrap();
        let got = drain_until(&mut rc, |m| matches!(m, ServerMsg::VersionMismatch { .. }));
        assert!(matches!(
            got.first(),
            Some(ServerMsg::VersionMismatch { .. })
        ));
        drop(c);
        assert!(
            !st.join().unwrap(),
            "a stale client must NOT retire the daemon"
        );
    }

    #[test]
    fn quit_requests_shutdown() {
        let (client, server) = UnixStream::pair().unwrap();
        let host = SessionHost::new();
        let st = thread::spawn(move || serve_client(server, host).unwrap());
        let mut c = client;
        write_frame(
            &mut c,
            &ClientMsg::Hello {
                wire_version: WIRE_VERSION,
            },
        )
        .unwrap();
        write_frame(
            &mut c,
            &ClientMsg::Request {
                request_id: 1,
                command: Command::Quit,
            },
        )
        .unwrap();
        let quit = st.join().unwrap();
        assert!(quit, "Quit did not signal shutdown");
    }
}
