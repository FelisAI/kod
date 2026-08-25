//  AppModel.swift — the one object the views read.
//
//  It owns three things and nothing else: the cache (`SessionStore`), the link
//  (`BridgeClient`), and what the user has selected. All the ordering logic lives
//  in Plan.swift, all the JSON in Wire.swift; this is the wiring between them.

import Foundation
import Observation

enum RootTab: Hashable {
    case standup, projects, session
}

@MainActor
@Observable
final class AppModel {
    private(set) var store = SessionStore()
    private(set) var connection: ConnectionState = .unconfigured
    /// v0 is read-only, and the Session tab says so out loud. Held anyway so the
    /// day the bridge flips it, the UI has the fact rather than a hardcoded false.
    private(set) var inputAllowed = false

    var tab: RootTab = .standup
    var selectedSid: UInt64?
    var showConnectionSheet = false

    private(set) var settings = BridgeSettings.empty

    /// Server-clock now, in ms. Every age on screen is measured against this and
    /// it ticks on a timer, which is what makes "12m" become "13m" without a frame
    /// arriving. Raw `Date()` would not: nothing would tell SwiftUI to re-render.
    private(set) var now: UInt64 = AppModel.localNowMs()
    /// serverTime - localTime at hello, so a phone clock minutes off the Mac does
    /// not render every session as having waited since the Cretaceous.
    private var clockOffsetMs: Int64 = 0

    private let client = BridgeClient()
    private var clock: Task<Void, Never>?
    #if DEBUG
    /// Fixture-backed: no socket, no ticking clock. Set only by the demo hooks.
    fileprivate var demoMode = false
    #endif

    init(settings: BridgeSettings = SettingsStore.load(), autostart: Bool = true) {
        self.settings = settings
        client.onState = { [weak self] state in self?.connection = state }
        client.onMessage = { [weak self] msg in self?.ingest(msg) }
        client.onDisconnect = { [weak self] in
            // Frozen rows shown as live are a lie; the next attach mints a new
            // epoch and resends everything anyway.
            self?.store.flush()
            self?.inputAllowed = false
        }
        if autostart { start() }
        #if DEBUG
        seedDemoIfRequested()
        #endif
    }

    #if DEBUG
    /// `-kod-demo` (and optionally `-kod-tab standup|projects|session`) fills the
    /// app with fixture sessions and dials nothing. It exists so the design can be
    /// looked at in a simulator without a bridge — and so looking at it can never
    /// involve pointing a client at the real daemon.
    private func seedDemoIfRequested() {
        let args = CommandLine.arguments
        guard args.contains("-kod-demo") else { return }
        demoMode = true
        stop()
        let fixtures = args.contains("-kod-quiet") ? Fixtures.allQuiet : Fixtures.everyTier
        store.apply(.sessions(epoch: "demo", sessions: fixtures))
        connection = .connected
        now = Fixtures.now + 60_000
        selectedSid = 2
        if let i = args.firstIndex(of: "-kod-tab"), i + 1 < args.count {
            switch args[i + 1] {
            case "projects": tab = .projects
            case "session": tab = .session
            default: tab = .standup
            }
        }
    }
    #endif

    // MARK: - Derived views of state

    var sessions: [Session] { store.all }
    var standup: StandupPlan { StandupPlan(sessions: sessions) }
    var projects: ProjectsPlan { ProjectsPlan(sessions: sessions) }
    var selected: Session? { selectedSid.flatMap { store[$0] } }
    /// Sessions worth offering in the Session tab's picker.
    var pickable: [Session] {
        sessions.filter { $0.alive && $0.phase != .dead }.sorted { $0.sid < $1.sid }
    }
    var attentionCount: Int { standup.attentionCount }
    var hasEverSynced: Bool { store.hasSnapshot }

    func age(since ts: UInt64) -> UInt64 { TimeFmt.age(since: ts, now: now) }

    // MARK: - Intent

    /// The one way a session becomes the subject of the Session tab.
    func open(_ session: Session) {
        selectedSid = session.sid
        tab = .session
    }

    func apply(settings new: BridgeSettings) {
        settings = new
        SettingsStore.save(new)
        start()
    }

    func start() {
        #if DEBUG
        // A demo model dials nothing and freezes its clock; otherwise the
        // foreground restart would stomp the fixture `now` with wall-clock time
        // and every age would read in days.
        if demoMode { return }
        #endif
        startClock()
        guard settings.isUsable else {
            connection = .unconfigured
            return
        }
        client.startIfNeeded(settings)
    }

    func stop() {
        client.stop()
        clock?.cancel()
        clock = nil
    }

    func retry() { client.retry() }

    // MARK: - Plumbing

    private func ingest(_ msg: ServerMessage) {
        if case .helloOk(_, _, let serverTime, let input) = msg {
            inputAllowed = input
            clockOffsetMs = serverTime == 0 ? 0 : Int64(serverTime) - Int64(Self.localNowMs())
            now = serverNowMs()
        }
        store.apply(msg)
        // A selection that just went away should not silently point at nothing;
        // the Session tab renders a "this session ended" state off `selectedSid`
        // still being set, so it is deliberately NOT cleared here.
    }

    private func startClock() {
        guard clock == nil else { return }
        clock = Task { [weak self] in
            while !Task.isCancelled {
                self?.now = self?.serverNowMs() ?? AppModel.localNowMs()
                try? await Task.sleep(nanoseconds: 15_000_000_000)
            }
        }
    }

    private func serverNowMs() -> UInt64 {
        let adjusted = Int64(Self.localNowMs()) + clockOffsetMs
        return adjusted > 0 ? UInt64(adjusted) : Self.localNowMs()
    }

    private static func localNowMs() -> UInt64 {
        UInt64(Date().timeIntervalSince1970 * 1000)
    }
}

#if DEBUG
// Preview support lives in this file because `private(set)` is file-private on
// the setter — an extension anywhere else could not fake a connected model.
extension AppModel {
    static func preview(_ sessions: [Session] = Fixtures.everyTier,
                        state: ConnectionState = .connected) -> AppModel {
        let m = AppModel(settings: BridgeSettings(host: "10.0.0.14", port: BridgeSettings.defaultPort, token: "preview"),
                         autostart: false)
        m.store.apply(.sessions(epoch: "preview", sessions: sessions))
        m.connection = state
        m.now = Fixtures.now + 60_000
        m.demoMode = true
        m.selectedSid = sessions.first(where: { $0.pendingHeadline != nil })?.sid
        return m
    }
}
#endif
