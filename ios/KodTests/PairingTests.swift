//  PairingTests.swift — the pairing payload contract, asserted.
//
//  PURE: a string in, a Result out. No camera, no simulator services — which is
//  the only way to test the scanner's rejection paths at all, since the Simulator
//  has no camera to point at a bad QR code.
//
//  Table-driven because the interesting part is the SET of inputs: every row is a
//  code a phone could plausibly be shown, and each one must land on exactly one
//  outcome. Rows carry #line so a failure points at the row, not at the loop.

import XCTest
@testable import Kod

final class PairingTests: XCTestCase {
    /// 64 lowercase hex — the only shape a real KOD_BRIDGE_TOKEN has.
    private static let token = String(repeating: "0123456789abcdef", count: 4)

    private struct Accept {
        let why: String
        let input: String
        let want: BridgeSettings
        let line: UInt

        init(_ why: String, _ input: String, _ want: BridgeSettings, line: UInt = #line) {
            self.why = why
            self.input = input
            self.want = want
            self.line = line
        }
    }

    private struct Reject {
        let why: String
        let input: String
        let want: PairingError
        let line: UInt

        init(_ why: String, _ input: String, _ want: PairingError, line: UInt = #line) {
            self.why = why
            self.input = input
            self.want = want
            self.line = line
        }
    }

    func testAcceptedCodes() {
        let t = Self.token
        let want = BridgeSettings(host: "100.101.102.103", port: 8787, token: t)

        let rows: [Accept] = [
            Accept("the exact payload the Mac emits",
                   "kod://pair?h=100.101.102.103&p=8787&t=\(t)", want),

            Accept("a query is a set, not a tuple — order must not matter",
                   "kod://pair?t=\(t)&p=8787&h=100.101.102.103", want),

            Accept("unknown params are ignored so the Mac can add fields later",
                   "kod://pair?h=100.101.102.103&v=2&p=8787&name=Studio%20Mac&t=\(t)&fp=abc", want),

            Accept("a QR read out of a text file arrives with a trailing newline",
                   "  \n kod://pair?h=100.101.102.103&p=8787&t=\(t)\n\n", want),

            Accept("whitespace INSIDE fields, mid-payload, is trimmed — the trailing "
                   + "space on a pasted token used to surface as 'unauthorized'",
                   "kod://pair?h= 100.101.102.103 & p= 8787 &t= \(t) &x=1", want),

            Accept("percent-encoded whitespace is the same whitespace",
                   "kod://pair?h=100.101.102.103&p=8787&t=%20\(t)%20&x=1", want),

            Accept("scheme and authority are case-insensitive per RFC 3986",
                   "KOD://PAIR?h=100.101.102.103&p=8787&t=\(t)", want),

            Accept("a URL normaliser on the Mac may insert the authority's slash",
                   "kod://pair/?h=100.101.102.103&p=8787&t=\(t)", want),

            Accept("first occurrence of a duplicated key wins, deterministically",
                   "kod://pair?h=100.101.102.103&h=10.0.0.9&p=8787&t=\(t)", want),

            Accept("port 1 is the low boundary and is legal",
                   "kod://pair?h=100.101.102.103&p=1&t=\(t)",
                   BridgeSettings(host: "100.101.102.103", port: 1, token: t)),

            Accept("port 65535 is the high boundary and is legal",
                   "kod://pair?h=100.101.102.103&p=65535&t=\(t)",
                   BridgeSettings(host: "100.101.102.103", port: 65_535, token: t)),

            Accept("the host is not required to be an IPv4 literal — MagicDNS and "
                   + ".local names must keep working",
                   "kod://pair?h=studio.local&p=8787&t=\(t)",
                   BridgeSettings(host: "studio.local", port: 8787, token: t)),
        ]

        for row in rows {
            switch Pairing.parse(row.input) {
            case .success(let got):
                XCTAssertEqual(got, row.want, row.why, file: #filePath, line: row.line)
            case .failure(let err):
                XCTFail("\(row.why) — rejected as \(err)", file: #filePath, line: row.line)
            }
        }
    }

    func testRejectedCodes() {
        let t = Self.token

        let rows: [Reject] = [
            Reject("nothing scanned", "", .empty),
            Reject("whitespace only is still nothing", "  \n\t ", .empty),

            Reject("a website QR", "https://example.com/pair?h=1.2.3.4&p=8787&t=\(t)", .notAPairingCode),
            Reject("plain text on a poster", "join wifi: guest", .notAPairingCode),
            Reject("a prefix match is not a scheme match",
                   "kod://pairing?h=1.2.3.4&p=8787&t=\(t)", .notAPairingCode),
            Reject("some other kod:// action is not pairing",
                   "kod://connect?h=1.2.3.4&p=8787&t=\(t)", .notAPairingCode),

            Reject("no host", "kod://pair?p=8787&t=\(t)", .missingField(.host)),
            Reject("no port", "kod://pair?h=1.2.3.4&t=\(t)", .missingField(.port)),
            Reject("no token", "kod://pair?h=1.2.3.4&p=8787", .missingField(.token)),
            Reject("present but blank is missing", "kod://pair?h=1.2.3.4&p=8787&t=", .missingField(.token)),
            Reject("blank after trimming is also missing",
                   "kod://pair?h=1.2.3.4&p=8787&t=%20%20&x=1", .missingField(.token)),
            Reject("a bare kod://pair is ours, just empty — say WHICH field is gone",
                   "kod://pair", .missingField(.host)),

            Reject("the contract says lowercase; an uppercase token would be rejected "
                   + "by the bridge as a bare 'unauthorized'",
                   "kod://pair?h=1.2.3.4&p=8787&t=\(t.uppercased())", .tokenNotHex),
            Reject("g is not a hex digit",
                   "kod://pair?h=1.2.3.4&p=8787&t=\(t.dropLast())g", .tokenNotHex),
            Reject("Character.isHexDigit accepts fullwidth digits; 32 raw bytes do not",
                   "kod://pair?h=1.2.3.4&p=8787&t=\(t.dropLast())\u{FF11}", .tokenNotHex),
            Reject("one character short",
                   "kod://pair?h=1.2.3.4&p=8787&t=\(t.dropLast())", .tokenWrongLength(63)),
            Reject("one character long",
                   "kod://pair?h=1.2.3.4&p=8787&t=\(t)a", .tokenWrongLength(65)),

            Reject("port 0 is not a port", "kod://pair?h=1.2.3.4&p=0&t=\(t)", .badPort("0")),
            Reject("port 70000 does not fit in 16 bits",
                   "kod://pair?h=1.2.3.4&p=70000&t=\(t)", .badPort("70000")),
            Reject("negative", "kod://pair?h=1.2.3.4&p=-1&t=\(t)", .badPort("-1")),
            Reject("not a number at all", "kod://pair?h=1.2.3.4&p=87a7&t=\(t)", .badPort("87a7")),
        ]

        for row in rows {
            switch Pairing.parse(row.input) {
            case .success(let got):
                XCTFail("\(row.why) — accepted as \(got)", file: #filePath, line: row.line)
            case .failure(let err):
                XCTAssertEqual(err, row.want, row.why, file: #filePath, line: row.line)
            }
        }
    }

    /// The whole point of pairing is that a scan produces something dialable. If
    /// the parser ever returned settings the client cannot use, the user would see
    /// "no bridge configured" straight after a successful scan.
    func testAcceptedCodeIsImmediatelyUsable() {
        guard case .success(let s) = Pairing.parse("kod://pair?h=100.101.102.103&p=8787&t=\(Self.token)") else {
            return XCTFail("the canonical payload must parse")
        }
        XCTAssertTrue(s.isUsable)
        XCTAssertEqual(s.url?.absoluteString, "ws://100.101.102.103:8787/")
    }

    /// A scanner that says "invalid code" for everything sends the user back to
    /// typing 64 characters by hand. Every reason must read differently.
    func testEveryRejectionReasonSaysSomethingDifferent() {
        let all: [PairingError] = [
            .empty, .notAPairingCode,
            .missingField(.host), .missingField(.port), .missingField(.token),
            .tokenNotHex, .tokenWrongLength(63), .tokenWrongLength(65),
            .badPort("0"), .badPort("70000"),
        ]
        let messages = all.map(\.message)
        XCTAssertTrue(messages.allSatisfy { !$0.isEmpty })
        XCTAssertEqual(Set(messages).count, all.count, "two rejections share a message: \(messages)")
    }
}
