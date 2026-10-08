//  FrozenLinkUITests.swift — a Mac that stops answering is noticed.
//
//  The case TCP hides: the bridge process is frozen (or the Mac asleep, or the
//  phone on a network that blackholes), the socket stays open, and nothing ever
//  arrives. The phone's idle deadline is the only thing that can tell, and it
//  used to be decorative — URLSession's async receive ignores task cancellation,
//  so the timer's win waited on a receive that never came, and the chip said
//  "live" for as long as the freeze lasted.
//
//  Needs a host-side harness, so it is opt-in: pair the simulator with a SANDBOX
//  bridge first (EndToEndUITests does), start this test with
//  TEST_RUNNER_KOD_FREEZE_TEST=1, and `kill -STOP` the sandbox bridge once the
//  test has seen "live". `kill -CONT` it afterwards.

import XCTest

final class FrozenLinkUITests: XCTestCase {
    func testAFrozenMacIsNoticedWithinTheIdleDeadline() throws {
        guard ProcessInfo.processInfo.environment["KOD_FREEZE_TEST"] == "1" else {
            throw XCTSkip("opt-in: needs a sandbox bridge the harness can freeze")
        }
        let app = XCUIApplication()
        app.launch()
        let live = app.buttons["live"]
        XCTAssertTrue(live.waitForExistence(timeout: 20), "not paired with a sandbox bridge")
        // The harness freezes the bridge now. 45 s idle deadline + the hello
        // deadline of the redial + slack.
        let noticed = NSPredicate(format: "exists == false")
        let window = Double(ProcessInfo.processInfo.environment["KOD_FREEZE_WAIT"] ?? "") ?? 75
        let wait = XCTWaiter().wait(for: [expectation(for: noticed, evaluatedWith: live)], timeout: window)
        XCTAssertEqual(wait, .completed, "a frozen Mac still reads as live after \(Int(window)) s")
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = "frozen-noticed"
        shot.lifetime = .keepAlways
        add(shot)
    }
}
