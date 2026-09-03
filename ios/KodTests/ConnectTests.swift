//  ConnectTests.swift — the two ways this app failed to reach a Mac that was
//  sitting right there, listening.
//
//  Both were reported together and they compound, which is why they are pinned in
//  one file:
//
//    1. The pairing code named ONE address. A Mac binds its Wi-Fi address and its
//       tailnet address at once, the endpoint list is tailnet-first, so the code
//       carried the 100.x one — and a phone with Tailscale off had nowhere to go.
//    2. Typing the Wi-Fi address by hand, the only apparent way out, DROPPED the
//       pinned key. That made the settings "insecure", which made them unusable,
//       which meant they were never handed to the socket — so the old connection
//       kept dialling the old address, and the screen showed a connect/timeout
//       loop against an address the user could no longer see named anywhere.
//
//  Everything here is pure: strings and structs in, decisions out. The one thing
//  that is not — that the toolbar button commits — is a view, and lives in
//  `RemoteFlowUITests`.

import XCTest
@testable import Kod

final class ConnectTests: XCTestCase {
    /// 64 lowercase hex — the only shape a real KOD_BRIDGE_TOKEN has.
    private static let token = String(repeating: "0123456789abcdef", count: 4)
    /// 43 base64url characters: the SHA-256 of a real self-signed certificate's
    /// SPKI, the same one `KeyPinTests` pins.
    private static let pin = "H8C5F70XufKBxLhvttik7TqtBqqnYM1qRye3BXYOd1o"

    /// The Mac as paired from across the room: reachable over Tailscale, pinned.
    private static var paired: BridgeSettings {
        BridgeSettings(host: "100.68.100.56", port: 18787, token: token, fingerprint: pin)
    }

    // MARK: - The pin belongs to the Mac, not to the address

    /// THE REGRESSION THIS FILE EXISTS FOR.
    ///
    /// The pin is a SHA-256 of the server's public KEY. A Mac mints that key once
    /// and serves it on every address it binds, and nothing in `PinnedTrust` ever
    /// looks at a hostname — so an edit to the address says nothing whatever about
    /// whether the key still applies. Dropping it there made "type the address
    /// that works" the single action that guaranteed nothing would work again.
    func testTypingAnotherAddressForTheSameMacKeepsThePinnedKey() {
        let edited = BridgeSettings.edited(host: "192.168.0.71", port: 18787,
                                           token: Self.token, from: Self.paired)
        XCTAssertEqual(edited.fingerprint, Self.pin, "the key identifies the Mac, not the address")
        XCTAssertEqual(edited.host, "192.168.0.71")
        XCTAssertTrue(edited.usesTLS)
        XCTAssertEqual(edited.url?.absoluteString, "wss://192.168.0.71:18787/",
                       "keeping the pin is what keeps the scheme wss://")
        XCTAssertTrue(edited.isUsable, "the whole point: this settings object can be dialled")
        XCTAssertFalse(edited.insecureBeyondThisDevice)
    }

    /// The state the old rule actually produced, asserted so the fix above cannot
    /// be mistaken for a test that would pass either way.
    ///
    /// Dropping the pin does not degrade the connection, it ENDS it: no pin means
    /// no TLS, no TLS off-device means unusable, and unusable means nothing is
    /// ever dialled at all.
    func testDroppingThePinIsWhatMadeTheSettingsUndialable() {
        var dropped = Self.paired
        dropped.host = "192.168.0.71"
        dropped.fingerprint = nil
        XCTAssertTrue(dropped.insecureBeyondThisDevice)
        XCTAssertFalse(dropped.isUsable, "this is the state the user was left in")
    }

    /// Editing must still edit. A "keep everything" rule that also kept the old
    /// host would be a form that silently ignores its own fields.
    func testAnEditStillChangesWhatWasEdited() {
        let edited = BridgeSettings.edited(host: "  10.0.0.9  ", port: 9999,
                                           token: "  \(Self.token)\n", from: Self.paired)
        XCTAssertEqual(edited.host, "10.0.0.9", "whitespace off the address")
        XCTAssertEqual(edited.port, 9999)
        XCTAssertEqual(edited.token, Self.token,
                       "a token pasted from a terminal arrives with a newline on it")
    }

    /// A key that is not this Mac's is still refused — keeping the pin is the
    /// SAFER direction, not a relaxation. Nothing here can accept a stranger; the
    /// worst case is a named refusal instead of a silent plaintext dial.
    func testAnAddressThatIsNotThisMacIsRefusedByKeyRatherThanDialledInTheClear() {
        let edited = BridgeSettings.edited(host: "192.168.0.99", port: 18787,
                                           token: Self.token, from: Self.paired)
        XCTAssertTrue(edited.usesTLS, "still wss://, still pinned, still one acceptable key")
        // And the verdict machinery says so in words, rather than failing silently.
        let stranger = KeyPin.verdict(expected: Self.pin, chain: [])
        XCTAssertEqual(stranger, .refuse(KeyPin.noCertificate))
    }

    // MARK: - One Mac, several addresses

    /// A code from a Mac bound to both networks carries both addresses, Wi-Fi
    /// first. The Wi-Fi one is `h` because pairing happens with a camera pointed
    /// at the Mac's screen — whoever is scanning is in the room.
    func testAPairingCodeCarriesEveryAddressTheMacAnswersAt() {
        guard case .success(let s) = Pairing.parse(
            "kod://pair?h=192.168.0.71&h2=100.68.100.56&p=18787&t=\(Self.token)&f=\(Self.pin)")
        else { return XCTFail("a two-address code must parse") }
        XCTAssertEqual(s.host, "192.168.0.71")
        XCTAssertEqual(s.altHosts, ["100.68.100.56"])
        XCTAssertEqual(s.allHosts, ["192.168.0.71", "100.68.100.56"],
                       "primary first — that is the order they are dialled in")
    }

    /// The contract's other half: a code from an OLDER Mac, and a code read by an
    /// older phone. Unknown params are ignored by design, which is the only reason
    /// `h2` could be added at all.
    func testACodeWithOneAddressStillMeansOneAddress() {
        guard case .success(let s) = Pairing.parse(
            "kod://pair?h=192.168.0.71&p=18787&t=\(Self.token)&f=\(Self.pin)")
        else { return XCTFail("a one-address code must parse") }
        XCTAssertTrue(s.altHosts.isEmpty)
        XCTAssertEqual(s.allHosts, ["192.168.0.71"])
    }

    /// A repeated address would cost a whole failed dial per pass, for nothing.
    func testTheSameAddressTwiceIsOnlyDialledOnce() {
        guard case .success(let s) = Pairing.parse(
            "kod://pair?h=192.168.0.71&h2=192.168.0.71&p=18787&t=\(Self.token)&f=\(Self.pin)")
        else { return XCTFail("must parse") }
        XCTAssertEqual(s.allHosts, ["192.168.0.71"])
        XCTAssertTrue(s.altHosts.isEmpty, "normalising drops the repeat at the door")
    }

    /// THE SAFETY CHECK IS ASKED OF EVERY ADDRESS.
    ///
    /// An alternate is an address this phone would put the bearer token on the
    /// wire for, so a pinless one is exactly as unsafe as a pinless primary. A
    /// check that only looked at `host` would wave this through — and the phone
    /// would dial loopback, fail, then hand the token to a LAN address in the
    /// clear.
    func testAnAlternateAddressGoesThroughTheSameSafetyCheck() {
        var s = BridgeSettings(host: "127.0.0.1", port: 18787, token: Self.token, fingerprint: nil)
        XCTAssertFalse(s.insecureBeyondThisDevice, "loopback alone is legitimately plaintext")
        s.altHosts = ["192.168.0.71"]
        XCTAssertTrue(s.insecureBeyondThisDevice,
                      "an unencrypted alternate off this device is the same hole as a primary one")
        XCTAssertFalse(s.isUsable)
    }

    /// Each address gets its own URL, and the scheme still follows the pin.
    func testEachAddressGetsItsOwnUrl() {
        var s = Self.paired
        s.host = "192.168.0.71"
        s.altHosts = ["100.68.100.56"]
        XCTAssertEqual(s.url(for: "192.168.0.71")?.absoluteString, "wss://192.168.0.71:18787/")
        XCTAssertEqual(s.url(for: "100.68.100.56")?.absoluteString, "wss://100.68.100.56:18787/")
        XCTAssertEqual(s.url?.absoluteString, s.url(for: s.host)?.absoluteString,
                       "the bare `url` is still the primary's")
    }

    /// The alternates have to survive a relaunch, or the fallback works exactly
    /// once — on the run where the code was scanned.
    func testAlternateAddressesSurviveARelaunch() {
        let before = SettingsStore.load()
        defer { SettingsStore.save(before) }

        SettingsStore.save(BridgeSettings(host: "", port: 1, token: ""))
        XCTAssertTrue(SettingsStore.load().altHosts.isEmpty, "precondition: no stale alternates")

        var s = Self.paired
        s.host = "192.168.0.71"
        s.altHosts = ["100.68.100.56"]
        SettingsStore.save(s)
        XCTAssertEqual(SettingsStore.load().altHosts, ["100.68.100.56"])

        // And they are CLEARED by a pairing that has none — otherwise a phone
        // re-paired to a Mac on one network keeps dialling an address that
        // pairing never mentioned.
        SettingsStore.save(Self.paired)
        XCTAssertTrue(SettingsStore.load().altHosts.isEmpty, "a stale alternate outlives its pairing")
    }

    // MARK: - Saying what actually went wrong

    /// Silence from a Wi-Fi address has one overwhelmingly common cause on iOS
    /// that the app cannot detect and the user cannot guess: a Local Network
    /// permission that was denied once and is remembered forever. There is no API
    /// to ask, so naming it is the only help available.
    @MainActor
    func testSilenceFromAWifiAddressNamesTheLocalNetworkPermission() {
        let s = BridgeClient.silence(host: "192.168.0.71")
        XCTAssertTrue(s.contains("192.168.0.71"), s)
        XCTAssertTrue(s.contains("Local Network"), s)
    }

    /// …and a tailnet address does NOT get that sentence. 100.64/10 arrives over a
    /// utun tunnel, which that permission does not govern — sending someone to a
    /// switch that cannot be the cause is worse than saying less.
    @MainActor
    func testSilenceFromATailnetAddressDoesNotBlameAPermissionThatCannotApply() {
        let s = BridgeClient.silence(host: "100.68.100.56")
        XCTAssertTrue(s.contains("100.68.100.56"), s)
        XCTAssertFalse(s.contains("Local Network"), s)
    }

    /// The boundaries of the block that permission actually covers. 172.16-31 is
    /// the half of 172/8 that is private; 100.64/10 is Tailscale's and is not.
    func testWhichAddressesTheLocalNetworkPermissionGoverns() {
        for host in ["192.168.0.71", "10.0.0.9", "172.16.0.1", "172.31.255.254", "169.254.1.1"] {
            XCTAssertTrue(BridgeSettings.isPrivateLAN(host), host)
        }
        for host in ["100.68.100.56", "172.15.0.1", "172.32.0.1", "8.8.8.8", "", "kod.local",
                     "fd7a:115c:a1e0::1"] {
            XCTAssertFalse(BridgeSettings.isPrivateLAN(host), host)
        }
    }

    /// Every connect failure used to collapse into one sentence — a closed port, a
    /// sleeping Mac, a denied permission and a wrong address all read as "could
    /// not connect to the server", behind four different next moves.
    @MainActor
    func testEveryConnectFailureReadsDifferently() {
        let errors: [Error] = [
            BridgeError.badURL,
            BridgeError.timeout,
            BridgeError.badPin,
            BridgeError.oversized(1),
            NSError(domain: NSURLErrorDomain, code: NSURLErrorCannotConnectToHost),
            NSError(domain: NSURLErrorDomain, code: NSURLErrorCannotFindHost),
            NSError(domain: NSURLErrorDomain, code: NSURLErrorNotConnectedToInternet),
            NSError(domain: NSURLErrorDomain, code: NSURLErrorNetworkConnectionLost),
            NSError(domain: NSURLErrorDomain, code: NSURLErrorSecureConnectionFailed),
        ]
        let said = errors.map { BridgeClient.describe($0, host: "192.168.0.71") }
        XCTAssertTrue(said.allSatisfy { !$0.isEmpty })
        XCTAssertEqual(Set(said).count, errors.count, "two failures share a sentence: \(said)")
        // A code with nothing useful to say still carries its number, so it can be
        // looked up rather than guessed at.
        let obscure = BridgeClient.describe(
            NSError(domain: NSURLErrorDomain, code: -1234), host: "192.168.0.71")
        XCTAssertTrue(obscure.contains("-1234"), obscure)
    }

    // MARK: - What the screen says while it is failing

    /// The state a stuck phone spends nearly all its time in has to name the
    /// address it is dialling. `.failed` used to be the only state that did, and
    /// it was written and replaced inside one main-actor turn — no suspension
    /// point in between — so it was never drawn once. The user watched a countdown
    /// against an address that appeared nowhere on screen.
    func testTheRetryCountdownNamesTheAddressItCannotReach() {
        let s = ConnectionState.reconnecting(seconds: 8, reason: "no answer from 100.68.100.56.",
                                             endpoint: "100.68.100.56:18787")
        let label = s.longLabel(endpoint: "192.168.0.71:18787")
        XCTAssertTrue(label.contains("100.68.100.56:18787"),
                      "it must name the address it dialled, not the one configured: \(label)")
        XCTAssertTrue(label.contains("8"), label)
    }

    /// Connected names where, too — with two addresses in play, "connected" alone
    /// no longer says which network the phone is actually using.
    func testConnectedNamesTheAddressThatAnswered() {
        let label = ConnectionState.connected("192.168.0.71:18787")
            .longLabel(endpoint: "100.68.100.56:18787")
        XCTAssertTrue(label.contains("192.168.0.71:18787"), label)
        XCTAssertTrue(ConnectionState.connected("x").isConnected)
        XCTAssertFalse(ConnectionState.connecting("x").isConnected)
    }
}
