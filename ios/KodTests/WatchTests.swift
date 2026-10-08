//  WatchTests.swift — the terminal keeps coming after a reconnect.
//
//  The bridge keeps ONE watch per connection and forgets it when the connection
//  ends. The phone used to hold a single "watching" value that outlived the
//  connection it described, so after the first reconnect — or the first trip to
//  the background, which drops the link on purpose — it still read "already
//  watching", nothing told the new connection, and the terminal never updated
//  again. A watch asked for before `hello_ok` was lost the same way.

import XCTest
@testable import Kod

final class WatchTests: XCTestCase {
    func testAWatchWaitsForTheLinkInsteadOfBeingLost() {
        var w = WatchPlan()
        w.request(7)
        XCTAssertNil(w.next(linkUp: false), "written before hello_ok, the bridge would drop it")
        XCTAssertEqual(w.next(linkUp: true)?.json, ClientMessage.watch(sid: 7, on: true).json,
                       "and it must still go out once the link is up")
    }

    func testAskingAgainSendsNothing() {
        var w = WatchPlan()
        w.request(7)
        _ = w.next(linkUp: true)
        w.request(7)
        XCTAssertNil(w.next(linkUp: true), "SwiftUI re-runs onAppear; a watch per re-layout is noise")
    }

    /// THE REGRESSION.
    func testANewConnectionIsToldAgain() {
        var w = WatchPlan()
        w.request(7)
        _ = w.next(linkUp: true)

        w.linkEnded()
        XCTAssertNil(w.next(linkUp: false), "nothing to say while the link is down")
        XCTAssertEqual(w.next(linkUp: true)?.json, ClientMessage.watch(sid: 7, on: true).json,
                       "the new connection has no watch until it is told")
    }

    func testSwitchingSessionsSendsOnlyTheNewWatch() {
        var w = WatchPlan()
        w.request(7)
        _ = w.next(linkUp: true)
        w.request(9)
        XCTAssertEqual(w.next(linkUp: true)?.json, ClientMessage.watch(sid: 9, on: true).json,
                       "the bridge replaces the old watch itself")
    }

    func testLeavingTheScreenTurnsTheStreamOff() {
        var w = WatchPlan()
        w.request(7)
        _ = w.next(linkUp: true)
        w.request(nil)
        XCTAssertEqual(w.next(linkUp: true)?.json, ClientMessage.watch(sid: 7, on: false).json)
        XCTAssertNil(w.next(linkUp: true))
    }

    /// Nothing was ever streamed on this connection, so there is nothing to turn
    /// off — and an "off" for a session nobody watched is just noise.
    func testNoOffForAWatchThisConnectionNeverHad() {
        var w = WatchPlan()
        w.request(7)
        _ = w.next(linkUp: true)
        w.linkEnded()
        w.request(nil)
        XCTAssertNil(w.next(linkUp: true))
    }
}
