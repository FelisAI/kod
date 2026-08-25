//  RemoteFlowUITests.swift — the only test that exercises the WHOLE chain.
//
//  Everything in KodTests is pure: it proves the ordering rules and the decoder in
//  isolation, against fixtures. None of it can catch the failure that actually
//  bit us — the app dialling a port the bridge does not bind — because that bug
//  lives in the seam between two binaries, and a unit test never crosses a seam.
//
//  So this drives the shipped app against a REAL bridge: tap each tab, assert the
//  connection chip went live, and keep a screenshot of each screen. It needs a
//  bridge on the host and is skipped (loudly) when there isn't one, because a
//  test that quietly passes with nothing behind it is worse than no test.

import XCTest

final class RemoteFlowUITests: XCTestCase {
    private var app: XCUIApplication!

    override func setUpWithError() throws {
        continueAfterFailure = false
        app = XCUIApplication()
        app.launch()
    }

    /// Screenshot every tab, and prove the session data actually arrived.
    func testTabsRenderAgainstALiveBridge() throws {
        let tabs = app.tabBars.firstMatch
        XCTAssertTrue(tabs.waitForExistence(timeout: 10), "no tab bar — the app did not reach RootView")

        // The chip is the whole chain's verdict: daemon -> bridge -> ws -> decode.
        // It is a Button wrapping the dot+label, so SwiftUI publishes it as a
        // BUTTON named "live" and never as a static text — query accordingly.
        guard liveChip.waitForExistence(timeout: 15) else {
            throw XCTSkip("""
                No live bridge. Start one and re-run:
                  cargo run -p orchestrator-bridge --bin kod-bridge -- serve <daemon.sock>
                then point bridge.host/bridge.port/bridge.token.fallback at it.
                """)
        }

        for name in ["Standup", "Projects", "Session"] {
            let tab = tabs.buttons[name]
            XCTAssertTrue(tab.waitForExistence(timeout: 5), "missing tab: \(name)")
            tab.tap()
            // The nav title confirms the tab actually swapped, not just highlighted.
            XCTAssertTrue(app.navigationBars[name].waitForExistence(timeout: 5),
                          "tapped \(name) but its navigation bar never appeared")
            attach(named: name)
        }
    }

    /// Standup must never be empty while sessions exist: an empty home screen with
    /// a live chip means the plan dropped everything the bridge sent.
    func testStandupSummarisesTheSessionsTheBridgeSent() throws {
        guard liveChip.waitForExistence(timeout: 15) else {
            throw XCTSkip("no live bridge — see testTabsRenderAgainstALiveBridge")
        }
        // Query across element TYPES, not just staticTexts: the ambient LIVE strip
        // is tappable, so SwiftUI publishes "8 idle across 3 projects" as a Button.
        // Asserting on the type would make this test a description of today's
        // layout rather than of the behaviour it is here to protect.
        let summary = app.descendants(matching: .any)
            .matching(NSPredicate(format: "label CONTAINS[c] 'projects'")).firstMatch
        XCTAssertTrue(summary.waitForExistence(timeout: 5),
                      "connected, but Standup shows no session summary at all")
        attach(named: "Standup-summary")
    }

    /// Projects -> expand -> tap a session must land on the Session tab showing
    /// THAT session. This is the app's only navigation path to the reader, and it
    /// crosses two tabs and a selection, so nothing pure can cover it.
    func testTappingASessionOpensItInTheSessionTab() throws {
        guard liveChip.waitForExistence(timeout: 15) else {
            throw XCTSkip("no live bridge — see testTabsRenderAgainstALiveBridge")
        }
        app.tabBars.firstMatch.buttons["Projects"].tap()
        XCTAssertTrue(app.navigationBars["Projects"].waitForExistence(timeout: 5))

        // The first project card: tapping the header expands it in place.
        // Match "sessions ·" (plural, with the separator) — a bare "session"
        // ALSO matches the "Session" TAB BUTTON, and tapping that silently
        // changes tab instead of expanding anything.
        let card = app.buttons
            .matching(NSPredicate(format: "label CONTAINS[c] 'sessions \u{00B7}'")).firstMatch
        XCTAssertTrue(card.waitForExistence(timeout: 5), "no project card to open")
        card.tap()
        attach(named: "Projects-expanded")

        // A session row appears only once expanded. Match on the row's own
        // "<cli> · <phase>" metatag: every project card subtitle also contains
        // "idle" ("2 sessions · 2 idle"), so a looser predicate picks another CARD.
        let row = app.buttons
            .matching(NSPredicate(format: "label CONTAINS[c] 'shell'")).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 5), "expanded, but no session row to tap")
        row.tap()

        XCTAssertTrue(app.navigationBars["Session"].waitForExistence(timeout: 5),
                      "tapped a session but never landed on the Session tab")
        XCTAssertFalse(app.staticTexts["No session open"].exists,
                       "landed on Session, but nothing was selected")
        attach(named: "Session-open")
    }

    /// The connection chip, reading "live" only once the snapshot has decoded.
    private var liveChip: XCUIElement { app.buttons["live"] }

    private func attach(named name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
