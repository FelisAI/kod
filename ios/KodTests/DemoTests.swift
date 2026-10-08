//  DemoTests.swift — the sample data is a shipped feature, so it is tested like one.
//
//  Without a Mac running Kod this app is a connection screen and nothing else.
//  "Explore with sample data" is what App Review — and anyone deciding whether to
//  set Kod up — actually gets to use, so it has to work in the build that ships:
//  it was once declared under #if DEBUG while `start()` read it unconditionally,
//  and the Release build did not compile at all.

import XCTest
@testable import Kod

@MainActor
final class DemoTests: XCTestCase {
    private func demo() -> AppModel {
        let m = AppModel(settings: .empty, autostart: false)
        m.enterDemo()
        return m
    }

    func testTheSampleDataFillsEveryTier() {
        let m = demo()
        XCTAssertTrue(m.demoMode)
        XCTAssertTrue(m.connection.isConnected)
        XCTAssertFalse(m.standup.blocked.isEmpty, "a blocked session, so the wall is on show")
        XCTAssertGreaterThanOrEqual(m.standup.needsYou.count, 2, "the needs-you queue, oldest first")
        XCTAssertFalse(m.standup.live.isEmpty, "the ambient strip")
        XCTAssertEqual(m.selectedSid, Fixtures.firstToAnswer)
        XCTAssertEqual(m.standup.needsYou.first?.sid, Fixtures.firstToAnswer,
                       "it opens on the one that has waited longest")
    }

    /// Every live sample session takes typing, shells included — the bridge's
    /// rule (`can_input: alive`). A dead one never does.
    func testTheSampleDataIsMarkedTheWayTheMacMarksSessions() {
        let m = demo()
        for s in m.sessions {
            XCTAssertEqual(s.canInput, s.alive && s.phase != .dead, "\(s.displayTitle)")
        }
        XCTAssertTrue(m.sessions.contains { $0.cli == .shell && $0.canInput },
                      "a live shell is typeable: the Mac accepts it")
    }

    func testWatchingASampleSessionShowsItsTerminal() {
        let m = demo()
        m.watch(Fixtures.firstToAnswer)
        XCTAssertEqual(m.store.grid?.sid, Fixtures.firstToAnswer)
        XCTAssertFalse(m.store.grid?.lines.isEmpty ?? true)
    }

    /// The whole loop: type, the "Mac" accepts, the Enter that submits follows,
    /// and the agent that was waiting gets back to work.
    func testTypingIntoTheSampleDataIsAnsweredAndTheAgentResumes() async throws {
        let m = demo()
        m.watch(Fixtures.firstToAnswer)
        XCTAssertEqual(m.selected?.phase, .awaiting)

        m.draft = "yes, run it"
        m.sendDraft()
        XCTAssertTrue(m.composer.busy)

        try await waitUntil { !m.composer.busy }
        XCTAssertNil(m.composer.failure, "the sample data must never answer with an error")
        XCTAssertEqual(m.composer.delivered, "yes, run it")
        XCTAssertEqual(m.draft, "")
        XCTAssertEqual(m.selected?.phase, .busy, "answered, so it is working again")
        XCTAssertNil(m.selected?.pendingHeadline)
        XCTAssertFalse(m.standup.needsYou.contains { $0.sid == Fixtures.firstToAnswer },
                       "and it has left the needs-you queue")
    }

    func testExitingPutsTheRealStateBack() {
        let m = demo()
        m.exitDemo()
        XCTAssertFalse(m.demoMode)
        XCTAssertTrue(m.sessions.isEmpty, "no sample session may outlive the demo")
        XCTAssertNil(m.selectedSid)
        XCTAssertFalse(m.connection.isConnected, "an unpaired phone is not connected to anything")
    }

    /// Backgrounding the app stops the link; with sample data up there is no link,
    /// and stopping must not turn "sample data" into a reconnecting banner.
    func testBackgroundingDoesNotDisturbTheSampleData() {
        let m = demo()
        m.stop()
        m.start()
        XCTAssertTrue(m.demoMode)
        XCTAssertEqual(m.connection, .connected("sample data"))
        XCTAssertFalse(m.sessions.isEmpty)
    }

    /// Pairing is the way OUT of the sample data. A demo model dials nothing, so a
    /// code scanned with the sample data up used to be saved and then ignored.
    func testPairingLeavesTheSampleData() {
        let before = SettingsStore.load()
        defer { SettingsStore.save(before) }

        let m = demo()
        // Unusable on purpose (no token): the point is what happens to the demo,
        // and a test must not dial anything.
        m.apply(settings: BridgeSettings(host: "127.0.0.1", port: 18787, token: ""))
        XCTAssertFalse(m.demoMode)
        XCTAssertTrue(m.sessions.isEmpty)
    }

    // MARK: - one composer per session

    /// A glance at another session used to throw the draft away.
    func testADraftSurvivesLookingAtAnotherSession() {
        let m = demo()
        m.selectedSid = 2
        m.draft = "half a thought"
        m.selectedSid = 3
        XCTAssertEqual(m.draft, "", "session 3 has its own, empty, box")
        m.draft = "for three"
        m.selectedSid = 2
        XCTAssertEqual(m.draft, "half a thought")
        m.selectedSid = 3
        XCTAssertEqual(m.draft, "for three")
    }

    /// Switching away mid-send used to ABANDON the send: the paste had been
    /// accepted, but its answer then belonged to a composer that no longer
    /// existed, so the Enter that submits it never went out — the text sat in the
    /// agent's prompt, typed and unsent.
    func testASendIsFollowedThroughAfterSwitchingAway() async throws {
        let m = demo()
        m.selectedSid = 2
        m.draft = "yes, run it"
        m.sendDraft()
        m.selectedSid = 3          // before the Mac has answered

        try await waitUntil { m.store[2]?.phase == .busy }
        m.selectedSid = 2
        XCTAssertFalse(m.composer.busy)
        XCTAssertEqual(m.composer.delivered, "yes, run it", "the paste was accepted")
        XCTAssertNil(m.store[2]?.pendingHeadline, "and the Enter that submits it went out")
    }

    // MARK: - the attach a selection belongs to

    /// Session ids restart at 1 when the Mac's daemon does, so after a new bridge
    /// attach "session 1" may be another agent entirely. A draft written for one
    /// must never be one tap from being sent to the other. A reconnect to the
    /// SAME bridge keeps its epoch, and keeps the user's place.
    func testANewAttachForgetsTheSelectionAndTheDraftsButAReconnectDoesNot() {
        let m = AppModel(settings: .empty, autostart: false)
        let hello = { (epoch: String) in
            ServerMessage.helloOk(proto: 2, epoch: epoch, serverTime: 0,
                                  inputAllowed: true, gridAllowed: true)
        }
        let one = AppModel.asTheDaemonWouldMark(Fixtures.session(1, "p", "first", phase: .awaiting))
        m.ingest(hello("e1"))
        m.ingest(.sessions(epoch: "e1", sessions: [one]))
        m.selectedSid = 1
        m.draft = "meant for the first agent"

        m.ingest(hello("e1"))   // the same bridge, reconnected
        XCTAssertEqual(m.selectedSid, 1)
        XCTAssertEqual(m.draft, "meant for the first agent")

        m.ingest(hello("e2"))   // a new attach
        XCTAssertNil(m.selectedSid)
        m.selectedSid = 1
        XCTAssertEqual(m.draft, "", "the old draft must not reappear on the new session 1")
    }

    private func waitUntil(timeout: Duration = .seconds(3),
                           _ condition: () -> Bool) async throws {
        let clock = ContinuousClock()
        let deadline = clock.now + timeout
        while !condition() {
            guard clock.now < deadline else { return XCTFail("timed out") }
            try await Task.sleep(for: .milliseconds(20))
        }
    }
}
