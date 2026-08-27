//  WireTests.swift — the contract, asserted.
//
//  PURE: no socket, no simulator services, no files. Every one of these runs
//  against a string literal, which is the only honest way to test a protocol
//  whose real server can only be reached through the user's live daemon.
//
//  `ComposerTests` at the bottom is here for the same reason: the rule that the
//  box is emptied only on an acceptance is a rule about what the wire ANSWERED,
//  and `Composer` is a pure value, so it is tested the same way — no view, no
//  socket, no timing.

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
        XCTAssertFalse(input, "a Mac that says it takes no typing must never decode as one that does")
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
        // proto 2: a bridge that takes typing answers `bad_proto` to a 1, so this
        // number is the difference between a composer and a phone that cannot
        // even connect.
        XCTAssertEqual(json, #"{"t":"hello","proto":2,"token":"s3cr3t"}"#)
        XCTAssertEqual(ClientMessage.ping.json, #"{"t":"ping"}"#)
    }

    func testTokenWithQuotesIsEscaped() throws {
        let json = ClientMessage.hello(token: #"a"b\c"#).json
        let parsed = try JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any]
        XCTAssertEqual(parsed?["token"] as? String, #"a"b\c"#)
    }

    // MARK: - Typing back

    func testCanInputDefaultsToFalseWhenTheMacDoesNotSendIt() throws {
        // The Mac that omits the field is the Mac that would refuse the input.
        // Decoding it as true would grow a composer that cannot work.
        guard case .session(_, _, let s) = try Wire.parse(frame: #"{"t":"session","epoch":"e","rev":1,"session":\#(sessionJSON)}"#) else {
            return XCTFail("expected session")
        }
        XCTAssertFalse(s.canInput)
    }

    func testCanInputDecodesWhenPresent() throws {
        let frame = #"{"t":"session","epoch":"e","rev":1,"session":{"sid":1,"cli":"claude","project":"p","title":"t","phase":"awaiting","phase_since":1,"alive":true,"last_message":"","pending_headline":null,"trouble":null,"limit_hit":false,"limit_percent":null,"limit_reset":null,"can_input":true}}"#
        guard case .session(_, _, let s) = try Wire.parse(frame: frame) else { return XCTFail("expected session") }
        XCTAssertTrue(s.canInput)
    }

    func testInputEncodesSidAndText() {
        // A wrong field name here is answered with `err` and looks, on the phone,
        // exactly like nothing happening — so it is pinned character for character.
        XCTAssertEqual(ClientMessage.input(sid: 7, text: "yes, go ahead", rid: 4).json,
                       #"{"t":"input","sid":7,"text":"yes, go ahead","rid":4}"#)
    }

    func testKeyEncodesSidAndKeyName() {
        XCTAssertEqual(ClientMessage.key(sid: 3, key: .enter, rid: 9).json, #"{"t":"key","sid":3,"key":"enter","rid":9}"#)
        XCTAssertEqual(ClientMessage.key(sid: 3, key: .escape, rid: 9).json, #"{"t":"key","sid":3,"key":"escape","rid":9}"#)
        XCTAssertEqual(ClientMessage.key(sid: 3, key: .up, rid: 9).json, #"{"t":"key","sid":3,"key":"up","rid":9}"#)
        XCTAssertEqual(ClientMessage.key(sid: 3, key: .down, rid: 9).json, #"{"t":"key","sid":3,"key":"down","rid":9}"#)
        XCTAssertEqual(ClientMessage.key(sid: 3, key: .tab, rid: 9).json, #"{"t":"key","sid":3,"key":"tab","rid":9}"#)
        // Every key the daemon accepts is spelled above; a new one must be spelled
        // here before it can ship.
        XCTAssertEqual(PhoneKey.allCases.map(\.rawValue), ["enter", "escape", "up", "down", "tab"])
    }

    func testInputTextIsEscaped() throws {
        // A multi-line answer with a quote in it is ordinary typing, not an edge
        // case: unescaped it would be a bad_json refusal of the user's own words.
        let typed = "line one\nsaid \"no\"\tthen \\ stopped"
        let json = ClientMessage.input(sid: 1, text: typed, rid: 1).json
        let parsed = try JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any]
        XCTAssertEqual(parsed?["text"] as? String, typed)
        XCTAssertEqual(parsed?["sid"] as? UInt64, 1)
    }

    func testRefusalDecodesWithItsReason() throws {
        let frame = #"{"t":"input_result","sid":7,"ok":false,"reason":"Kod does not let a phone type into a shell"}"#
        XCTAssertEqual(Wire.inputResult(frame: frame),
                       InputResult(rid: 0, sid: 7, ok: false, message: "Kod does not let a phone type into a shell"))
    }

    func testAcceptanceDecodes() {
        // The bridge sends `reason: null` on an acceptance, which is no reason at
        // all — not the string "null", and not a failure.
        XCTAssertEqual(Wire.inputResult(frame: #"{"t":"input_result","sid":7,"ok":true,"reason":null}"#),
                       InputResult(rid: 0, sid: 7, ok: true, message: ""))
    }

    func testAResultThatDoesNotSayOkIsNotAnAcceptance() {
        // The composer empties the box on `ok`. A frame that lost the field, or a
        // bridge that never sent it, must not be read as "delivered".
        XCTAssertEqual(Wire.inputResult(frame: #"{"t":"input_result","sid":7}"#),
                       InputResult(rid: 0, sid: 7, ok: false, message: ""))
    }

    func testNoOtherFrameIsMistakenForAnAnswer() {
        // `gone` also carries a sid, so a shape-only check would answer the
        // composer with someone else's message and clear text nobody delivered.
        XCTAssertNil(Wire.inputResult(frame: #"{"t":"gone","epoch":"e1","sid":7}"#))
        XCTAssertNil(Wire.inputResult(frame: #"{"t":"pong"}"#))
        XCTAssertNil(Wire.inputResult(frame: "not json at all"))
    }

    func testAnAnswerNeverReachesTheSessionCache() {
        // The cache is a projection of the daemon's sessions; an ack of this
        // phone's own typing is not one, and must not flush or upsert anything.
        var store = SessionStore()
        store.apply(.sessions(epoch: "e1", sessions: []))
        let before = store
        // There is no `ServerMessage` case for it, by design — so even handed
        // straight to the cache it is an ignorable frame and changes nothing.
        let parsed = try? Wire.parse(frame: #"{"t":"input_result","sid":7,"ok":true}"#)
        XCTAssertEqual(parsed, .ignored(t: "input_result"))
        store.apply(parsed ?? .pong)
        XCTAssertEqual(store, before)
    }
}

/// The composer's one rule, from both sides: nothing is cleared that the Mac did
/// not confirm, and an accepted paste is followed by the Enter that submits it.
final class ComposerTests: XCTestCase {
    private func drafted(_ text: String, to sid: UInt64 = 7) -> (Composer, ClientMessage?) {
        var c = Composer()
        c.edit(text)
        let sent = c.send(to: sid)
        return (c, sent)
    }

    func testARefusalKeepsTheTextAndShowsTheReason() {
        var (c, sent) = drafted("yes, go ahead")
        XCTAssertEqual(sent?.json, #"{"t":"input","sid":7,"text":"yes, go ahead","rid":1}"#)
        XCTAssertTrue(c.busy)

        XCTAssertNil(c.settle(rid: c.inFlightRid, sid: 7, ok: false, message: "that session has ended"))
        XCTAssertEqual(c.text, "yes, go ahead", "text the Mac refused is not the user's to lose")
        XCTAssertEqual(c.failure, "that session has ended")
        XCTAssertFalse(c.busy)
    }

    func testAnAcceptedPasteIsFollowedByTheEnterThatSubmitsIt() {
        var (c, _) = drafted("ship it")
        // The daemon PASTES and stops, so without this Enter the line would sit in
        // the agent's prompt, typed and unsent.
        XCTAssertEqual(c.settle(rid: c.inFlightRid, sid: 7, ok: true, message: "")?.json,
                       #"{"t":"key","sid":7,"key":"enter","rid":2}"#)
        XCTAssertEqual(c.text, "")
        XCTAssertTrue(c.busy, "the submit is still on the wire")
        XCTAssertNil(c.settle(rid: c.inFlightRid, sid: 7, ok: true, message: ""))
        XCTAssertFalse(c.busy)
        XCTAssertNil(c.failure)
    }

    func testTextTypedWhileTheSendWasInFlightIsNotSwallowedByTheAck() {
        var (c, _) = drafted("first")
        c.edit("first, and one more thing")
        _ = c.settle(rid: c.inFlightRid, sid: 7, ok: true, message: "")
        XCTAssertEqual(c.text, "first, and one more thing",
                       "the ack confirmed the old text; what is in the box now was never sent")
    }

    func testAnAnswerForAnotherSessionIsIgnored() {
        var (c, _) = drafted("hi", to: 7)
        XCTAssertNil(c.settle(rid: c.inFlightRid, sid: 9, ok: false, message: "that session is gone"))
        XCTAssertTrue(c.busy)
        XCTAssertNil(c.failure, "a refusal aimed at another session must not blame this draft")
        XCTAssertEqual(c.text, "hi")
    }

    func testOneTapCannotBecomeTwoLines() {
        var (c, _) = drafted("hi")
        XCTAssertNil(c.send(to: 7))
        XCTAssertNil(c.press(.enter, on: 7))
    }

    func testAWhitespaceOnlyDraftIsNotSent() {
        var c = Composer()
        c.edit("   \n ")
        XCTAssertNil(c.send(to: 7))
        XCTAssertFalse(c.busy)
    }

    func testALostLinkFailsTheSendAndKeepsTheText() {
        var (c, _) = drafted("hi")
        c.fail("the link dropped before your Mac answered")
        XCTAssertFalse(c.busy)
        XCTAssertEqual(c.text, "hi")
        XCTAssertEqual(c.failure, "the link dropped before your Mac answered")
        // Nothing is in flight now, so a later failure has nothing to report and
        // must not overwrite the one the user is reading.
        c.fail("something else entirely")
        XCTAssertEqual(c.failure, "the link dropped before your Mac answered")
    }

    func testTypingClearsAStaleRefusal() {
        var (c, _) = drafted("hi")
        _ = c.settle(rid: c.inFlightRid, sid: 7, ok: false, message: "that session has ended")
        c.edit("hi there")
        XCTAssertNil(c.failure, "the reason was about text that is no longer in the box")
    }

    func testAControlKeyIsItsOwnRoundTrip() {
        var c = Composer()
        c.edit("a draft nobody sent")
        XCTAssertEqual(c.press(.down, on: 4)?.json, #"{"t":"key","sid":4,"key":"down","rid":1}"#)
        XCTAssertNil(c.settle(rid: c.inFlightRid, sid: 4, ok: true, message: ""))
        XCTAssertFalse(c.busy)
        XCTAssertEqual(c.text, "a draft nobody sent", "an arrow key is not a reason to empty the box")
    }

    /// THE reason `rid` exists.
    ///
    /// A phone backgrounded mid-send abandons that send; when it comes back the
    /// user sends again. If the first answer then arrives and is matched by
    /// session alone, it settles the SECOND send — and for a paste, settling
    /// means dispatching the Enter that submits it. The agent receives a submit
    /// against text that never landed.
    func testALateAnswerToAnAbandonedSendCannotSettleANewerOne() {
        var c = Composer()
        c.edit("first")
        let first = c.send(to: 7)
        XCTAssertNotNil(first)
        let staleRid = c.inFlightRid

        // The link dropped; the text is kept because it was never delivered.
        c.fail("the connection dropped")
        XCTAssertFalse(c.busy)
        XCTAssertEqual(c.text, "first")

        // The user edits and sends again.
        c.edit("second")
        XCTAssertNotNil(c.send(to: 7))
        let liveRid = c.inFlightRid
        XCTAssertNotEqual(staleRid, liveRid, "ids must never be reused")

        // The FIRST send's answer finally arrives.
        let follow = c.settle(rid: staleRid, sid: 7, ok: true, message: "")
        XCTAssertNil(follow, "a stale answer must not produce the Enter that submits")
        XCTAssertTrue(c.busy, "and must not settle the send that is actually in flight")
        XCTAssertEqual(c.text, "second", "nor empty a box it knows nothing about")

        // The real answer still works.
        XCTAssertNotNil(c.settle(rid: liveRid, sid: 7, ok: true, message: ""))
    }

    /// A send that is never answered must leave the composer USABLE again.
    ///
    /// Without this the spinner replaces the send button and every control key
    /// stays disabled for the life of the app, and the only way out — switching
    /// sessions — is also the one action that throws the draft away.
    func testAnUnansweredSendLeavesTheComposerRecoverable() {
        var c = Composer()
        c.edit("are you sure?")
        XCTAssertNotNil(c.send(to: 7))
        XCTAssertTrue(c.busy)
        XCTAssertFalse(c.canSend, "no second send while one is in flight")

        c.fail("your Mac did not answer. Your text is still here — try again.")

        XCTAssertFalse(c.busy)
        XCTAssertEqual(c.text, "are you sure?", "undelivered text is not the user's to lose")
        XCTAssertTrue(c.canSend, "the same draft must be sendable again")
        XCTAssertNotNil(c.failure, "and the reason must be there to read")
    }

    /// Typing after a failure clears the explanation — it described a send that
    /// is no longer the one in the box.
    func testEditingAfterAFailureClearsTheExplanation() {
        var c = Composer()
        c.edit("first")
        _ = c.send(to: 7)
        c.fail("the connection dropped")
        XCTAssertNotNil(c.failure)
        c.edit("first and more")
        XCTAssertNil(c.failure)
    }

    /// The echo must only appear for text the Mac ACCEPTED. Showing a refused or
    /// in-flight message as "you sent" would be the screen claiming something
    /// happened that did not.
    func testTheEchoOnlyRecordsAcceptedText() {
        var c = Composer()
        c.edit("run the tests")
        _ = c.send(to: 7)
        XCTAssertNil(c.delivered, "in flight is not delivered")

        c.fail("the connection dropped")
        XCTAssertNil(c.delivered, "a failed send is not delivered")

        c.edit("run the tests")
        _ = c.send(to: 7)
        _ = c.settle(rid: c.inFlightRid, sid: 7, ok: false, message: "refused")
        XCTAssertNil(c.delivered, "a refusal is not delivered")

        c.edit("run the tests")
        _ = c.send(to: 7)
        _ = c.settle(rid: c.inFlightRid, sid: 7, ok: true, message: "")
        XCTAssertEqual(c.delivered, "run the tests")
    }

    /// It belongs to one session.
    ///
    /// The app never carries a composer between sessions — `selectedSid`'s didSet
    /// replaces the whole struct — so this pins the property that makes that
    /// safe, rather than a cross-session sequence the app cannot produce. A first
    /// draft of this test drove one composer across two sids and failed for an
    /// unrelated reason: after an accepted paste the composer is still busy on the
    /// Enter that submits it.
    func testAFreshComposerCarriesNothingFromTheLastSession() {
        var c = Composer()
        c.edit("for seven")
        _ = c.send(to: 7)
        _ = c.settle(rid: c.inFlightRid, sid: 7, ok: true, message: "")
        XCTAssertEqual(c.delivered, "for seven")

        // What the model actually does when you open another session.
        let next = Composer(sid: 9)
        XCTAssertNil(next.delivered)
        XCTAssertEqual(next.text, "")
        XCTAssertNil(next.failure)
    }

    /// An accepted paste leaves the composer busy on the Enter that submits it —
    /// the property the test above tripped over, worth stating outright.
    func testAnAcceptedPasteIsStillBusyUntilItsEnterIsAcknowledged() {
        var c = Composer()
        c.edit("go")
        _ = c.send(to: 7)
        let follow = c.settle(rid: c.inFlightRid, sid: 7, ok: true, message: "")
        XCTAssertNotNil(follow, "the paste's acceptance produces the Enter")
        XCTAssertTrue(c.busy, "and the composer stays busy until that Enter is answered")
        _ = c.settle(rid: c.inFlightRid, sid: 7, ok: true, message: "")
        XCTAssertFalse(c.busy)
    }
}
