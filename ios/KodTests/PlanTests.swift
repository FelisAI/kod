//  PlanTests.swift — the ordering rules, which are the product.
//
//  If needs-you ever stops being oldest-first, or Projects ever starts sorting by
//  recency, the app still "works" and is still useless. These are the assertions
//  that catch that.

import XCTest
@testable import Kod

final class PlanTests: XCTestCase {
    private let now: UInt64 = 1_000_000

    private func s(_ sid: UInt64,
                   _ project: String = "p",
                   phase: Phase = .busy,
                   since: UInt64 = 0,
                   alive: Bool = true,
                   limitHit: Bool = false) -> Session {
        Session(sid: sid, cli: .claude, project: project, title: "t\(sid)", phase: phase,
                phaseSince: since, alive: alive, lastMessage: "", pendingHeadline: nil,
                trouble: nil, limitHit: limitHit, limitPercent: nil, limitReset: nil)
    }

    func testTiersSplitOnlyThreeWays() {
        let plan = StandupPlan(sessions: [
            s(1, phase: .awaiting, since: 500, limitHit: true),  // blocked wins over awaiting
            s(2, phase: .awaiting, since: 100),
            s(3, phase: .busy),
            s(4, phase: .idle),
            s(5, phase: .dead, alive: false),
        ])
        XCTAssertEqual(plan.blocked.map(\.sid), [1])
        XCTAssertEqual(plan.needsYou.map(\.sid), [2])
        XCTAssertEqual(plan.live.map(\.sid), [3, 4], "dead sessions are in no tier")
        XCTAssertEqual(plan.attentionCount, 2)
        XCTAssertFalse(plan.isQuiet)
    }

    func testNeedsYouIsOldestFirst() {
        let plan = StandupPlan(sessions: [
            s(1, phase: .awaiting, since: 900),
            s(2, phase: .awaiting, since: 100),
            s(3, phase: .awaiting, since: 500),
        ])
        XCTAssertEqual(plan.needsYou.map(\.sid), [2, 3, 1],
                       "the one waiting longest is costing the most and must stay on top")
    }

    func testEqualWaitTimesTieBreakOnSidSoRowsCannotSwap() {
        let plan = StandupPlan(sessions: [
            s(9, phase: .awaiting, since: 100),
            s(4, phase: .awaiting, since: 100),
        ])
        XCTAssertEqual(plan.needsYou.map(\.sid), [4, 9])
    }

    func testAmbientSentence() {
        let plan = StandupPlan(sessions: [
            s(1, "a", phase: .busy), s(2, "b", phase: .busy), s(3, "c", phase: .busy),
            s(4, "a", phase: .idle), s(5, "b", phase: .idle),
            s(6, "d", phase: .spawning),
        ])
        XCTAssertEqual(plan.ambientSentence, "3 working · 2 idle · 1 starting across 4 projects")
    }

    func testQuietStandup() {
        let plan = StandupPlan(sessions: [s(1, phase: .idle), s(2, phase: .busy)])
        XCTAssertTrue(plan.isQuiet)
        XCTAssertEqual(plan.ambientSentence, "1 working · 1 idle across 1 project")
    }

    func testEmptyStandup() {
        let plan = StandupPlan(sessions: [])
        XCTAssertTrue(plan.isQuiet)
        XCTAssertEqual(plan.ambientSentence, "nothing else running")
    }

    func testProjectsSplitIntoExactlyTwoSections() {
        let plan = ProjectsPlan(sessions: [
            s(1, "zeta", phase: .busy),
            s(2, "alpha", phase: .idle),
            s(3, "archived", phase: .dead, alive: false),
        ])
        XCTAssertEqual(plan.active.map(\.project), ["alpha", "zeta"])
        XCTAssertEqual(plan.rest.map(\.project), ["archived"])
    }

    func testNeedsYouProjectsFloatToTopOfActive() {
        let plan = ProjectsPlan(sessions: [
            s(1, "alpha", phase: .busy),
            s(2, "beta", phase: .busy),
            s(3, "zeta", phase: .awaiting),
        ])
        XCTAssertEqual(plan.active.map(\.project), ["zeta", "alpha", "beta"])
    }

    func testActiveOrderIsAlphabeticalNotRecency() {
        // Newest phase_since first would give [old-but-touched-last ...]; the rule
        // is alphabetical precisely so a printing agent cannot reorder the screen.
        let plan = ProjectsPlan(sessions: [
            s(1, "zeta", phase: .busy, since: 999),
            s(2, "alpha", phase: .busy, since: 1),
        ])
        XCTAssertEqual(plan.active.map(\.project), ["alpha", "zeta"])
    }

    func testSessionsWithinAProjectPutAttentionFirstThenLive() {
        let plan = ProjectsPlan(sessions: [
            s(1, "p", phase: .idle),
            s(2, "p", phase: .dead, alive: false),
            s(3, "p", phase: .awaiting),
            s(4, "p", phase: .busy, limitHit: true),
        ])
        XCTAssertEqual(plan.active.first?.sessions.map(\.sid), [3, 4, 1, 2])
    }

    func testProjectCountsAndSubtitleCarryNoRecency() {
        let group = ProjectsPlan(sessions: [
            s(1, "p", phase: .busy), s(2, "p", phase: .busy), s(3, "p", phase: .idle),
        ]).active.first
        XCTAssertEqual(group?.subtitle, "3 sessions · 2 working · 1 idle")
        XCTAssertEqual(group?.liveCount, 3)
        XCTAssertEqual(group?.attentionCount, 0)
    }

    func testBlankProjectGetsAPlaceholderBucket() {
        let plan = ProjectsPlan(sessions: [s(1, "", phase: .busy)])
        XCTAssertEqual(plan.active.map(\.project), ["(no project)"])
    }

    func testAgeIsSaturating() {
        XCTAssertEqual(TimeFmt.age(since: now + 5_000, now: now), 0, "a phone clock ahead of the Mac must not underflow")
        XCTAssertEqual(TimeFmt.age(since: now - 60_000, now: now), 60_000)
    }

    func testTimeLabels() {
        XCTAssertEqual(TimeFmt.compact(0), "<1m")
        XCTAssertEqual(TimeFmt.compact(12 * 60_000), "12m")
        XCTAssertEqual(TimeFmt.compact(3 * 3_600_000), "3h")
        XCTAssertEqual(TimeFmt.compact(50 * 3_600_000), "2d")
        XCTAssertEqual(TimeFmt.ago(0), "just now")
        XCTAssertEqual(TimeFmt.ago(5 * 60_000), "5m ago")
    }

    func testSettingsURL() {
        XCTAssertEqual(BridgeSettings(host: "10.0.0.4", port: 8765, token: "t").url?.absoluteString, "ws://10.0.0.4:8765/")
        XCTAssertEqual(BridgeSettings(host: "ws://mac.local/", port: 80, token: "t").url?.absoluteString, "ws://mac.local:80/")
        XCTAssertEqual(BridgeSettings(host: "fe80::1", port: 9, token: "t").url?.absoluteString, "ws://[fe80::1]:9/")
        XCTAssertNil(BridgeSettings(host: "  ", port: 1, token: "t").url)
        XCTAssertFalse(BridgeSettings(host: "h", port: 8765, token: "").isUsable, "no token means nothing to dial with")
    }

    /// The bridge declares `pub const DEFAULT_PORT: u16 = 8787` in ws.rs and has its
    /// own test pinning it. This is the other half of that contract: the two numbers
    /// are compiled into different binaries in different languages, so nothing but a
    /// test on each side keeps them equal. If you change one, this fails.
    func testDefaultPortMatchesTheBridge() {
        XCTAssertEqual(BridgeSettings.defaultPort, 8787)
        XCTAssertEqual(BridgeSettings.empty.port, BridgeSettings.defaultPort)
    }

    // MARK: - ProjectName

    func testShortNameStripsTheKeyPrefixAndKeepsTheBasename() {
        XCTAssertEqual(ProjectName.short("path:/Users/me/local/orchestrator"), "orchestrator")
        XCTAssertEqual(ProjectName.short("github:acme/widget"), "widget")
    }

    func testShortNameSurvivesTrailingSlashesAndOddKeys() {
        // A stored path may keep its trailing slash; splitting naively would give "".
        XCTAssertEqual(ProjectName.short("path:/Users/me/local/orchestrator/"), "orchestrator")
        // Unrecognised keys are shown as-is rather than mangled into nothing.
        XCTAssertEqual(ProjectName.short("idea:zomb"), "idea:zomb")
        XCTAssertEqual(ProjectName.short(""), "")
        // A prefix with no body must not render as an empty pill.
        XCTAssertEqual(ProjectName.short("path:"), "path:")
        XCTAssertEqual(ProjectName.short("path:/"), "path:/")
    }

    func testShortNameOnlyStripsAnANCHOREDPrefix() {
        // "path:" appearing mid-key is part of the name, not a prefix to strip.
        XCTAssertEqual(ProjectName.short("path:/Users/me/notes/path:weird"), "path:weird")
    }

    func testQualifierDisambiguatesProjectsThatShareABasename() {
        XCTAssertEqual(ProjectName.qualifier("github:acme/widget"), "acme")
        XCTAssertEqual(ProjectName.qualifier("path:/Users/me/local/orchestrator"), "local")
        // Nothing to disambiguate with -> empty, and the view omits the separator.
        XCTAssertEqual(ProjectName.qualifier("github:solo"), "")
        XCTAssertEqual(ProjectName.qualifier("idea:zomb"), "")
    }

    /// The pill's colour must key on the FULL key, not the shortened label, or two
    /// different projects that share a basename would also share a colour.
    func testPillHueIsKeyedOnTheFullKey() {
        let a = "path:/Users/me/work/app"
        let b = "path:/Users/me/side/app"
        XCTAssertEqual(ProjectName.short(a), ProjectName.short(b), "precondition: same label")
        XCTAssertNotEqual(ProjectPill.hue(for: a), ProjectPill.hue(for: b))
    }

    /// The manual-entry path is the fallback when the QR scan is unavailable, and
    /// it is exactly where a token gets pasted from a terminal with a newline on
    /// the end. The bridge compares tokens byte-for-byte, so that lands as a
    /// generic "bad token" with nothing on screen to suggest whitespace.
    func testSettingsNormalizeStripsWhitespaceFromHostAndToken() {
        let messy = BridgeSettings(host: "  100.101.102.103 ", port: 8787, token: "  abc123\n")
        let clean = messy.normalized()
        XCTAssertEqual(clean.host, "100.101.102.103")
        XCTAssertEqual(clean.token, "abc123")
        XCTAssertEqual(clean.port, 8787, "the port is not text and must be untouched")
    }

    func testNormalizeIsIdempotent() {
        let once = BridgeSettings(host: " h ", port: 1, token: " t ").normalized()
        XCTAssertEqual(once, once.normalized())
    }
}
