//  PairingTests.swift — the pairing payload contract, asserted.
//
//  PURE: a string in, a Result out. No camera, no simulator services — which is
//  the only way to test the scanner's rejection paths at all, since the Simulator
//  has no camera to point at a bad QR code.
//
//  Table-driven because the interesting part is the SET of inputs: every row is a
//  code a phone could plausibly be shown, and each one must land on exactly one
//  outcome. Rows carry #line so a failure points at the row, not at the loop.
//
//  `KeyPinTests`, at the bottom, is the other half: the code decides WHERE to
//  connect, the pin decides WHO answered.

import Security
import XCTest
@testable import Kod

final class PairingTests: XCTestCase {
    /// 64 lowercase hex — the only shape a real KOD_BRIDGE_TOKEN has.
    private static let token = String(repeating: "0123456789abcdef", count: 4)
    /// A real fingerprint: 43 base64url characters, the SHA-256 of an actual
    /// self-signed certificate's SPKI (the same one `KeyPinTests` pins).
    private static let pin = "H8C5F70XufKBxLhvttik7TqtBqqnYM1qRye3BXYOd1o"

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

            Accept("a key fingerprint is carried through verbatim — it is the only "
                   + "thing the phone will ever know about who answers",
                   "kod://pair?h=100.101.102.103&p=8787&t=\(t)&f=\(Self.pin)",
                   BridgeSettings(host: "100.101.102.103", port: 8787, token: t,
                                  fingerprint: Self.pin)),

            Accept("order still does not matter with f in the set",
                   "kod://pair?f=\(Self.pin)&t=\(t)&h=100.101.102.103&p=8787",
                   BridgeSettings(host: "100.101.102.103", port: 8787, token: t,
                                  fingerprint: Self.pin)),

            Accept("a fingerprint scanned with whitespace around it is the same "
                   + "fingerprint — a stray space would refuse the user's own Mac",
                   "kod://pair?h=100.101.102.103&p=8787&t=\(t)&f=%20\(Self.pin)%20",
                   BridgeSettings(host: "100.101.102.103", port: 8787, token: t,
                                  fingerprint: Self.pin)),
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

            // A fingerprint that is present but unusable must NEVER be treated as
            // absent: absent means plaintext, so ignoring it downgrades a TLS
            // pairing to an unpinned connection with nothing on screen to say so.
            Reject("f present but blank pins nothing",
                   "kod://pair?h=1.2.3.4&p=8787&t=\(t)&f=", .badFingerprint(blank: true)),
            Reject("f blank after trimming pins nothing either",
                   "kod://pair?h=1.2.3.4&p=8787&t=\(t)&f=%20%20", .badFingerprint(blank: true)),
            Reject("one character short of a 32-byte digest",
                   "kod://pair?h=1.2.3.4&p=8787&t=\(t)&f=\(Self.pin.dropLast())",
                   .badFingerprint(blank: false)),
            Reject("one character long",
                   "kod://pair?h=1.2.3.4&p=8787&t=\(t)&f=\(Self.pin)A",
                   .badFingerprint(blank: false)),
            Reject("standard base64, not base64url — the Mac never emits +",
                   "kod://pair?h=1.2.3.4&p=8787&t=\(t)&f=\(Self.pin.dropLast())%2B",
                   .badFingerprint(blank: false)),
            Reject("padding is not part of the spelling",
                   "kod://pair?h=1.2.3.4&p=8787&t=\(t)&f=\(Self.pin.dropLast())%3D",
                   .badFingerprint(blank: false)),
            Reject("a hex digest is the right key in the wrong encoding",
                   "kod://pair?h=1.2.3.4&p=8787&t=\(t)"
                   + "&f=1fc0b917ed17b9f281c4b86fb6d8a4ed3aad06aaa760cd6a4727b705760e775a",
                   .badFingerprint(blank: false)),
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
        guard case .success(let s) = Pairing.parse(
            "kod://pair?h=100.101.102.103&p=8787&t=\(Self.token)&f=\(Self.pin)") else {
            return XCTFail("the canonical payload must parse")
        }
        XCTAssertTrue(s.isUsable)
        XCTAssertEqual(s.url?.absoluteString, "wss://100.101.102.103:8787/")
        XCTAssertTrue(s.usesTLS)
    }

    /// PLAINTEXT IS ONLY EVER LEGAL ON LOOPBACK, and the phone enforces that for
    /// itself.
    ///
    /// The Mac refusing to bind a non-loopback address without TLS protects a
    /// correctly-configured Mac. It does not protect THIS phone, which is what
    /// holds the bearer token and what would put it on the wire. A pairing code
    /// with no `f=` naming any off-device address must therefore be unusable here,
    /// whatever produced it.
    func testAPinlessCodeForAnOffDeviceHostIsRefused() {
        for host in ["100.101.102.103", "192.168.0.71", "10.0.0.5", "example.com"] {
            guard case .success(let s) = Pairing.parse(
                "kod://pair?h=\(host)&p=8787&t=\(Self.token)") else {
                return XCTFail("\(host) must still PARSE — it is refused for being insecure, not malformed")
            }
            XCTAssertTrue(s.insecureBeyondThisDevice, "\(host): plaintext off-device")
            XCTAssertFalse(s.isUsable, "\(host): must not be dialled in the clear")
        }
    }

    /// …and loopback stays usable without a pin, because nothing leaves the device.
    /// This is the simulator and SSH-tunnel case, and breaking it would make local
    /// development impossible.
    func testAPinlessLoopbackCodeIsStillUsable() {
        for host in ["127.0.0.1", "localhost"] {
            guard case .success(let s) = Pairing.parse(
                "kod://pair?h=\(host)&p=8787&t=\(Self.token)") else {
                return XCTFail("\(host) must parse")
            }
            XCTAssertFalse(s.insecureBeyondThisDevice, "\(host) never leaves the device")
            XCTAssertTrue(s.isUsable)
            XCTAssertFalse(s.usesTLS)
        }
    }

    /// A code WITH a fingerprint dials wss://. Getting this backwards is a phone
    /// that speaks plaintext at a TLS listener and reports "can't reach your Mac",
    /// which sends the user looking at their wifi instead of at the pairing code.
    func testAFingerprintMakesTheConnectionTLS() {
        guard case .success(let s) =
                Pairing.parse("kod://pair?h=100.101.102.103&p=8787&t=\(Self.token)&f=\(Self.pin)")
        else { return XCTFail("a TLS payload must parse") }
        XCTAssertEqual(s.fingerprint, Self.pin)
        XCTAssertTrue(s.usesTLS)
        XCTAssertEqual(s.url?.absoluteString, "wss://100.101.102.103:8787/")
    }

    /// "" is not a pin, and must never survive as one: wss:// with nothing to
    /// compare against refuses every certificate forever, for a reason no screen
    /// explains.
    func testAnEmptyFingerprintIsNotTLS() {
        let blank = BridgeSettings(host: "h", port: 1, token: "t", fingerprint: "").normalized()
        XCTAssertNil(blank.fingerprint)
        XCTAssertFalse(blank.usesTLS)
        XCTAssertEqual(blank.url?.absoluteString, "ws://h:1/")

        let spaces = BridgeSettings(host: "h", port: 1, token: "t", fingerprint: "  \n ").normalized()
        XCTAssertNil(spaces.fingerprint)
    }

    /// The pin has to survive a relaunch. Losing it means the next launch dials
    /// ws:// at a wss:// bridge — and losing the CLEARING of it means a phone
    /// re-paired to a loopback bridge keeps dialling wss:// at a plaintext port.
    func testTheFingerprintIsPersistedBesideHostAndPort() {
        // Whatever this simulator already had, put back afterwards — this is the
        // real store, on purpose: a test against an injected one would not catch a
        // missing key or a missing removeObject.
        let before = SettingsStore.load()
        defer { SettingsStore.save(before) }

        // START FROM EMPTY. Using the real store is deliberate, but inheriting its
        // prior contents is not: this test failed once because a fingerprint had
        // been written into the simulator's defaults by hand, and a test that
        // depends on what happened to be there before is measuring the machine,
        // not the code.
        SettingsStore.save(BridgeSettings(host: "", port: 1, token: ""))
        XCTAssertNil(SettingsStore.load().fingerprint, "precondition: no stale pin")

        SettingsStore.save(BridgeSettings(host: "100.101.102.103", port: 8787,
                                          token: Self.token, fingerprint: Self.pin))
        let tls = SettingsStore.load()
        XCTAssertEqual(tls.fingerprint, Self.pin)
        XCTAssertEqual(tls.url?.absoluteString, "wss://100.101.102.103:8787/")

        SettingsStore.save(BridgeSettings(host: "127.0.0.1", port: 8787, token: Self.token))
        let plain = SettingsStore.load()
        XCTAssertNil(plain.fingerprint, "a stale pin outlives the pairing that set it")
        XCTAssertEqual(plain.url?.absoluteString, "ws://127.0.0.1:8787/")
    }

    /// A scanner that says "invalid code" for everything sends the user back to
    /// typing 64 characters by hand. Every reason must read differently.
    func testEveryRejectionReasonSaysSomethingDifferent() {
        let all: [PairingError] = [
            .empty, .notAPairingCode,
            .missingField(.host), .missingField(.port), .missingField(.token),
            .tokenNotHex, .tokenWrongLength(63), .tokenWrongLength(65),
            .badPort("0"), .badPort("70000"),
            .badFingerprint(blank: true), .badFingerprint(blank: false),
        ]
        let messages = all.map(\.message)
        XCTAssertTrue(messages.allSatisfy { !$0.isEmpty })
        XCTAssertEqual(Set(messages).count, all.count, "two rejections share a message: \(messages)")
    }
    /// A refused-for-insecurity config must NOT present as "no bridge configured".
    ///
    /// That wording sends the user back to re-enter host, port and token — none of
    /// which are wrong — and there is no field in that form for the one thing that
    /// is missing. The founder hit exactly this: typed the Wi-Fi address by hand,
    /// tapped Save & connect, and got a silent nothing.
    func testAnInsecureConfigIsItsOwnStateNotUnconfigured() {
        let s = BridgeSettings(host: "192.168.0.71", port: 8787,
                               token: Self.token, fingerprint: nil)
        XCTAssertTrue(s.insecureBeyondThisDevice)
        XCTAssertFalse(s.isUsable)

        // Fully blank settings are a DIFFERENT thing and must stay distinguishable.
        let blank = BridgeSettings(host: "", port: 8787, token: "", fingerprint: nil)
        XCTAssertFalse(blank.insecureBeyondThisDevice,
                       "nothing configured is not the same as configured-but-unencrypted")
    }

}


/// KeyPinTests — the half of pairing that decides WHO the phone is talking to.
///
/// A pinning callback that accepts everything looks exactly like one that works.
/// The only way to tell them apart is to run the real comparison against a
/// certificate carrying a DIFFERENT key and watch it refuse, so that is what these
/// do — over real DER, with digests computed by openssl rather than by the code
/// under test. Nothing here needs a server, a CA or a network.
final class KeyPinTests: XCTestCase {
    /// The Mac, as it presents itself: self-signed, P-256, SANs for a LAN
    /// address, a tailnet address and loopback. Minted with the system openssl;
    /// `pin` below is what `openssl x509 -pubkey | openssl pkey -pubin -outform
    /// DER | openssl dgst -sha256 -binary | base64url` says about it, so this is
    /// a known-answer test against an implementation that is not ours.
    private static let macCert =
        "MIIBQDCB5qADAgECAgkAv/C1SoaS0ckwCgYIKoZIzj0EAwIwFTETMBEGA1UEAwwKa29kLWJy"
        + "aWRnZTAeFw0yNjA4MjUyMzQ4MTVaFw0zNjA4MjIyMzQ4MTVaMBUxEzARBgNVBAMMCmtvZC1i"
        + "cmlkZ2UwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQDFtRzluFApPZM3lFAvK+s21D/HqPG"
        + "s3Yo7nX8eG2C+3y4wScxGICE9pa3dyC3lvBQ6ZliRtK3LGaWUgTjALLpox8wHTAbBgNVHREE"
        + "FDAShwTAqABHhwRkRGQ4hwR/AAABMAoGCCqGSM49BAMCA0kAMEYCIQC/sz2+SmNW8hE5RPoQ"
        + "1R2TEBdDmR3kUwSmijUx7t7a6AIhAJ/b8Qp9bGG7gJWM3/GxIWiuuywEH02RMixWNZBT1K1X"

    /// SHA-256 of macCert's SubjectPublicKeyInfo. 43 characters, base64url,
    /// unpadded — exactly what the Mac puts in the `f=` of a pairing code.
    private static let macPin =
        "H8C5F70XufKBxLhvttik7TqtBqqnYM1qRye3BXYOd1o"

    /// A DIFFERENT key, with the SAME subject and the SAME SANs as macCert.
    ///
    /// That is the point of it: everything a hostname check could look at
    /// matches, so a pin that quietly falls back to name or chain validation
    /// passes this certificate — and the whole feature is worthless. This is the
    /// certificate an attacker on the same coffee-shop LAN serves.
    private static let imposterCert =
        "MIIBPzCB5qADAgECAgkA5SupCCqqUWowCgYIKoZIzj0EAwIwFTETMBEGA1UEAwwKa29kLWJy"
        + "aWRnZTAeFw0yNjA4MjUyMzQ4MTVaFw0zNjA4MjIyMzQ4MTVaMBUxEzARBgNVBAMMCmtvZC1i"
        + "cmlkZ2UwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAT5zyZmj4XhE1UgHfb88mahXF6nSEHl"
        + "oOvSoWkLNeAy+6kshmdYi3NY6Wn2Du0NjPfjNla62EgenHzjaXq/7CMKox8wHTAbBgNVHREE"
        + "FDAShwTAqABHhwRkRGQ4hwR/AAABMAoGCCqGSM49BAMCA0gAMEUCIQCspHqhGTYa7uapIStS"
        + "ZcGg/6UrLQ5pQx54Oy9UItEOZwIgXjkJ7vQzn+gE5yRGTR1KUt0t2elb7Gysux7kwjfJQww="

    /// macCert's KEY in a new certificate: different serial, different subject,
    /// different validity, different SANs (10.0.0.9 and studio.local rather than
    /// the 192.168 and 100.64 addresses). The Mac's addresses change on a DHCP
    /// renewal; the pin must not.
    private static let reissuedCert =
        "MIIBQDCB6KADAgECAgkAruBF3a9OqaEwCgYIKoZIzj0EAwIwFTETMBEGA1UEAwwKc3R1ZGlv"
        + "LW1hYzAeFw0yNjA4MjUyMzU1MTJaFw0yODA3MjUyMzU1MTJaMBUxEzARBgNVBAMMCnN0dWRp"
        + "by1tYWMwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQDFtRzluFApPZM3lFAvK+s21D/HqPG"
        + "s3Yo7nX8eG2C+3y4wScxGICE9pa3dyC3lvBQ6ZliRtK3LGaWUgTjALLpoyEwHzAdBgNVHREE"
        + "FjAUhwQKAAAJggxzdHVkaW8ubG9jYWwwCgYIKoZIzj0EAwIDRwAwRAIgUFu4ojicpj/AREwg"
        + "nm2/aWiS5GzUCcRoLrcRW/eDFxICIHPxJfQU5C53+02gBI3EYvflMfPrFF+WJAktGm6ua60j"

    /// RSA-2048, and a v1 certificate — no [0] EXPLICIT version tag at all, so
    /// every field of the TBSCertificate sits one place earlier. A walker that
    /// assumed the tag is always there reads the issuer as the key here.
    private static let rsaCert =
        "MIICrjCCAZYCCQCm/WAQLXMu1zANBgkqhkiG9w0BAQsFADAZMRcwFQYDVQQDDA5rb2QtYnJp"
        + "ZGdlLXJzYTAeFw0yNjA4MjUyMzQ1MjhaFw0zNjA4MjIyMzQ1MjhaMBkxFzAVBgNVBAMMDmtv"
        + "ZC1icmlkZ2UtcnNhMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEApNNyJ7aXixLH"
        + "c7m4+VR8sA1HDMD5qtVikA2xRSDUT3Pb6mFcNHeTnlVayEfwPAjCOVg5cTbtxwiI1x+D0O3t"
        + "auPZy9OMM8agaN1Rq5GPEs3+yLcncY4zxxpodBfd2NOxPex0Ro8u+XXD6duY0CezysgKhXO7"
        + "L4UcIluIFZc1hFG+wTVI1qZ8/RYyh+bbGiMRrK+igWdkn4x6REv/GrnT/5YRRB/d0Yof8FaV"
        + "cP/ZoArOUOplxm7hjwUhIk4kW7TT2BLOn26nGMCz++sVhSuxDMKAqg3ljTxgkNbeAPP1yk18"
        + "j/A92dKKBBpvQQFOgMl6S2Nvelf7jjEskTSPkZrfJwIDAQABMA0GCSqGSIb3DQEBCwUAA4IB"
        + "AQARxxsZ+GghWeDSe7yWpPrwHIamKVnlhxZPq+HoPqOqL3g9jYBZfTHddCsStY9uznNp4I04"
        + "MWhQeEqVn9Ukeherw/P1QnmhBNvSM4/jvc79K8kIpXfnp7yTXZhOgtCDrvDIQoVvImQhz3VQ"
        + "hQkV5m7sIUpDPp+7BhYRbL9H71BQcVUS5OT2b2igWTffZoyxXMTCqQHbrluG6q5Ndbv1tj8I"
        + "+VkUAiPU7JZHlCJ0BVQ+zk4YwCtjL5tWLcgvVf6RKNB9lNcrNNINkov8EwNChp73BuF9PGIl"
        + "IO3eU1hX26L6x9+QvPyD3Wr+iKIrrK4DvwHoI1hpP6lJcLTApG6G+Kpa"

    /// What openssl says rsaCert's key hashes to.
    private static let rsaPin =
        "X3mJBUhqyIAuAizML1Ybl-oq7Y36t-dqzUc_UaD_xV0"

    /// EC with EXPLICIT curve parameters instead of a named curve: a 335-byte
    /// SubjectPublicKeyInfo whose DER lengths need the long form. Nothing mints
    /// these on purpose any more, which is precisely why it is here — it is the
    /// shape that finds an off-by-one in a length parser.
    private static let explicitCurveCert =
        "MIICMzCCAdqgAwIBAgIJAKoL6p7DgDFOMAoGCCqGSM49BAMCMBUxEzARBgNVBAMMCmtvZC1i"
        + "cmlkZ2UwHhcNMjYwODI1MjM0NTI4WhcNMzYwODIyMjM0NTI4WjAVMRMwEQYDVQQDDAprb2Qt"
        + "YnJpZGdlMIIBSzCCAQMGByqGSM49AgEwgfcCAQEwLAYHKoZIzj0BAQIhAP////8AAAABAAAA"
        + "AAAAAAAAAAAA////////////////MFsEIP////8AAAABAAAAAAAAAAAAAAAA////////////"
        + "///8BCBaxjXYqjqT57PrvVV2mIa8ZR0GsMxTsPY7zjw+J9JgSwMVAMSdNgiG5wSTamZ44ROd"
        + "JreBn36QBEEEaxfR8uEsQkf4vOblY6RA8ncDfYEt6zOg9KE5RdiYwpZP40Li/hp/m47n60p8"
        + "D54WK84zV2sxXs7LtkBoN79R9QIhAP////8AAAAA//////////+85vqtpxeehPO5ysL8YyVR"
        + "AgEBA0IABJF+tZMlyM/nK8VL3fanhny8JRE8yjytZ6q7X17mJSa34AAhReHwMaPyapSa8CEY"
        + "nmfLRRAfMqz1xWSTfN2rxbijHzAdMBsGA1UdEQQUMBKHBMCoAEeHBGREZDiHBH8AAAEwCgYI"
        + "KoZIzj0EAwIDRwAwRAIgevGGqLHp+HdYPKlsOa+02cGxpzHPoa0+CAvI93DZmKMCIBDVkSXD"
        + "Gt0TQquBfddLhLSq3BQeOfkHuYSUKdzAvU3L"

    /// What openssl says explicitCurveCert's key hashes to.
    private static let explicitCurvePin =
        "krm9g5les1MkFeQ-J8tAhJqMnJfEpqXo0m-t09hgebA"

    /// macCert's SubjectPublicKeyInfo, DER, exactly as openssl emits it. Pinned
    /// as BYTES and not just as a digest, so a change in what gets hashed says
    /// which end moved.
    private static let macSPKI =
        "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEAxbUc5bhQKT2TN5RQLyvrNtQ/x6jxrN2KO51"
        + "/Hhtgvt8uMEnMRiAhPaWt3cgt5bwUOmZYkbStyxmllIE4wCy6Q=="

    /// rsaCert's SubjectPublicKeyInfo, DER, from openssl.
    private static let rsaSPKI =
        "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEApNNyJ7aXixLHc7m4+VR8sA1HDMD5"
        + "qtVikA2xRSDUT3Pb6mFcNHeTnlVayEfwPAjCOVg5cTbtxwiI1x+D0O3tauPZy9OMM8agaN1R"
        + "q5GPEs3+yLcncY4zxxpodBfd2NOxPex0Ro8u+XXD6duY0CezysgKhXO7L4UcIluIFZc1hFG+"
        + "wTVI1qZ8/RYyh+bbGiMRrK+igWdkn4x6REv/GrnT/5YRRB/d0Yof8FaVcP/ZoArOUOplxm7h"
        + "jwUhIk4kW7TT2BLOn26nGMCz++sVhSuxDMKAqg3ljTxgkNbeAPP1yk18j/A92dKKBBpvQQFO"
        + "gMl6S2Nvelf7jjEskTSPkZrfJwIDAQAB"

    private static func der(_ base64: String) -> Data {
        guard let d = Data(base64Encoded: base64) else {
            fatalError("fixture is not base64 — the test file is corrupt, not the code")
        }
        return d
    }

    /// KNOWN-ANSWER. If this drifts, the phone and the Mac no longer agree on what
    /// is being hashed, and every pairing code in the wild stops matching.
    func testFingerprintsMatchAnIndependentImplementation() {
        XCTAssertEqual(KeyPin.fingerprint(ofCertificate: Self.der(Self.macCert)), Self.macPin)
        XCTAssertEqual(KeyPin.fingerprint(ofCertificate: Self.der(Self.rsaCert)), Self.rsaPin,
                       "RSA, and a v1 certificate with no version tag")
        XCTAssertEqual(KeyPin.fingerprint(ofCertificate: Self.der(Self.explicitCurveCert)),
                       Self.explicitCurvePin,
                       "explicit EC parameters — the long-form DER lengths")
    }

    /// What is hashed is the SubjectPublicKeyInfo, header included — not the raw
    /// key, not the certificate. Asserted as bytes so a regression names the end
    /// that moved instead of just producing a digest that differs.
    func testWhatGetsHashedIsTheSubjectPublicKeyInfo() {
        XCTAssertEqual(KeyPin.publicKeyInfo(inCertificate: Self.der(Self.macCert)),
                       Self.der(Self.macSPKI))
        XCTAssertEqual(KeyPin.publicKeyInfo(inCertificate: Self.der(Self.rsaCert)),
                       Self.der(Self.rsaSPKI))
    }

    /// THE test. Same subject, same SANs, different key: this is what an attacker
    /// on the LAN serves, and it must be refused. If this ever passes, the pin has
    /// silently become a hostname check or an accept-anything callback, and the
    /// only symptom until then is that everything appears to work.
    func testACertificateWithADifferentKeyIsRefused() {
        let imposter = Self.der(Self.imposterCert)

        XCTAssertFalse(KeyPin.certificate(imposter, matches: Self.macPin))
        XCTAssertEqual(KeyPin.verdict(expected: Self.macPin, chain: [imposter]),
                       .refuse(KeyPin.mismatch))
        // And the refusal must not read like a flaky connection.
        XCTAssertTrue(KeyPin.mismatch.contains("different key"), KeyPin.mismatch)

        // The imposter is a real, well-formed certificate — it is refused for its
        // key and nothing else, which this proves by pinning it and succeeding.
        let imposterPin = KeyPin.fingerprint(ofCertificate: imposter)
        XCTAssertNotNil(imposterPin)
        XCTAssertNotEqual(imposterPin, Self.macPin)
        XCTAssertEqual(KeyPin.verdict(expected: imposterPin!, chain: [imposter]), .trust)
    }

    /// A leaf deeper in the chain must not rescue a wrong leaf. The bridge serves
    /// one self-signed certificate; anything else offering the paired key behind a
    /// different front is not the Mac.
    func testOnlyTheLeafIsPinned() {
        XCTAssertEqual(
            KeyPin.verdict(expected: Self.macPin,
                           chain: [Self.der(Self.imposterCert), Self.der(Self.macCert)]),
            .refuse(KeyPin.mismatch))
    }

    /// The reason the KEY is pinned and not the certificate: the Mac re-issues its
    /// certificate when its addresses change, and no phone should need re-pairing
    /// for a DHCP renewal.
    func testReissuingTheCertificateAroundTheSameKeyKeepsThePin() {
        let reissued = Self.der(Self.reissuedCert)
        XCTAssertNotEqual(reissued, Self.der(Self.macCert), "a genuinely different certificate")
        XCTAssertEqual(KeyPin.fingerprint(ofCertificate: reissued), Self.macPin)
        XCTAssertEqual(KeyPin.verdict(expected: Self.macPin, chain: [reissued]), .trust)
    }

    /// No certificate is not "nothing to check" — it is a connection whose identity
    /// was never established, and it must refuse as loudly as a wrong key.
    func testAnEmptyChainIsRefused() {
        XCTAssertEqual(KeyPin.verdict(expected: Self.macPin, chain: []),
                       .refuse(KeyPin.noCertificate))
        XCTAssertNotEqual(KeyPin.noCertificate, KeyPin.mismatch,
                          "two different security events must not share a sentence")
    }

    /// Every malformed input FAILS CLOSED. A parser that returns nil is fine; one
    /// that returns nil and is then treated as "no objection" is the hole.
    func testMalformedCertificatesAreRefused() {
        let good = Self.der(Self.macCert)
        var truncations: [Data] = [Data(), Data([0x30]), Data([0x30, 0x82]), Data([0x00, 0x01, 0x02])]
        // Every prefix of a real certificate: at some point the walk runs off the
        // end of the buffer, and it must return nil rather than read past it.
        for n in stride(from: 1, to: good.count, by: 7) { truncations.append(good.prefix(n)) }
        // A SEQUENCE whose length claims far more than is there.
        truncations.append(Data([0x30, 0x82, 0xFF, 0xFF, 0x30, 0x01, 0x00]))

        for bad in truncations {
            XCTAssertNil(KeyPin.fingerprint(ofCertificate: bad), "accepted \(bad.count) bytes")
            XCTAssertFalse(KeyPin.certificate(bad, matches: Self.macPin))
            XCTAssertEqual(KeyPin.verdict(expected: Self.macPin, chain: [bad]),
                           .refuse(KeyPin.mismatch))
        }
    }

    /// The pin the Mac sends is 43 base64url characters and nothing else. Anything
    /// looser lets a code parse that can never be compared, which is a phone that
    /// refuses its own Mac forever with no explanation.
    func testFingerprintDecodingIsStrict() {
        XCTAssertEqual(KeyPin.decode(fingerprint: Self.macPin)?.count, 32)
        XCTAssertEqual(KeyPin.base64url(KeyPin.decode(fingerprint: Self.macPin)!), Self.macPin,
                       "decode and encode must be each other's inverse")
        // rsaPin and explicitCurvePin both carry - and _, so the alphabet mapping
        // is exercised in both directions.
        XCTAssertEqual(KeyPin.base64url(KeyPin.decode(fingerprint: Self.rsaPin)!), Self.rsaPin)

        let refused: [(String, String)] = [
            ("empty", ""),
            ("one short", String(Self.macPin.dropLast())),
            ("one long", Self.macPin + "A"),
            ("padded", String(Self.macPin.dropLast()) + "="),
            ("standard base64 plus", String(Self.macPin.dropLast()) + "+"),
            ("standard base64 slash", String(Self.macPin.dropLast()) + "/"),
            ("not base64 at all", String(repeating: "!", count: 43)),
            ("whitespace inside", String(Self.macPin.dropLast()) + " "),
            ("a hex digest of the right key is still not the spelling we use",
             "1fc0b917ed17b9f281c4b86fb6d8a4ed3aad06aaa760cd6a4727b705760e775a"),
        ]
        for (why, s) in refused {
            XCTAssertNil(KeyPin.decode(fingerprint: s), why)
            // …and a fingerprint that cannot be decoded refuses the RIGHT
            // certificate too. Fail closed, never open.
            XCTAssertFalse(KeyPin.certificate(Self.der(Self.macCert), matches: s), why)
        }
    }

    /// The comparison must still be a comparison. Timing cannot be asserted here,
    /// but a "constant-time" helper that returns true for everything would be a
    /// much worse bug than a leaky one.
    func testConstantTimeComparisonStillCompares() {
        let a = Data([1, 2, 3, 4])
        XCTAssertTrue(KeyPin.equalInConstantTime(a, Data([1, 2, 3, 4])))
        XCTAssertFalse(KeyPin.equalInConstantTime(a, Data([1, 2, 3, 5])), "last byte")
        XCTAssertFalse(KeyPin.equalInConstantTime(a, Data([9, 2, 3, 4])), "first byte")
        XCTAssertFalse(KeyPin.equalInConstantTime(a, Data([1, 2, 3])), "shorter")
        XCTAssertFalse(KeyPin.equalInConstantTime(a, Data([1, 2, 3, 4, 5])), "longer")
        XCTAssertTrue(KeyPin.equalInConstantTime(Data(), Data()))
    }

    /// Cross-check against the Security framework: the bytes this hashes really do
    /// contain the public key iOS itself reads out of that certificate. Catches a
    /// DER walk that lands on some other field and hashes it consistently — which
    /// would agree with itself forever and never match the Mac.
    func testTheHashedBytesReallyCarryTheCertificatesPublicKey() {
        for (name, fixture) in [("EC", Self.macCert), ("RSA", Self.rsaCert)] {
            let der = Self.der(fixture)
            guard let cert = SecCertificateCreateWithData(nil, der as CFData),
                  let key = SecCertificateCopyKey(cert),
                  let raw = SecKeyCopyExternalRepresentation(key, nil) as Data?
            else { return XCTFail("\(name): Security would not read the fixture") }
            guard let spki = KeyPin.publicKeyInfo(inCertificate: der) else {
                return XCTFail("\(name): no SubjectPublicKeyInfo")
            }
            XCTAssertNotNil(spki.range(of: raw),
                            "\(name): the hashed bytes do not contain the certificate's public key")
        }
    }
}

