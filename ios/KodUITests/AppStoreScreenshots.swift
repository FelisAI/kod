//  AppStoreScreenshots.swift — the product page's screenshots, reproducibly.
//
//  Opt-in (TEST_RUNNER_KOD_SCREENSHOTS=1), because it is a camera rather than a
//  test. It runs the DEBUG sample data with `-kod-screenshots`, which hides the
//  "sample data" banner so the shots show the app as a paired phone shows it.
//  Run it on the simulator whose size App Store Connect asks for, then export the
//  attachments from the result bundle:
//
//      TEST_RUNNER_KOD_SCREENSHOTS=1 xcodebuild test -scheme KodUITests \
//        -only-testing:KodUITests/AppStoreScreenshots \
//        -destination 'platform=iOS Simulator,name=iPhone 17 Pro' -resultBundlePath shots.xcresult
//      xcrun xcresulttool export attachments --path shots.xcresult --output-path shots/

import XCTest

final class AppStoreScreenshots: XCTestCase {
    func testCaptureTheProductPage() throws {
        guard ProcessInfo.processInfo.environment["KOD_SCREENSHOTS"] == "1" else {
            throw XCTSkip("opt-in: set TEST_RUNNER_KOD_SCREENSHOTS=1")
        }
        continueAfterFailure = false
        let app = XCUIApplication()
        app.launchArguments = ["-kod-demo", "-kod-screenshots"]
        app.launch()

        // 1. Standup: who needs you, across every project.
        XCTAssertTrue(app.staticTexts["2 need you"].waitForExistence(timeout: 10)
                      || app.staticTexts["3 need you"].exists)
        shoot(app, "1-standup")

        // 2. The agent that has waited longest, and the screen it is waiting on.
        app.buttons.matching(NSPredicate(format: "label CONTAINS 'Move checkout'")).firstMatch.tap()
        XCTAssertTrue(app.staticTexts["TERMINAL"].waitForExistence(timeout: 5))
        shoot(app, "2-session-waiting")

        // 3. Every project, and what each one's sessions are doing.
        app.tabBars.firstMatch.buttons["Projects"].tap()
        let storefront = app.buttons.matching(NSPredicate(format: "label CONTAINS 'storefront'")).firstMatch
        XCTAssertTrue(storefront.waitForExistence(timeout: 5))
        storefront.tap()
        shoot(app, "3-projects")

        // 4. Answering one: type, send, and it goes back to work.
        app.tabBars.firstMatch.buttons["Standup"].tap()
        app.buttons.matching(NSPredicate(format: "label CONTAINS 'flaky webhook'")).firstMatch.tap()
        let field = app.textFields.matching(NSPredicate(format: "placeholderValue BEGINSWITH 'answer'")).firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.tap()
        field.typeText("Cap it at 30s, as the docs say, and fix the test clock.")
        app.buttons["composer-send"].tap()
        XCTAssertTrue(app.staticTexts["YOU SENT"].waitForExistence(timeout: 5))
        app.buttons["Done"].firstMatch.tap()
        sleep(1)
        shoot(app, "4-session-answered")
    }

    private func shoot(_ app: XCUIApplication, _ name: String) {
        let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
