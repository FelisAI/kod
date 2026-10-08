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
    /// The last text this composer got an ACCEPTANCE for, kept so the reader can
    /// show it. Without it the screen is identical before and after a send, and
    /// the only feedback is the box emptying — which reads as "did that do
    /// anything?" right up until the agent finishes its turn, which can be
    /// minutes. Cleared when the composer moves to another session.
    private(set) var delivered: String?
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
        if self.sid != sid { delivered = nil }
        self.sid = sid
        failure = nil
        // The step keeps the text AS TYPED, because the ack compares it with what
        // is in the box now. Only the wire copy is flattened.
        inFlight = .paste(text)
        inFlightRid = nextRid
        nextRid += 1
        return .input(sid: sid, text: Self.oneLine(text), rid: inFlightRid)
    }

    /// The text as the daemon should receive it: line breaks become spaces.
    ///
    /// The daemon strips every control character from phone text, newlines
    /// included, because a newline in a terminal is SUBMIT and submitting is its
    /// own key. Stripped rather than replaced, a two-line answer arrives with the
    /// last word of one line glued to the first word of the next — so the phone
    /// says what it means before the Mac has to guess.
    static func oneLine(_ text: String) -> String {
        text.split(omittingEmptySubsequences: false, whereSeparator: \.isNewline)
            .joined(separator: " ")
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
            let why = message.isEmpty ? "your Mac refused it, without saying why" : message
            failure = step == .submit ? Self.submitInDoubt(why) : why
            return nil
        }
        switch step {
        case .paste(let sent):
            if text == sent { text = "" }
            delivered = sent
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
        guard let step = inFlight else { return }
        inFlight = nil
        failure = step == .submit ? Self.submitInDoubt(why) : why
    }

    /// The one failure where "your text is still here" would be false.
    ///
    /// The paste WAS accepted — that acceptance is what emptied the box and
    /// filled "you sent" — so the text is sitting in the session's prompt and only
    /// the Enter that submits it is in doubt. Retyping would paste it twice; what
    /// the user needs is the enter key, and the sentence has to say so.
    static func submitInDoubt(_ why: String) -> String {
        "Your text reached the session, but the Enter that submits it did not (\(why)). "
            + "Tap enter to submit it."
    }
}

/// Which terminal the Session screen wants streamed, and which one THIS
/// connection has been told about.
///
/// Two values, because they have different lifetimes. What the screen wants
/// survives a reconnect; what the bridge was told does not — it keeps one watch
/// per connection and forgets it when the connection ends. One variable doing
/// both jobs is how the terminal used to freeze for good after the first
/// reconnect (or the first trip to the background): it still read "already
/// watching", so the new connection was never told, and no grid ever came again.
/// A watch asked for before the link was up was lost the same way.
struct WatchPlan: Equatable {
    private(set) var wanted: UInt64?
    private(set) var sent: UInt64?

    mutating func request(_ sid: UInt64?) { wanted = sid }

    /// The connection is gone, and its watch with it.
    mutating func linkEnded() { sent = nil }

    /// What to put on the wire to make this connection match `wanted` — nil when
    /// nothing is needed, or nothing can be sent yet. A watch written before
    /// `hello_ok` is answered with `err` and dropped, so `linkUp` must be true.
    ///
    /// Switching sessions sends only the new watch (the bridge replaces the old
    /// one); an explicit off is sent only when nothing is wanted, which is what
    /// stops a phone being streamed a terminal nobody is looking at.
    mutating func next(linkUp: Bool) -> ClientMessage? {
        guard linkUp, sent != wanted else { return nil }
        let previous = sent
        sent = wanted
        if let sid = wanted { return .watch(sid: sid, on: true) }
        if let previous { return .watch(sid: previous, on: false) }
        return nil
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
    /// Whether this bridge sends terminals at all, as announced at hello. False
    /// against an older Mac, which is exactly right: it cannot send them, so the
    /// phone must not offer a screen that would stay blank.
    private(set) var gridAllowed = false
    /// The terminal the Session screen wants, and what this connection was told.
    private var watching = WatchPlan()

    var tab: RootTab = .standup
    /// The Session tab's subject. Changing it touches no composer: each session
    /// keeps its own (`composers`).
    var selectedSid: UInt64?
    var showConnectionSheet = false

    private(set) var settings = BridgeSettings.empty

    /// One composer per session, for the life of a bridge attach.
    ///
    /// There used to be ONE, replaced whenever the selection changed, and three
    /// things went wrong with it. A draft was thrown away by a glance at another
    /// session. A send in flight was abandoned rather than followed, so an
    /// accepted paste never got the Enter that submits it — text left sitting in
    /// the agent's prompt, typed and unsent. And the replacement restarted its
    /// request ids at 1, so the late answer to an abandoned send could settle a
    /// NEW send with the same id: emptying the box and submitting text the Mac
    /// had not taken. Keyed by session, a composer lives as long as its session
    /// can, answers route to the one that asked, and ids never repeat within it.
    private var composers: [UInt64: Composer] = [:]

    /// The selected session's composer — what the Session screen draws.
    var composer: Composer {
        guard let sid = selectedSid else { return Composer() }
        return composers[sid] ?? Composer(sid: sid)
    }

    /// The bridge attach the selection and the composers belong to. Not the
    /// store's epoch: a dropped link flushes that, and a reconnect to the SAME
    /// bridge must keep the user's place and their drafts.
    private var attachEpoch: String?

    /// Server-clock now, in ms. Every age on screen is measured against this and
    /// it ticks on a timer, which is what makes "12m" become "13m" without a frame
    /// arriving. Raw `Date()` would not: nothing would tell SwiftUI to re-render.
    private(set) var now: UInt64 = AppModel.localNowMs()
    /// serverTime - localTime at hello, so a phone clock minutes off the Mac does
    /// not render every session as having waited since the Cretaceous.
    private var clockOffsetMs: Int64 = 0

    private let client = BridgeClient()
    private var clock: Task<Void, Never>?
    /// Showing sample data instead of a Mac: no socket, no ticking clock, and
    /// typing is answered here rather than by a daemon.
    ///
    /// SHIPPED, in every build, and reachable from the UI: without a Mac running
    /// Kod this app is a connection screen and nothing else, which is unevaluable
    /// for anyone deciding whether to set it up — App Review included. It used to
    /// be declared under `#if DEBUG` while `start()` read it unconditionally, so
    /// the Release build — the only one that can be uploaded — did not compile.
    /// Internal, not fileprivate, because the views must say so on screen; a demo
    /// that does not announce itself is a lie.
    private(set) var demoMode = false
    /// The sample sessions' rev counter, so a demo session can change state
    /// through the same `SessionStore.apply` a bridge frame goes through.
    private var demoRev: UInt64 = 0

    init(settings: BridgeSettings = SettingsStore.load(), autostart: Bool = true) {
        self.settings = settings
        client.onState = { [weak self] state in
            guard let self else { return }
            connection = state
            // A link that just came up has no watch on it yet, whatever this
            // phone sent the last one.
            if state.isConnected { syncWatch() }
        }
        client.onMessage = { [weak self] msg in self?.ingest(msg) }
        client.onInputResult = { [weak self] answer in self?.settle(answer) }
        client.onDisconnect = { [weak self] in
            // Frozen rows shown as live are a lie; the next attach mints a new
            // epoch and resends everything anyway.
            self?.store.flush()
            self?.linkEnded()
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
    /// involve pointing a client at the real daemon. Launch arguments stay a
    /// development hook; the shipped way in is `enterDemo`.
    private func seedDemoIfRequested() {
        let args = CommandLine.arguments
        guard args.contains("-kod-demo") else { return }
        enterDemo(args.contains("-kod-quiet") ? Fixtures.allQuiet : Fixtures.everyTier)
        if let i = args.firstIndex(of: "-kod-tab"), i + 1 < args.count {
            switch args[i + 1] {
            case "projects": tab = .projects
            case "session": tab = .session
            default: tab = .standup
            }
        }
    }
    #endif

    /// Fill the app with sample sessions and dial nothing.
    ///
    /// The same fixtures the previews and the `-kod-demo` launch argument use, so
    /// what a reviewer or a curious user sees is what the design was checked
    /// against.
    func enterDemo(_ sessions: [Session] = Fixtures.everyTier) {
        stop()
        demoMode = true
        demoRev = 0
        composers.removeAll()
        store = SessionStore()
        store.apply(.sessions(epoch: "demo", sessions: sessions.map(Self.asTheDaemonWouldMark)))
        connection = .connected("sample data")
        inputAllowed = true
        gridAllowed = true
        now = Fixtures.now + 60_000
        selectedSid = sessions.contains { $0.sid == Fixtures.firstToAnswer }
            ? Fixtures.firstToAnswer
            : sessions.first?.sid
        tab = .standup
        syncWatch()
    }

    /// Leave the demo and go back to whatever was configured.
    func exitDemo() {
        guard demoMode else { return }
        leaveDemo()
        start()
    }

    /// Everything the demo put on screen, gone — without dialling anything. The
    /// half of `exitDemo` that pairing also needs: a code scanned while the
    /// sample data is up must connect, not be swallowed by a model that still
    /// thinks it is a demo.
    private func leaveDemo() {
        guard demoMode else { return }
        demoMode = false
        store = SessionStore()
        selectedSid = nil
        composers.removeAll()
        inputAllowed = false
        gridAllowed = false
        watching.linkEnded()
        connection = .unconfigured
        tab = .standup
    }

    /// The daemon's own rule — every LIVE session accepts typing, shells
    /// included, and a dead one never does — applied to fixtures, which carry no
    /// `can_input` because they never came off a wire. It mirrors the bridge's
    /// `can_input: alive`; a copy of an older rule here once kept shells
    /// read-only on the phone long after the Mac started accepting them.
    static func asTheDaemonWouldMark(_ s: Session) -> Session {
        var marked = s
        marked.canInput = s.alive && s.phase != .dead
        return marked
    }

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
        set {
            guard let sid = selectedSid else { return }
            composers[sid, default: Composer(sid: sid)].edit(newValue)
        }
    }

    /// Send the draft to the selected session. `canInput` is re-checked here and
    /// not only in the view: a session can die between the frame that drew the
    /// composer and the tap on its send button.
    func sendDraft() {
        guard let s = selected, s.canInput,
              let msg = composers[s.sid, default: Composer(sid: s.sid)].send(to: s.sid)
        else { return }
        transmit(msg)
    }

    func press(_ key: PhoneKey) {
        guard let s = selected, s.canInput,
              let msg = composers[s.sid, default: Composer(sid: s.sid)].press(key, on: s.sid)
        else { return }
        transmit(msg)
    }

    /// Point the bridge's terminal stream at one session, or turn it off.
    ///
    /// This records what the screen WANTS; `syncWatch` decides what to send, now
    /// and again on every new connection. Idempotent, because SwiftUI calls
    /// `onAppear` more than once for the same screen and a watch per call would
    /// be a watch per re-layout.
    func watch(_ sid: UInt64?) {
        watching.request(sid)
        syncWatch()
    }

    /// Bring this connection's watch in line with what the screen wants.
    private func syncWatch() {
        if demoMode {
            if let sid = watching.wanted, let g = Fixtures.grid(for: sid), store.grid?.sid != sid {
                store.apply(.grid(epoch: "demo", grid: g))
            }
            return
        }
        if let msg = watching.next(linkUp: gridAllowed && connection.isConnected) {
            send(msg)
        }
    }

    /// Fire-and-forget, unlike `transmit`: a watch has no `rid`, gets no answer,
    /// and must never touch the composer's in-flight state.
    private func send(_ msg: ClientMessage) {
        Task { [weak self] in
            guard let self else { return }
            _ = await self.client.send(msg)
        }
    }

    func apply(settings new: BridgeSettings) {
        // NORMALISED before it is kept, not only before it is stored. Holding the
        // raw value meant `model.settings` and the store could differ by a
        // trailing space — so a form comparing its fields against `settings` to
        // decide whether anything had changed could read "edited" for a value it
        // had just saved.
        let new = new.normalized()
        let changed = new != settings
        settings = new
        SettingsStore.save(new)
        // Pairing is the way OUT of the sample data, not something to do behind
        // it: a demo model dials nothing, so a code scanned with the demo up
        // would otherwise be saved and then silently ignored.
        leaveDemo()
        if changed {
            // What is on screen came from the OLD settings — possibly another
            // Mac — and the client's restart cancels that link without a
            // disconnect callback. Left alone, the old Mac's sessions stayed up
            // under a banner naming the new one, for as long as the new one took
            // to answer, or forever if it never did.
            store.flush()
            linkEnded()
        }
        start()
    }

    func start() {
        // A demo model dials nothing and freezes its clock; otherwise the
        // foreground restart would stomp the fixture `now` with wall-clock time
        // and every age would read in days.
        if demoMode { return }
        startClock()
        // HANDED OVER UNCONDITIONALLY, usable or not.
        //
        // There used to be a guard here that set `.insecure`/`.unconfigured` and
        // returned WITHOUT touching the client — which meant a running loop kept
        // running. Saving an unusable settings object therefore left the old
        // socket dialling the OLD address, overwriting the state this line had
        // just set, one `.connecting` and `.reconnecting` at a time: the user
        // typed their Wi-Fi address, watched a connect/timeout loop against a
        // tailnet address they could no longer see named anywhere, and never once
        // saw the sentence explaining why the new one was refused. The client's
        // own `start` stops first and reaches the same two states (`BridgeClient.
        // start`), so the guard bought nothing and cost the teardown — and it
        // also left `BridgeClient.settings` stale, which is what made the banner's
        // "retry" re-dial the address the user had just replaced.
        client.startIfNeeded(settings)
    }

    /// Drop the link — the app is going to the background.
    ///
    /// The rows stay on screen, but the state stops claiming they are live: until
    /// the next `hello_ok` replaces them, they are what the Mac said BEFORE, and
    /// the banner says it is reconnecting. A cancelled socket never reaches the
    /// client's disconnect path, so everything that path would have reset is
    /// reset here too.
    func stop() {
        client.stop()
        clock?.cancel()
        clock = nil
        guard !demoMode else { return }
        linkEnded()
        if case .connected(let at) = connection { connection = .connecting(at) }
    }

    func retry() {
        if demoMode { return }
        client.retry()
    }

    // MARK: - Plumbing

    /// What every way a link can end has in common.
    private func linkEnded() {
        inputAllowed = false
        // The bridge's watch died with the connection; the next one starts
        // with none, and `syncWatch` will re-send what the screen wants.
        watching.linkEnded()
        // Nothing else ever answers a send that was in the air when the link
        // died — without this a composer waits forever on a socket that is
        // gone, and the user cannot even retype. Every composer, not just the
        // one on screen: a send made before switching sessions is still in the
        // air.
        for sid in composers.keys {
            composers[sid]?.fail("the link dropped before your Mac answered — it may not have arrived")
        }
    }

    /// Put one message on the wire and own what happens to it. The socket's own
    /// refusal is reported here; the daemon's arrives later as `input_result`.
    private func transmit(_ msg: ClientMessage) {
        guard let (sid, rid) = msg.request else { return }
        // The bridge answers an oversized frame with `err` and KEEPS the
        // connection, so a too-long paste would leave the composer waiting on an
        // `input_result` that is never coming. Refuse it while there is still
        // someone to tell.
        guard msg.json.utf8.count <= kMaxFrameBytes else {
            composers[sid]?.fail("that is too long to send from the phone")
            return
        }
        if demoMode {
            answerLikeTheMac(msg)
            return
        }
        Task { [weak self] in
            guard let self else { return }
            // Only if that send is still the one in flight: by the time the
            // socket says no, a deadline may already have failed it and the user
            // sent something new.
            if let why = await client.send(msg), composers[sid]?.inFlightRid == rid {
                composers[sid]?.fail(why)
            }
        }
        armInputDeadline(sid: sid, rid: rid)
    }

    /// The sample data's stand-in for a daemon: accept, a beat later, the way a
    /// Mac on the same Wi-Fi would — and when what was sent answers a waiting
    /// agent, let that agent get back to work, so the demo shows the whole loop
    /// rather than a box that empties and nothing else.
    private func answerLikeTheMac(_ msg: ClientMessage) {
        let (rid, sid, answersTheAgent): (UInt64, UInt64, Bool)
        switch msg {
        case .input(let s, _, let r): (rid, sid, answersTheAgent) = (r, s, false)
        case .key(let s, let k, let r): (rid, sid, answersTheAgent) = (r, s, k == .enter || k == .escape)
        default: return
        }
        Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(350))
            guard let self, demoMode else { return }
            settle(InputResult(rid: rid, sid: sid, ok: true, message: ""))
            if answersTheAgent { demoAgentResumes(sid) }
        }
    }

    private func demoAgentResumes(_ sid: UInt64) {
        guard var s = store[sid], s.phase == .awaiting || s.pendingHeadline != nil else { return }
        s.phase = .busy
        s.pendingHeadline = nil
        s.phaseSince = now
        demoRev += 1
        store.apply(.session(epoch: "demo", rev: demoRev, session: s))
        if let g = Fixtures.gridAfterAnswer(for: sid) {
            store.apply(.grid(epoch: "demo", grid: g))
        }
    }

    /// How long to wait for the Mac's answer before giving the composer back.
    ///
    /// Every send is a request/reply, and the reply can simply not arrive: the
    /// link drops mid-flight, or a frame the bridge answers with `err` rather
    /// than `input_result` lands on the floor. Without a deadline the composer
    /// stays busy for the life of the app — spinner instead of a send button,
    /// every control key disabled — and the only escape is switching sessions,
    /// which is also the one action that discards the draft. Backgrounding a
    /// phone mid-send is the ordinary way to reach that.
    private static let inputDeadline: Duration = .seconds(12)

    private func armInputDeadline(sid: UInt64, rid: UInt64) {
        Task { [weak self] in
            try? await Task.sleep(for: Self.inputDeadline)
            guard let self else { return }
            // Only the send this deadline was armed for. A later send has its own,
            // and settling it here would blame the wrong text.
            guard let c = composers[sid], c.inFlightRid == rid, c.busy else { return }
            composers[sid]?.fail("your Mac did not answer. Your text is still here — try again.")
        }
    }

    /// An accepted paste is only half of a send: the composer hands back the
    /// Enter that submits it, and that goes out on this same ack — whichever
    /// session is on screen by then.
    private func settle(_ answer: InputResult) {
        if let next = composers[answer.sid]?.settle(rid: answer.rid, sid: answer.sid,
                                                    ok: answer.ok, message: answer.message) {
            transmit(next)
        }
    }

    /// Internal rather than private so the epoch rule can be tested without a socket.
    func ingest(_ msg: ServerMessage) {
        if case .helloOk(_, let epoch, let serverTime, let input, let grid) = msg {
            inputAllowed = input
            gridAllowed = grid
            // A fresh connection, so a fresh watch. Sent once the client calls
            // the link connected — see `onState` — not from here, which runs
            // before the socket is marked ready for writes.
            watching.linkEnded()
            clockOffsetMs = serverTime == 0 ? 0 : Int64(serverTime) - Int64(Self.localNowMs())
            now = serverNowMs()
            // A NEW BRIDGE ATTACH: a restarted Mac, or a different one. Session
            // ids restart at 1 in a new daemon, so the selection and every draft
            // may now name a different session — and a draft written for one
            // agent must never be sendable to another. A reconnect to the same
            // bridge keeps its epoch, and keeps both.
            if let held = attachEpoch, held != epoch {
                composers.removeAll()
                selectedSid = nil
            }
            attachEpoch = epoch
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
                        state: ConnectionState = .connected("10.0.0.14:18787")) -> AppModel {
        let m = AppModel(settings: BridgeSettings(host: "10.0.0.14", port: BridgeSettings.defaultPort, token: "preview"),
                         autostart: false)
        m.enterDemo(sessions)
        m.connection = state
        m.selectedSid = sessions.first(where: { $0.pendingHeadline != nil })?.sid
        return m
    }
}
#endif
