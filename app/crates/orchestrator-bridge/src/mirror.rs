//! The bridge's view of daemon state.
//!
//! Pure: `apply` is a function of (state, message) with no IO, no clock and no
//! socket, so every update rule is unit-tested with nothing spawned and nothing
//! connected. That matters more here than usual — the alternative is testing a
//! state machine by attaching it to a live daemon, and a wrong attach retires
//! the daemon.

use std::collections::HashMap;

use orchestrator_host::emulator::GridSnapshot;
use orchestrator_host::host::SessionInfo;
use orchestrator_host::protocol::{EventKind, ServerMsg};
use orchestrator_host::session::SessionId;

/// What changed, so a caller can push only what moved instead of re-sending the
/// world. The phone-facing protocol is built on these, not on raw daemon events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The session list was replaced wholesale (attach, or reattach).
    Reset,
    /// This session's metadata moved — phase, title, pending decision, usage.
    Info(SessionId),
    /// A new frame is available for this session. Deliberately carries no
    /// payload: the newest grid is in the mirror, and a caller that has fallen
    /// behind wants the CURRENT frame, never a queue of stale ones.
    Grid(SessionId),
    /// Timeline events arrived (Standup's feed).
    Events(SessionId),
    Closed(SessionId),
    /// The attach snapshot is complete; everything after this is live.
    ReplayDone,
}

#[derive(Default)]
pub struct Mirror {
    pub sessions: HashMap<SessionId, SessionInfo>,
    pub grids: HashMap<SessionId, GridSnapshot>,
    /// True once the daemon has finished replaying the attach snapshot.
    pub replay_done: bool,
}

impl Mirror {
    /// Fold one daemon message in, and say what a client would need told.
    pub fn apply(&mut self, msg: &ServerMsg) -> Option<Change> {
        match msg {
            ServerMsg::Welcome { infos, .. } => {
                // A reattach must not leave ghosts from the previous connection:
                // the daemon's Welcome is the whole truth, so replace rather
                // than merge. Grids are dropped too — they are about to be
                // resent, and a stale frame shown as live is worse than none.
                self.sessions = infos.iter().map(|i| (i.id, i.clone())).collect();
                self.grids.clear();
                self.replay_done = false;
                Some(Change::Reset)
            }
            ServerMsg::ReplayDone => {
                self.replay_done = true;
                Some(Change::ReplayDone)
            }
            ServerMsg::Event(ev) => match &ev.kind {
                EventKind::Info(info) => {
                    self.sessions.insert(info.id, info.clone());
                    Some(Change::Info(info.id))
                }
                EventKind::Grid(grid) => {
                    // LATEST WINS. The daemon sends a whole viewport per tick,
                    // so keeping anything but the newest is keeping garbage —
                    // this is the whole backpressure story in one line.
                    self.grids.insert(ev.session_id, grid.clone());
                    Some(Change::Grid(ev.session_id))
                }
                EventKind::Events(_) => Some(Change::Events(ev.session_id)),
                EventKind::Closed => {
                    self.sessions.remove(&ev.session_id);
                    self.grids.remove(&ev.session_id);
                    Some(Change::Closed(ev.session_id))
                }
            },
            // A reply belongs to whoever asked; the mirror holds no request state.
            ServerMsg::Reply { .. } => None,
            ServerMsg::VersionMismatch { .. } => None,
        }
    }

    /// Sessions a triage client should see: alive, newest-trouble first is the
    /// caller's job — this only filters the dead.
    pub fn live(&self) -> Vec<&SessionInfo> {
        self.sessions.values().filter(|s| s.alive).collect()
    }
}
