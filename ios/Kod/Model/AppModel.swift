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

/// The phone's half of a line typed at an agent: the draft, what is on the wire,
/// and why the last attempt was refused.
///
/// PURE, and it holds the draft text itself, for one reason: the box may be
/// emptied only when the Mac says it took the text. A `@State` string in the view
/// would have to empty itself on the tap — which loses whatever the daemon
/// refused — and the rule that matters most here would live where nothing can
/// test it.
struct Composer: Equatable {
    /// Which session this draft belongs to. Nothing here follows the user to
    /// another session: a line meant for one agent must not land in another.
    private(set) var sid: UInt64?
    var text: String = ""
    private(set) var inFlight: Step?
    private(set) var failure: String?
    /// The id of the send currently in flight, and the source of the next one.
    /// Monotonic and never reused, so an answer that arrives after its send was
    /// abandoned can be told apart from the answer to what is in flight NOW.
    private(set) var inFlightRid: UInt64 = 0
    private var nextRid: UInt64 = 1

    /// One thing on the wire, and what it was.
    enum Step: Equatable {
        /// A paste, carrying the exact text handed over. The text is kept so the
        /// ack can empty the box only if it STILL holds what was sent — the user
        /// may have typed more while the frame was in the air.
        case paste(String)
        /// The Enter that submits an accepted paste. The daemon pastes and stops
        /// (`KeyInput::Paste`), so without this the line sits in the agent's
        /// prompt, typed but never sent.
        case submit
        /// A control key pressed on its own.
        case key(PhoneKey)
    }

    init(sid: UInt64? = nil) { self.sid = sid }

    var busy: Bool { inFlight != nil }
    var canSend: Bool { !busy && !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }

    /// Typing clears the last refusal — it explained text that is no longer in
    /// the box.
    mutating func edit(_ new: String) {
        text = new
        failure = nil
    }

    /// Hand the draft to the wire. Nil when there is nothing to send, or when
    /// something already is — one tap must not become two lines.
    mutating func send(to sid: UInt64) -> ClientMessage? {
        guard canSend else { return nil }
        self.sid = sid
        failure = nil
        inFlight = .paste(text)
        inFlightRid = nextRid
        nextRid += 1
        return .input(sid: sid, text: text, rid: inFlightRid)
    }

    mutating func press(_ key: PhoneKey, on sid: UInt64) -> ClientMessage? {
        guard !busy else { return nil }
        self.sid = sid
        failure = nil
        inFlight = .key(key)
        inFlightRid = nextRid
        nextRid += 1
        return .key(sid: sid, key: key, rid: inFlightRid)
    }

    /// Apply the Mac's answer, and return whatever must follow it.
    mutating func settle(rid: UInt64, sid: UInt64, ok: Bool, message: String) -> ClientMessage? {
        // An answer for a session this composer is no longer pointed at belongs
        // to nobody: applying it would clear or blame the wrong draft.
        //
        // The rid check is the one that matters. Without it a LATE answer to an
        // abandoned send settles whatever is in flight now — and for a paste that
        // means dispatching the Enter below against text that never landed, i.e.
        // submitting the wrong thing at the agent.
        guard self.sid == sid, rid == inFlightRid, let step = inFlight else { return nil }
        guard ok else {
            inFlight = nil
            failure = message.isEmpty ? "your Mac refused it, without saying why" : message
            return nil
        }
        switch step {
        case .paste(let sent):
            if text == sent { text = "" }
            inFlight = .submit
            inFlightRid = nextRid
            nextRid += 1
            return .key(sid: sid, key: .enter, rid: inFlightRid)
        case .submit, .key:
            inFlight = nil
            return nil
        }
    }

    /// The send did not reach the Mac, or the link died before it answered. The
    /// text STAYS: it was not delivered, so it is not the user's to lose.
    mutating func fail(_ why: String) {
        guard busy else { return }
        inFlight = nil
        failure = why
    }
}

@MainActor
@Observable
final class AppModel {
    private(set) var store = SessionStore()
    private(set) var connection: ConnectionState = .unconfigured
    /// Whether this bridge relays typing AT ALL, as announced at hello. It is the
    /// coarse answer; `Session.canInput` is the per-session one, and the composer
    /// needs both — a Mac that answers false here has nothing to type into.
    private(set) var inputAllowed = false

    var tab: RootTab = .standup
    var selectedSid: UInt64? {
        didSet {
            // Every path that changes the subject goes through here, which is why
            // the reset lives here and not in the view. An in-flight send is
            // abandoned rather than followed: its answer will name a session this
            // composer no longer points at, and `settle` drops it.
            if selectedSid != oldValue { composer = Composer(sid: selectedSid) }
        }
    }
    var showConnectionSheet = false

    private(set) var settings = BridgeSettings.empty
    private(set) var composer = Composer()

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
        client.onInputResult = { [weak self] answer in self?.settle(answer) }
        client.onDisconnect = { [weak self] in
            // Frozen rows shown as live are a lie; the next attach mints a new
            // epoch and resends everything anyway.
            self?.store.flush()
            self?.inputAllowed = false
            // Nothing else ever answers a send that was in the air when the link
            // died — without this the composer waits forever on a socket that is
            // gone, and the user cannot even retype.
            self?.composer.fail("the link dropped before your Mac answered — it may not have arrived")
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
        store.apply(.sessions(epoch: "demo", sessions: fixtures.map(Self.asTheDaemonWouldMark)))
        connection = .connected
        inputAllowed = true
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

    /// The daemon's own rule — agents that are alive accept typing, shells and
    /// dead sessions never do — applied to fixtures, which carry no `can_input`
    /// because they never came off a wire. Without it every preview and every
    /// `-kod-demo` run would show the composer's refusal state and nothing else.
    fileprivate static func asTheDaemonWouldMark(_ s: Session) -> Session {
        var marked = s
        marked.canInput = s.alive && s.phase != .dead && s.cli != .shell
        return marked
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

    /// The composer's text, as a settable property so the field can bind to it.
    /// Every write goes through `edit`, which is what drops a stale refusal the
    /// moment the user starts changing the text it was about.
    var draft: String {
        get { composer.text }
        set { composer.edit(newValue) }
    }

    /// Send the draft to the selected session. `canInput` is re-checked here and
    /// not only in the view: a session can die between the frame that drew the
    /// composer and the tap on its send button.
    func sendDraft() {
        guard let s = selected, s.canInput, let msg = composer.send(to: s.sid) else { return }
        transmit(msg)
    }

    func press(_ key: PhoneKey) {
        guard let s = selected, s.canInput, let msg = composer.press(key, on: s.sid) else { return }
        transmit(msg)
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

    /// Put one message on the wire and own what happens to it. The socket's own
    /// refusal is reported here; the daemon's arrives later as `input_result`.
    private func transmit(_ msg: ClientMessage) {
        // The bridge answers an oversized frame with `err` and KEEPS the
        // connection, so a too-long paste would leave the composer waiting on an
        // `input_result` that is never coming. Refuse it while there is still
        // someone to tell.
        guard msg.json.utf8.count <= kMaxFrameBytes else {
            composer.fail("that is too long to send from the phone")
            return
        }
        Task { [weak self] in
            guard let self else { return }
            if let why = await client.send(msg) { composer.fail(why) }
        }
    }

    /// An accepted paste is only half of a send: the composer hands back the
    /// Enter that submits it, and that goes out on this same ack.
    private func settle(_ answer: InputResult) {
        if let next = composer.settle(rid: answer.rid, sid: answer.sid, ok: answer.ok, message: answer.message) {
            transmit(next)
        }
    }

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
        m.store.apply(.sessions(epoch: "preview", sessions: sessions.map(asTheDaemonWouldMark)))
        m.inputAllowed = true
        m.connection = state
        m.now = Fixtures.now + 60_000
        m.demoMode = true
        m.selectedSid = sessions.first(where: { $0.pendingHeadline != nil })?.sid
        return m
    }
}
#endif
