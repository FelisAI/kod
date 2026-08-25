//  SessionStore.swift — the phone's cache of what the bridge said.
//
//  PURE ON PURPOSE: `apply` is a function of (state, message) with no clock, no
//  socket and no view. Every rule below — the epoch flush, the rev guard — is
//  unit-tested with nothing connected. The alternative (testing the cache by
//  pointing it at a live bridge) is exactly the thing that must not be needed.

import Foundation

struct SessionStore: Equatable {
    /// The bridge attach this cache belongs to. Nil until the first message.
    private(set) var epoch: String?
    private(set) var sessions: [UInt64: Session] = [:]
    /// Highest rev applied per sid, within `epoch`. Cleared whenever the cache is.
    private(set) var revs: [UInt64: UInt64] = [:]
    /// True once the one full snapshot has landed — before that, "no sessions" is
    /// ignorance, not emptiness, and the views say so.
    private(set) var hasSnapshot = false

    /// Every session the bridge currently knows, in a stable order (sid, which is
    /// monotonic at the daemon). NEVER recency: rows that reorder under the thumb
    /// are the single worst thing a status app can do.
    var all: [Session] { sessions.values.sorted { $0.sid < $1.sid } }

    subscript(sid: UInt64) -> Session? { sessions[sid] }

    mutating func apply(_ msg: ServerMessage) {
        switch msg {
        case .helloOk(_, let epoch, _, _):
            adopt(epoch)
        case .sessions(let epoch, let list):
            adopt(epoch)
            // ONE full snapshot: replace, never merge. A merge would keep ghosts
            // from the previous attach alive forever.
            sessions = Dictionary(list.map { ($0.sid, $0) }, uniquingKeysWith: { _, b in b })
            revs.removeAll()
            hasSnapshot = true
        case .session(let epoch, let rev, let s):
            adopt(epoch)
            // Upsert iff rev > the rev we hold. Frames can be reordered or replayed;
            // a stale one must not overwrite a newer phase.
            if let held = revs[s.sid], rev <= held { return }
            revs[s.sid] = rev
            sessions[s.sid] = s
        case .gone(let epoch, let sid):
            adopt(epoch)
            sessions.removeValue(forKey: sid)
            revs.removeValue(forKey: sid)
        case .helloErr, .pong, .err, .ignored:
            break
        }
    }

    /// The epoch rule, in one place: a new epoch means a different bridge attach,
    /// so everything keyed on the old one is fiction and gets dropped.
    private mutating func adopt(_ incoming: String) {
        guard epoch != incoming else { return }
        epoch = incoming
        sessions.removeAll()
        revs.removeAll()
        hasSnapshot = false
    }

    /// Local reset for a dropped connection — the next attach mints a new epoch
    /// anyway, and showing a frozen list as if it were live is a lie.
    mutating func flush() {
        epoch = nil
        sessions.removeAll()
        revs.removeAll()
        hasSnapshot = false
    }
}
