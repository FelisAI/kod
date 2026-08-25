//  StoreTests.swift — the two cache rules that keep the phone honest:
//  epoch flushes everything, and a stale rev never wins.

import XCTest
@testable import Kod

final class StoreTests: XCTestCase {
    private func s(_ sid: UInt64, phase: Phase = .busy, title: String = "t") -> Session {
        Session(sid: sid, cli: .claude, project: "p", title: title, phase: phase,
                phaseSince: 1000, alive: true, lastMessage: "", pendingHeadline: nil,
                trouble: nil, limitHit: false, limitPercent: nil, limitReset: nil)
    }

    func testSnapshotReplacesRatherThanMerges() {
        var store = SessionStore()
        store.apply(.sessions(epoch: "e1", sessions: [s(1), s(2)]))
        store.apply(.sessions(epoch: "e1", sessions: [s(2), s(3)]))
        XCTAssertEqual(store.all.map(\.sid), [2, 3], "sid 1 was not in the new snapshot and must not survive it")
        XCTAssertTrue(store.hasSnapshot)
    }

    func testNewEpochFlushesEverything() {
        var store = SessionStore()
        store.apply(.sessions(epoch: "e1", sessions: [s(1), s(2)]))
        store.apply(.session(epoch: "e2", rev: 1, session: s(9)))
        XCTAssertEqual(store.all.map(\.sid), [9], "an epoch change means the old cache is fiction")
        XCTAssertEqual(store.epoch, "e2")
        XCTAssertFalse(store.hasSnapshot, "the new epoch has not sent its snapshot yet")
    }

    func testStaleRevIsDropped() {
        var store = SessionStore()
        store.apply(.sessions(epoch: "e1", sessions: []))
        store.apply(.session(epoch: "e1", rev: 5, session: s(1, phase: .awaiting)))
        store.apply(.session(epoch: "e1", rev: 4, session: s(1, phase: .idle)))
        XCTAssertEqual(store[1]?.phase, .awaiting, "rev 4 arrived after rev 5 and must lose")

        store.apply(.session(epoch: "e1", rev: 5, session: s(1, phase: .idle)))
        XCTAssertEqual(store[1]?.phase, .awaiting, "equal rev is not greater — it must also lose")

        store.apply(.session(epoch: "e1", rev: 6, session: s(1, phase: .idle)))
        XCTAssertEqual(store[1]?.phase, .idle)
    }

    func testRevsAreForgottenAcrossSnapshotsAndEpochs() {
        var store = SessionStore()
        store.apply(.session(epoch: "e1", rev: 99, session: s(1, phase: .busy)))
        store.apply(.sessions(epoch: "e1", sessions: [s(1, phase: .idle)]))
        // A fresh snapshot resets the rev ladder; rev 1 after it is NEW, not stale.
        store.apply(.session(epoch: "e1", rev: 1, session: s(1, phase: .awaiting)))
        XCTAssertEqual(store[1]?.phase, .awaiting)
    }

    func testGoneRemoves() {
        var store = SessionStore()
        store.apply(.sessions(epoch: "e1", sessions: [s(1), s(2)]))
        store.apply(.gone(epoch: "e1", sid: 1))
        XCTAssertEqual(store.all.map(\.sid), [2])
    }

    func testNonStateMessagesChangeNothing() {
        var store = SessionStore()
        store.apply(.sessions(epoch: "e1", sessions: [s(1)]))
        let before = store
        store.apply(.pong)
        store.apply(.err(code: "x", message: "y"))
        store.apply(.ignored(t: "future"))
        store.apply(.helloErr(code: "unauthorized", message: "no"))
        XCTAssertEqual(store, before)
    }

    func testFlushClearsEpochSoAReattachIsAlwaysFresh() {
        var store = SessionStore()
        store.apply(.sessions(epoch: "e1", sessions: [s(1)]))
        store.flush()
        XCTAssertTrue(store.all.isEmpty)
        XCTAssertNil(store.epoch)
        // Same epoch string coming back must still land, not be swallowed by adopt().
        store.apply(.sessions(epoch: "e1", sessions: [s(1)]))
        XCTAssertEqual(store.all.count, 1)
    }
}
