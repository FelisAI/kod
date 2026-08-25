//! `RemoteHost` — the GUI-side daemon client (docs/018 §3, §6). Implements the
//! same `SessionBackend` the in-process `SessionHost` does, so render code is
//! unchanged. CRUCIAL (codex top-risk): reads are served from a LOCAL CACHE fed
//! by the daemon's event stream — never a blocking RPC — so a frame never stalls
//! on the socket. Only commands are RPC.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use orchestrator_host::decision::PendingDecision;
use orchestrator_host::emulator::GridSnapshot;
use orchestrator_host::input::KeyInput;
use orchestrator_host::protocol::{ClientRole, 
    read_frame, write_frame, BridgeStatus, ClientMsg, Command, CommandReply, EventKind,
    ServerEvent, ServerMsg, WIRE_VERSION,
};
use orchestrator_host::pty::SpawnSpec;
use orchestrator_host::session::{CliKind, SessionId};
use orchestrator_host::{SessionBackend, SessionEvent, SessionInfo};

/// The local mirror of daemon state, updated by the reader thread.
#[derive(Default)]
struct Cache {
    infos: BTreeMap<SessionId, SessionInfo>,
    grids: HashMap<SessionId, GridSnapshot>,
    /// per-session timeline, appended from Events deltas (#9), bounded.
    events: HashMap<SessionId, Vec<SessionEvent>>,
    /// monotonic repaint counter — stamped onto a session's `dirty` on every
    /// event so `drive_repaints` always notices (review #11).
    last_repaint: u64,
}

pub struct RemoteHost {
    /// write half — Hello + Requests (commands). Reads go through the cache.
    writer: Mutex<UnixStream>,
    cache: Arc<Mutex<Cache>>,
    next_id: AtomicU64,
    /// request_id → slot. A waiter inserts `None` before sending; the reader
    /// fills `Some(reply)` and notifies. Fire-and-forget commands skip this, so
    /// their replies are simply dropped by the reader.
    pending: Arc<(Mutex<HashMap<u64, Option<CommandReply>>>, Condvar)>,
    /// set true when the reader thread exits (daemon gone) — so an in-flight or
    /// future `request()` fails FAST instead of waiting 20s (review #9).
    dead: Arc<std::sync::atomic::AtomicBool>,
}

impl RemoteHost {
    /// Connect to a running daemon, do the Hello/Welcome handshake, drain the
    /// snapshot replay into the cache, and start the reader thread. Returns
    /// `ErrorKind::Unsupported` on a wire-version mismatch (docs/018 §13).
    pub fn connect(path: &std::path::Path) -> io::Result<Arc<RemoteHost>> {
        let stream = UnixStream::connect(path)?;
        let mut reader = stream.try_clone()?;
        let mut wh = stream.try_clone()?;
        write_frame(
            &mut wh,
            &ClientMsg::Hello {
                wire_version: WIRE_VERSION,
                // The desktop app. It spawns, closes and types into shells, so it
                // holds every capability — and it reaches the daemon over a unix
                // socket in a 0700 directory, not over a network.
                role: ClientRole::Full,
            },
        )?;

        let cache = Arc::new(Mutex::new(Cache::default()));

        // bound the handshake reads so a listening-but-wedged daemon can't hang
        // GUI startup forever (review #10); cleared before the live reader thread.
        let _ = reader.set_read_timeout(Some(Duration::from_secs(5)));

        // handshake: Welcome (or VersionMismatch), then replay until ReplayDone.
        loop {
            match read_frame::<_, ServerMsg>(&mut reader)? {
                ServerMsg::Welcome { infos, .. } => {
                    let mut c = cache.lock().unwrap();
                    for i in infos {
                        c.infos.insert(i.id, i);
                    }
                }
                ServerMsg::VersionMismatch { daemon_version } => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("daemon wire {daemon_version} != {WIRE_VERSION}"),
                    ));
                }
                ServerMsg::Event(ev) => apply_event(&cache, ev),
                ServerMsg::ReplayDone => break,
                ServerMsg::Reply { .. } => {} // none expected pre-ReplayDone
            }
        }

        // live events block indefinitely — clear the handshake timeout.
        let _ = reader.set_read_timeout(None);

        let dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let host = Arc::new(RemoteHost {
            writer: Mutex::new(stream),
            cache: cache.clone(),
            next_id: AtomicU64::new(1),
            pending: Arc::new((Mutex::new(HashMap::new()), Condvar::new())),
            dead: dead.clone(),
        });

        // reader thread: apply live events to the cache, route replies to waiters.
        let pending = host.pending.clone();
        std::thread::Builder::new()
            .name("daemon-reader".into())
            .spawn(move || {
                loop {
                    match read_frame::<_, ServerMsg>(&mut reader) {
                        Ok(ServerMsg::Event(ev)) => apply_event(&cache, ev),
                        Ok(ServerMsg::Reply { request_id, reply }) => {
                            let (lock, cv) = &*pending;
                            let mut g = lock.lock().unwrap();
                            if let Some(slot) = g.get_mut(&request_id) {
                                *slot = Some(reply);
                                cv.notify_all();
                            } // else: a fire-and-forget reply — drop it.
                        }
                        Ok(_) => {} // stray Welcome/VersionMismatch/ReplayDone post-handshake
                        Err(_) => break, // daemon gone / socket closed
                    }
                }
                // daemon gone: wake EVERY waiter with an error so no request()
                // blocks for the 20s timeout (review #9). Set `dead` and drain
                // UNDER the pending lock so a concurrent request() either
                // registered before (gets drained here) or sees `dead` and fails
                // fast — no request can slip through to a 20s wait (codex).
                let (lock, cv) = &*pending;
                let mut g = lock.lock().unwrap();
                dead.store(true, std::sync::atomic::Ordering::SeqCst);
                for slot in g.values_mut() {
                    if slot.is_none() {
                        *slot = Some(CommandReply::Error("daemon connection lost".into()));
                    }
                }
                cv.notify_all();
            })
            .expect("spawn daemon-reader");

        Ok(host)
    }

    /// Send a command and BLOCK for its reply (the RPC path — spawn/resume/
    /// close, whose results the GUI uses immediately).
    fn request(&self, command: Command) -> CommandReply {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (lock, cv) = &*self.pending;
        {
            // check `dead` AND register the slot under the SAME lock the reader's
            // death-drain holds — so a request can't slip in after the drain and
            // then block 20s (codex). Either we register first (reader fills it)
            // or we see `dead` and fail fast.
            let mut g = lock.lock().unwrap();
            if self.dead.load(Ordering::SeqCst) {
                return CommandReply::Error("daemon connection lost".into());
            }
            g.insert(id, None);
        }
        if let Ok(mut w) = self.writer.lock() {
            if write_frame(
                &mut *w,
                &ClientMsg::Request {
                    request_id: id,
                    command,
                },
            )
            .is_err()
            {
                lock.lock().unwrap().remove(&id);
                return CommandReply::Error("daemon write failed".into());
            }
        }
        // wait for the reader to fill the slot (bounded — never hang the UI).
        let mut g = lock.lock().unwrap();
        loop {
            if let Some(Some(reply)) = g.get(&id).cloned() {
                g.remove(&id);
                return reply;
            }
            let (ng, timed) = cv.wait_timeout(g, Duration::from_secs(20)).unwrap();
            g = ng;
            if timed.timed_out() {
                g.remove(&id);
                return CommandReply::Error("daemon reply timeout".into());
            }
        }
    }

    /// Fire-and-forget a command (send_key/resize — no result the GUI needs).
    fn fire(&self, command: Command) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut w) = self.writer.lock() {
            let _ = write_frame(
                &mut *w,
                &ClientMsg::Request {
                    request_id: id,
                    command,
                },
            );
        }
    }

    fn spawned(reply: CommandReply) -> io::Result<(SessionId, String)> {
        match reply {
            CommandReply::Spawned { id, cli_session_id } => {
                Ok((id, cli_session_id.unwrap_or_default()))
            }
            CommandReply::Error(e) => Err(io::Error::other(e)),
            _ => Err(io::Error::other("unexpected daemon reply")),
        }
    }

    /// Fold any bridge reply into a status. There is no `io::Result` here on
    /// purpose: the pane always has a line to draw, so a failure has to arrive AS
    /// a status. `request` already synthesizes `Error` for a lost daemon, a failed
    /// write and a 20s timeout, so every one of those lands in `unavailable` with
    /// its real text — the pane never shows a stale "running" for a daemon that
    /// went away.
    fn bridge(reply: CommandReply) -> BridgeStatus {
        match reply {
            CommandReply::Bridge(s) => s,
            CommandReply::Error(e) => BridgeStatus::unavailable(&e),
            // Ok/Bool/Spawned/Matches to a bridge command means the daemon and the
            // GUI disagree about the protocol — say so, don't invent a state.
            _ => BridgeStatus::unavailable("daemon answered a bridge command with something else"),
        }
    }
}

fn apply_event(cache: &Arc<Mutex<Cache>>, ev: ServerEvent) {
    let mut c = cache.lock().unwrap();
    let id = ev.session_id;
    match ev.kind {
        // upsert the full SessionInfo — materializes a session spawned after
        // attach (review #3/#8) and keeps phase/pending/title/alive fresh (#11).
        EventKind::Info(info) => {
            c.infos.insert(id, info);
            bump_dirty(&mut c, id);
        }
        EventKind::Grid(g) => {
            c.grids.insert(id, g);
            bump_dirty(&mut c, id);
        }
        // append the timeline delta + MUST bump_dirty — else a tool-event-only
        // tick (no grid/phase change) never folds into drive_repaints' signature
        // and the row silently never appears (the review-#11 trap).
        EventKind::Events(delta) => {
            let log = c.events.entry(id).or_default();
            log.extend(delta);
            // keep the client ring bounded to match the host cap.
            let overflow = log.len().saturating_sub(256);
            if overflow > 0 {
                log.drain(0..overflow);
            }
            bump_dirty(&mut c, id);
        }
        EventKind::Closed => {
            c.infos.remove(&id);
            c.grids.remove(&id);
            c.events.remove(&id);
        }
    }
}

/// Bump the cached session's `dirty` to a local MONOTONIC repaint counter so the
/// existing 16ms `drive_repaints` poll (which folds `infos().dirty`) notices
/// EVERY event (even a phase-only change at the same daemon dirty) and repaints —
/// no GUI-thread call needed.
fn bump_dirty(c: &mut Cache, id: SessionId) {
    let next = c.last_repaint.wrapping_add(1);
    c.last_repaint = next;
    if let Some(i) = c.infos.get_mut(&id) {
        i.dirty = next;
    }
}

impl SessionBackend for RemoteHost {
    fn infos(&self) -> Vec<SessionInfo> {
        self.cache.lock().unwrap().infos.values().cloned().collect()
    }
    fn infos_for(&self, project_slug: &str) -> Vec<SessionInfo> {
        self.cache
            .lock()
            .unwrap()
            .infos
            .values()
            .filter(|i| i.project_slug == project_slug)
            .cloned()
            .collect()
    }
    fn snapshot(&self, id: SessionId) -> Option<GridSnapshot> {
        self.cache.lock().unwrap().grids.get(&id).cloned()
    }
    fn pending(&self, id: SessionId) -> Option<PendingDecision> {
        self.cache
            .lock()
            .unwrap()
            .infos
            .get(&id)
            .and_then(|i| i.pending.clone())
    }
    fn events_for(&self, id: SessionId) -> Vec<SessionEvent> {
        self.cache
            .lock()
            .unwrap()
            .events
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }
    fn reconcile_pending(&self) {
        // the daemon runs reconcile_pending on its coalescer tick (docs/018 §6).
    }
    fn spawn_claude(
        &self,
        project_slug: String,
        spec: SpawnSpec,
    ) -> io::Result<(SessionId, String)> {
        Self::spawned(self.request(Command::SpawnClaude { project_slug, spec }))
    }
    fn resume_claude(
        &self,
        project_slug: String,
        session_id: &str,
        spec: SpawnSpec,
    ) -> io::Result<SessionId> {
        Self::spawned(self.request(Command::ResumeClaude {
            project_slug,
            session_id: session_id.into(),
            spec,
        }))
        .map(|(id, _)| id)
    }
    fn resume_codex(
        &self,
        project_slug: String,
        session_id: &str,
        spec: SpawnSpec,
    ) -> io::Result<SessionId> {
        Self::spawned(self.request(Command::ResumeCodex {
            project_slug,
            session_id: session_id.into(),
            spec,
        }))
        .map(|(id, _)| id)
    }
    fn spawn(&self, project_slug: String, kind: CliKind, spec: SpawnSpec) -> io::Result<SessionId> {
        Self::spawned(self.request(Command::Spawn {
            project_slug,
            kind,
            spec,
        }))
        .map(|(id, _)| id)
    }
    fn spawn_shell(&self, project_slug: String, cwd: std::path::PathBuf) -> io::Result<SessionId> {
        Self::spawned(self.request(Command::SpawnShell { project_slug, cwd })).map(|(id, _)| id)
    }
    fn send_key(&self, id: SessionId, key: &KeyInput) {
        self.fire(Command::SendKey {
            id,
            key: key.clone(),
        });
    }
    fn scroll(&self, id: SessionId, delta: i32) {
        self.fire(Command::Scroll { id, delta });
    }
    fn scroll_view(&self, id: SessionId, delta: i32) {
        self.fire(Command::ScrollView { id, delta });
    }
    fn rebind(&self, id: SessionId, project_slug: &str) {
        self.fire(Command::Rebind {
            id,
            project_slug: project_slug.into(),
        });
    }
    fn search(&self, id: SessionId, query: &str) -> Vec<orchestrator_host::emulator::SearchMatch> {
        match self.request(Command::Search {
            id,
            query: query.into(),
        }) {
            CommandReply::Matches { matches, .. } => matches,
            _ => Vec::new(),
        }
    }
    fn resize(&self, id: SessionId, rows: u16, cols: u16) {
        self.fire(Command::Resize { id, rows, cols });
    }
    fn close(&self, id: SessionId) -> bool {
        matches!(
            self.request(Command::Close { id }),
            CommandReply::Bool(true)
        )
    }
    fn set_cli_session_id(&self, id: SessionId, cli: String) {
        // fire-and-forget: the daemon updates the session; the resulting Info
        // event refreshes the cache (docs/018 §12).
        self.fire(Command::SetCliId {
            id,
            cli_session_id: cli,
        });
    }
    fn backfill_transcript(&self, id: SessionId, path: &std::path::Path) {
        // fire-and-forget: the daemon parses the transcript + pushes events, which
        // stream back as an Events delta (#9 §4).
        self.fire(Command::BackfillTranscript {
            id,
            path: path.to_string_lossy().into_owned(),
        });
    }
    fn set_auto_continue(&self, on: bool, fire_on_reset: bool) {
        // fire-and-forget: the daemon caches the flag in its SessionHost (docs/019
        // auto-continue). Mirrors set_cli_session_id — no reply is needed.
        self.fire(Command::SetAutoContinue { on, fire_on_reset });
    }
    fn set_bridge(&self, on: bool, port: u16, bind: &str, token: &str) -> BridgeStatus {
        // BLOCKING request(), NOT fire(): turning the bridge on can fail out in the
        // daemon (port already bound, bind spec rejected) and the user must see
        // that on the click that caused it — a fire-and-forget push would leave the
        // toggle looking green while nothing was listening. The daemon is
        // storage-free, so the GUI is the source of truth and re-pushes on attach.
        Self::bridge(self.request(Command::SetBridge {
            on,
            port,
            bind: bind.into(),
            token: token.into(),
        }))
    }
    fn bridge_status(&self) -> BridgeStatus {
        // also request(): unlike infos/grids there is no cached bridge state fed by
        // the event stream, so the only honest answer comes from asking. Called by
        // the settings pane on open/poll, never per frame.
        Self::bridge(self.request(Command::BridgeStatus))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon that died, timed out, or answered nonsense must NOT leave the
    /// settings pane drawing the last "running" it saw — `request` reports all
    /// three as `Error`, so each has to come back as an unavailable status
    /// carrying the real reason.
    #[test]
    fn non_bridge_replies_fold_to_unavailable() {
        let lost = RemoteHost::bridge(CommandReply::Error("daemon connection lost".into()));
        assert!(!lost.running);
        // NOT configured: an unreachable daemon is "waiting for Kod", never the
        // user-chosen Off (protocol.rs `BridgeStatus::configured`).
        assert!(!lost.configured);
        assert_eq!(lost.error.as_deref(), Some("daemon connection lost"));

        for reply in [
            CommandReply::Ok,
            CommandReply::Bool(true),
            CommandReply::Spawned {
                id: SessionId(1),
                cli_session_id: None,
            },
            CommandReply::Matches {
                matches: vec![],
                total: 0,
            },
        ] {
            let s = RemoteHost::bridge(reply);
            assert!(!s.running && !s.configured);
            assert!(
                s.error.is_some(),
                "an error-less unavailable renders as a healthy Off"
            );
        }
    }

    /// The daemon's own answer passes through untouched — including the pair the
    /// pane branches on (`running: false` + `configured: true` is Off).
    #[test]
    fn a_bridge_reply_passes_through_unchanged() {
        let s = RemoteHost::bridge(CommandReply::Bridge(BridgeStatus {
            running: false,
            endpoints: vec!["ws://10.0.0.2:8765".into()],
            clients: 0,
            error: None,
            configured: true,
            since_ms: 99,
        }));
        assert!(!s.running);
        assert!(s.configured);
        assert_eq!(s.endpoints, vec!["ws://10.0.0.2:8765"]);
        assert_eq!(s.since_ms, 99);
        assert!(s.error.is_none());
    }
}
