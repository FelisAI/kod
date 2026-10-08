//  EndToEndUITests.swift — the whole chain, the way a person uses it.
//
//  Pair by pasting the Mac's link, read the sessions, open one, see its terminal,
//  type a command into it and interrupt it — against a REAL daemon and bridge.
//  Everything in KodTests is pure and cannot cross the seam between two
//  binaries; this is the test that does.
//
//  It needs a SANDBOX Mac side: a throwaway daemon seeded with shells, and a
//  bridge serving it. NEVER point it at the daemon that hosts real sessions —
//  attaching a freshly built client can retire that daemon and every session in
//  it. Pass the sandbox bridge's pairing link through the test runner's
//  environment:
//
//      TEST_RUNNER_KOD_PAIR_LINK='kod://pair?h=127.0.0.1&p=…&t=…&f=…' \
//        xcodebuild test -scheme KodUITests -only-testing:KodUITests/EndToEndUITests …
//
//  Without it the test skips, loudly.

import UIKit
import XCTest

final class EndToEndUITests: XCTestCase {
    private var app: XCUIApplication!
    private var link = ""

    override func setUpWithError() throws {
        continueAfterFailure = false
        guard let link = ProcessInfo.processInfo.environment["KOD_PAIR_LINK"], !link.isEmpty else {
            throw XCTSkip("set TEST_RUNNER_KOD_PAIR_LINK to a SANDBOX bridge's pairing link")
        }
        self.link = link
        app = XCUIApplication()
        app.launch()
    }

    func testPairReadWatchTypeAndInterrupt() throws {
        // 1. PAIR, the way the Mac offers it: "Copy pairing link", then Paste.
        UIPasteboard.general.string = link
        let chip = app.buttons["connection-chip"].firstMatch
        XCTAssertTrue(chip.waitForExistence(timeout: 10), "no connection chip")
        chip.tap()
        let paste = pasteButton
        XCTAssertTrue(paste.waitForExistence(timeout: 5), "no Paste button on the connection sheet")
        paste.tap()
        attach("1-paired")
        let status = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH 'connected to'")).firstMatch
        XCTAssertTrue(status.waitForExistence(timeout: 15), "pasting the link did not connect")
        // The form holds what was paired, token included. Asserted through the
        // VALUE, not a screenshot: iOS leaves a secure field's contents out of
        // screen captures, so in every screenshot this field looks empty.
        let tokenShown = (app.secureTextFields["bridge-token"].value as? String) ?? ""
        XCTAssertFalse(tokenShown.isEmpty || tokenShown == "64-character token",
                       "the token field looks empty after pairing: \(tokenShown)")
        app.buttons["Done"].tap()
        XCTAssertTrue(app.buttons["live"].waitForExistence(timeout: 10), "the chip never went live")

        // 2. READ. The sandbox has only shells, none waiting on anything.
        XCTAssertTrue(app.staticTexts["All quiet"].waitForExistence(timeout: 10))
        attach("2-standup")

        // 3. OPEN one, from Projects.
        app.tabBars.firstMatch.buttons["Projects"].tap()
        let card = app.buttons.matching(NSPredicate(format: "label CONTAINS[c] 'storefront'")).firstMatch
        XCTAssertTrue(card.waitForExistence(timeout: 5), "no storefront project")
        card.tap()
        let row = app.buttons.matching(NSPredicate(format: "label CONTAINS[c] 'shell'")).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 5), "expanded, but no session row")
        attach("3-projects")
        row.tap()
        XCTAssertTrue(app.navigationBars["Session"].waitForExistence(timeout: 5))

        // 4. THE TERMINAL, with nothing on the Mac changing. An idle shell sends
        // no new frames, so this is the bridge answering the watch itself.
        XCTAssertTrue(app.staticTexts["TERMINAL"].waitForExistence(timeout: 10),
                      "an idle session's terminal never arrived")
        attach("4-terminal")

        // 5. TYPE into the shell — the Mac accepts it, and so must the phone.
        let marker = "kod-e2e-\(Int.random(in: 100_000...999_999))"
        let field = app.textFields.matching(NSPredicate(format: "placeholderValue == 'type a command…'")).firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 5), "no composer for a live shell")
        field.tap()
        field.typeText("echo \(marker)")
        app.buttons["composer-send"].tap()
        let echoed = app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", marker)).firstMatch
        XCTAssertTrue(echoed.waitForExistence(timeout: 15), "the command never reached the terminal")
        // It ran, rather than sitting typed-but-unsubmitted: the OUTPUT line is
        // the marker alone, without the "echo" in front of it.
        let output = app.staticTexts.matching(NSPredicate(format: "label == %@", marker)).firstMatch
        XCTAssertTrue(output.waitForExistence(timeout: 15), "typed but never submitted")
        XCTAssertTrue(app.staticTexts["YOU SENT"].exists)
        attach("5-typed")

        // 6. INTERRUPT. A shell you can type into but not stop is worse than one
        // you cannot type into at all.
        field.tap()
        field.typeText("sleep 30")
        app.buttons["composer-send"].tap()
        let stop = app.buttons["Stop, Control-C"]
        XCTAssertTrue(stop.waitForExistence(timeout: 5))
        // The prompt comes back well inside the 30 seconds only if ^C landed.
        let started = Date()
        _ = app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'sleep 30'")).firstMatch
            .waitForExistence(timeout: 10)
        stop.tap()
        let caret = app.staticTexts.matching(NSPredicate(format: "label CONTAINS '^C'")).firstMatch
        XCTAssertTrue(caret.waitForExistence(timeout: 10), "^C never reached the shell")
        XCTAssertLessThan(Date().timeIntervalSince(started), 25, "the sleep ran to the end")
        attach("6-interrupted")

        // 7. THE BACKGROUND TRIP. The app drops its link on the way out and
        // dials a new one on the way back; the bridge forgets a watch with its
        // connection, so the terminal keeps updating only if the phone asks
        // again. It used to think it already had — and the screen froze.
        XCUIDevice.shared.press(.home)
        sleep(2)
        app.activate()
        XCTAssertTrue(app.buttons["live"].waitForExistence(timeout: 15), "never reconnected")
        let again = "kod-back-\(Int.random(in: 100_000...999_999))"
        field.tap()
        field.typeText("echo \(again)")
        app.buttons["composer-send"].tap()
        let after = app.staticTexts.matching(NSPredicate(format: "label == %@", again)).firstMatch
        XCTAssertTrue(after.waitForExistence(timeout: 15),
                      "after a trip to the background the terminal stopped updating")
        attach("7-after-background")
    }

    /// `PasteButton` is drawn by the system; its identifier does not always make
    /// it through, its title does.
    private var pasteButton: XCUIElement {
        let byId = app.buttons["paste-pairing-link"]
        return byId.exists ? byId : app.buttons["Paste"].firstMatch
    }

    private func attach(_ name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
