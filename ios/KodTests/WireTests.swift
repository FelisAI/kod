//  WireTests.swift — the contract, asserted.
//
//  PURE: no socket, no simulator services, no files. Every one of these runs
//  against a string literal, which is the only honest way to test a protocol
//  whose real server can only be reached through the user's live daemon.

import XCTest
@testable import Kod

final class WireTests: XCTestCase {
    private let sessionJSON = """
    {"sid":7,"cli":"codex","project":"orchestrator","title":"bridge listener",
     "phase":"awaiting","phase_since":1700000000000,"alive":true,
     "last_message":"pick one","pending_headline":"Bind to 0.0.0.0?","trouble":null,
     "limit_hit":false,"limit_percent":null,"limit_reset":null}
    """

    func testHelloOkParses() throws {
        let msg = try Wire.parse(frame: #"{"t":"hello_ok","proto":1,"epoch":"e1","server_time":1700000000000,"caps":{"input":false}}"#)
        guard case .helloOk(let proto, let epoch, let time, let input) = msg else {
            return XCTFail("expected hello_ok, got \(msg)")
        }
        XCTAssertEqual(proto, 1)
        XCTAssertEqual(epoch, "e1")
        XCTAssertEqual(time, 1_700_000_000_000)
        XCTAssertFalse(input, "v0 is read-only; caps.input must never decode as true from a false wire value")
    }

    func testSessionFieldsMapExactly() throws {
        let msg = try Wire.parse(frame: #"{"t":"session","epoch":"e1","rev":4,"session":\#(sessionJSON)}"#)
        guard case .session(let epoch, let rev, let s) = msg else { return XCTFail("expected session") }
        XCTAssertEqual(epoch, "e1")
        XCTAssertEqual(rev, 4)
        XCTAssertEqual(s.sid, 7)
        XCTAssertEqual(s.cli, .codex)
        XCTAssertEqual(s.project, "orchestrator")
        XCTAssertEqual(s.phase, .awaiting)
        XCTAssertEqual(s.phaseSince, 1_700_000_000_000)
        XCTAssertTrue(s.alive)
        XCTAssertEqual(s.lastMessage, "pick one")
        XCTAssertEqual(s.pendingHeadline, "Bind to 0.0.0.0?")
        XCTAssertNil(s.trouble)
        XCTAssertFalse(s.limitHit)
        XCTAssertNil(s.limitPercent)
    }

    func testUnknownFieldsAreIgnored() throws {
        let frame = #"{"t":"gone","epoch":"e1","sid":3,"future_field":{"nested":[1,2,3]},"another":true}"#
        XCTAssertEqual(try Wire.parse(frame: frame), .gone(epoch: "e1", sid: 3))
    }

    func testUnknownTypeIsIgnoredNotFatal() throws {
        XCTAssertEqual(try Wire.parse(frame: #"{"t":"grid","sid":1}"#), .ignored(t: "grid"))
    }

    func testFrameLimitRejectedBeforeParsing() {
        // Valid JSON, one byte too big: it must fail on SIZE, not on content.
        let filler = String(repeating: "x", count: kMaxFrameBytes)
        let frame = #"{"t":"err","code":"x","message":"\#(filler)"}"#
        XCTAssertGreaterThan(frame.utf8.count, kMaxFrameBytes)
        XCTAssertThrowsError(try Wire.parse(frame: frame)) { error in
            guard case WireError.frameTooLarge = error else {
                return XCTFail("expected frameTooLarge, got \(error)")
            }
        }
    }

    func testFrameAtExactlyTheLimitIsAccepted() throws {
        let overhead = #"{"t":"err","code":"x","message":""}"#.utf8.count
        let filler = String(repeating: "x", count: kMaxFrameBytes - overhead)
        let frame = #"{"t":"err","code":"x","message":"\#(filler)"}"#
        XCTAssertEqual(frame.utf8.count, kMaxFrameBytes)
        XCTAssertEqual(try Wire.parse(frame: frame), .err(code: "x", message: filler))
    }

    func testUnknownEnumValuesDegradeInsteadOfThrowing() throws {
        let frame = #"{"t":"session","epoch":"e","rev":1,"session":{"sid":1,"cli":"aider","phase":"meditating","project":"p","title":"t","phase_since":1,"alive":true,"last_message":"","pending_headline":null,"trouble":null,"limit_hit":false,"limit_percent":null,"limit_reset":null}}"#
        guard case .session(_, _, let s) = try Wire.parse(frame: frame) else { return XCTFail("expected session") }
        XCTAssertEqual(s.cli, .unknown)
        XCTAssertEqual(s.phase, .unknown)
    }

    func testEmptyStringsNormaliseToNil() throws {
        let frame = #"{"t":"session","epoch":"e","rev":1,"session":{"sid":1,"cli":"claude","phase":"idle","project":"p","title":"","phase_since":1,"alive":true,"last_message":"","pending_headline":"","trouble":"","limit_hit":false}}"#
        guard case .session(_, _, let s) = try Wire.parse(frame: frame) else { return XCTFail("expected session") }
        XCTAssertNil(s.pendingHeadline)
        XCTAssertNil(s.trouble)
        XCTAssertEqual(s.displayTitle, "session 1", "an empty title still has to be tappable")
    }

    func testHelloEncodesExactlyThreeFields() throws {
        let json = ClientMessage.hello(token: "s3cr3t").json
        XCTAssertEqual(json, #"{"t":"hello","proto":1,"token":"s3cr3t"}"#)
        XCTAssertEqual(ClientMessage.ping.json, #"{"t":"ping"}"#)
    }

    func testTokenWithQuotesIsEscaped() throws {
        let json = ClientMessage.hello(token: #"a"b\c"#).json
        let parsed = try JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any]
        XCTAssertEqual(parsed?["token"] as? String, #"a"b\c"#)
    }
}
