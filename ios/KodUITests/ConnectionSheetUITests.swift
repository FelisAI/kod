//  ConnectionSheetUITests.swift — the toolbar button commits.
//
//  This is the only kind of test that could have caught the bug it pins. The
//  sheet's trailing button was `Button("Done") { dismiss() }` — the position iOS
//  has meant "commit" since 2007, wired to a pure dismissal — while the only
//  writer sat at the BOTTOM of a scroll view, under three fields and a paragraph,
//  i.e. off-screen with a keyboard up. So a typed address and token were silently
//  thrown away and the app went on dialling the old ones, with nothing anywhere
//  saying so.
//
//  Every pure test in KodTests passed throughout: the settings type was right, the
//  store was right, the parser was right. The defect lived in one line of wiring
//  between a button and a function, which is exactly the seam a unit test cannot
//  cross.
//
//  It needs NO bridge — it never points anywhere but loopback, and it does not
//  care what answers. Which is why it runs where `RemoteFlowUITests` skips.
//
//  IT ALSO STARTS FROM WHATEVER IT FINDS. The target shares one real UserDefaults
//  with its neighbour and with every previous run, so nothing here may assume an
//  unconfigured phone: the first draft did, passed once, and then failed on its
//  own leftovers — the values it typed were already there, so "edited" was false
//  and the button it was looking for never appeared.

import XCTest

final class ConnectionSheetUITests: XCTestCase {
    private var app: XCUIApplication!

    /// An empty SwiftUI text field reports its PLACEHOLDER as its value, so every
    /// read has to know which string means "nothing here". Without this the suite
    /// read a blank host as "192.168.1.20", concluded the form was complete,
    /// tapped a Save that was correctly disabled, and passed — on a sheet that had
    /// never closed.
    private enum Placeholder {
        static let host = "192.168.1.20"
        static let token = "paste KOD_BRIDGE_TOKEN"
    }

    /// What this simulator was configured with, put back on the way out — a test
    /// that left the app pointed somewhere else would break its neighbour rather
    /// than itself.
    private var originalHost: String?

    override func setUpWithError() throws {
        continueAfterFailure = false
        app = XCUIApplication()
        app.launch()
    }

    /// Type, tap the toolbar button, and ask whether it stuck.
    func testTheToolbarButtonSavesAndCancelDiscards() throws {
        openSheet()
        originalHost = text(of: hostField, placeholder: Placeholder.host)
        addTeardownBlock { [weak self] in self?.restore() }

        // SETUP, not assertion: a complete form. An incomplete one cannot be saved
        // by any route, so every assertion below would pass without proving
        // anything. Loopback, because 127.x is the one address this app will hold
        // without a pinned key — and it never has to answer.
        if text(of: hostField, placeholder: Placeholder.host).isEmpty {
            set(hostField, to: "127.0.0.1")
        }
        if text(of: tokenField, placeholder: Placeholder.token).isEmpty {
            set(tokenField, to: String(repeating: "0123456789abcdef", count: 4))
        }
        commitIfEdited()

        // A clean sheet: nothing to save, nothing to cancel.
        XCTAssertTrue(app.buttons["Done"].exists,
                      "with nothing edited the trailing button must read Done")
        XCTAssertFalse(app.buttons["Cancel"].exists, "nothing edited, so nothing to cancel")

        // Both spellings are loopback, so the app's state is the same either way
        // and only the SAVING is under test.
        let host0 = text(of: hostField, placeholder: Placeholder.host)
        XCTAssertFalse(host0.isEmpty, "setup failed: the form is still incomplete")
        let host1 = host0 == "127.0.0.1" ? "127.0.0.2" : "127.0.0.1"

        // 1. CANCEL DISCARDS.
        set(hostField, to: host1)
        XCTAssertTrue(app.buttons["Cancel"].exists,
                      "an edited sheet must offer a way to discard on purpose")
        XCTAssertFalse(app.buttons["Done"].exists,
                       "it must not still read Done — that is the word that lied")
        app.buttons["Cancel"].tap()
        XCTAssertTrue(sheetClosed(), "Cancel must close the sheet")
        openSheet()
        XCTAssertEqual(text(of: hostField, placeholder: Placeholder.host), host0,
                       "Cancel wrote the edit anyway")

        // 2. THE BUG: the toolbar button used to read "Done" and discard this.
        set(hostField, to: host1)
        XCTAssertTrue(app.buttons["Save"].isEnabled, "a complete, edited form must be savable")
        app.buttons["Save"].tap()
        XCTAssertTrue(sheetClosed(), "Save must close the sheet")
        openSheet()
        XCTAssertEqual(text(of: hostField, placeholder: Placeholder.host), host1,
                       "the toolbar button dismissed without saving — the original bug")

        // 3. And an unedited sheet still closes on Done.
        XCTAssertTrue(app.buttons["Done"].exists)
        app.buttons["Done"].tap()
        XCTAssertTrue(sheetClosed(), "Done must still close the sheet")
    }

    // MARK: - Driving the sheet

    private var hostField: XCUIElement { app.textFields["bridge-host"] }
    private var portField: XCUIElement { app.textFields["bridge-port"] }
    private var tokenField: XCUIElement { app.secureTextFields["bridge-token"] }

    private func openSheet(line: UInt = #line) {
        let chip = app.buttons["connection-chip"].firstMatch
        XCTAssertTrue(chip.waitForExistence(timeout: 10),
                      "no connection chip to open the sheet with", line: line)
        chip.tap()
        XCTAssertTrue(portField.waitForExistence(timeout: 5), "the sheet did not open", line: line)
    }

    /// Save if there is anything to save, then come back to a clean sheet. Used
    /// only to reach a known starting state.
    private func commitIfEdited() {
        guard app.buttons["Save"].exists else { return }
        XCTAssertTrue(app.buttons["Save"].isEnabled, "setup left the form unsavable")
        app.buttons["Save"].tap()
        XCTAssertTrue(sheetClosed())
        openSheet()
    }

    /// The sheet is gone. Asserted rather than assumed: a disabled Save leaves it
    /// up, and every later "reopen" then reads the sheet that never closed.
    private func sheetClosed() -> Bool {
        let gone = expectation(for: NSPredicate(format: "exists == false"),
                               evaluatedWith: portField)
        return XCTWaiter().wait(for: [gone], timeout: 5) == .completed
    }

    private func text(of field: XCUIElement, placeholder: String?) -> String {
        let v = (field.value as? String) ?? ""
        return v == placeholder ? "" : v
    }

    private func set(_ field: XCUIElement, to value: String) {
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.tap()
        // Deleting by length rather than by select-all: a number pad has no
        // selection gesture. Over-deleting an empty field is a no-op.
        let current = (field.value as? String) ?? ""
        field.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: current.count))
        field.typeText(value)
    }

    /// Best effort, and it also runs after a failure — when the app may be in any
    /// state at all.
    private func restore() {
        guard let originalHost, !originalHost.isEmpty else { return }
        if !portField.exists {
            let chip = app.buttons["connection-chip"].firstMatch
            guard chip.waitForExistence(timeout: 5) else { return }
            chip.tap()
        }
        guard portField.waitForExistence(timeout: 5) else { return }
        if text(of: hostField, placeholder: Placeholder.host) != originalHost {
            set(hostField, to: originalHost)
        }
        if app.buttons["Save"].exists, app.buttons["Save"].isEnabled {
            app.buttons["Save"].tap()
        } else if app.buttons["Cancel"].exists {
            app.buttons["Cancel"].tap()
        } else if app.buttons["Done"].exists {
            app.buttons["Done"].tap()
        }
    }
}
