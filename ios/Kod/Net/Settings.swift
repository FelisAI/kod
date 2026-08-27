//  Settings.swift — where the bridge lives and how we prove we may talk to it.
//
//  Host and port are ordinary preferences. The token is a bearer credential for a
//  process that can see every session title on the user's machine, so it goes in
//  the Keychain; UserDefaults is only the fallback for the (basically impossible)
//  case where the Keychain refuses.
//
//  The fingerprint sits in UserDefaults, NEXT to host and port and deliberately
//  NOT in the Keychain beside the token — the next reader will wonder, so: it is a
//  SHA-256 of a PUBLIC key. The Mac hands that key to anyone who connects, it
//  proves nothing on its own, and knowing it buys an attacker nothing. What
//  matters about it is integrity, not secrecy, and anyone who can rewrite this
//  app's UserDefaults can rewrite its Keychain items too. Keeping it a plain
//  preference also means it can be read with `defaults` while debugging a pin that
//  will not match, which the Keychain would make needlessly hard.

import Foundation
import Security

struct BridgeSettings: Equatable {
    var host: String
    var port: Int
    var token: String
    /// base64url, unpadded, of SHA-256 over the DER SubjectPublicKeyInfo the Mac
    /// serves — the phone's ONLY notion of who it is talking to, since no CA will
    /// issue a certificate for 192.168.0.71 and the Mac's certificate is therefore
    /// self-signed.
    ///
    /// nil means PLAINTEXT, which the bridge only permits on loopback. It is
    /// `String?` and not `String` because those two states are different
    /// connections (ws:// vs wss://), and an empty string would let "TLS with
    /// nothing to pin" — the one combination that must never exist — be spelled.
    /// `normalized()` collapses "" back to nil for exactly that reason.
    ///
    /// Defaulted so every existing three-argument call site still compiles and
    /// still means plaintext — the old behaviour, unchanged.
    var fingerprint: String? = nil

    /// Must equal the bridge's `ws::DEFAULT_PORT`. It lives here once, as a named
    /// constant, because it previously existed as a bare 8765 in four places and
    /// silently drifted away from the port the bridge actually binds — so the app
    /// dialled a port nothing was listening on. `defaultPortMatchesTheBridge`
    /// pins it.
    static let defaultPort = 18787

    static let empty = BridgeSettings(host: "", port: defaultPort, token: "")

    var displayEndpoint: String { "\(host):\(port)" }

    /// Whether this connection is TLS. There is no separate "use TLS" switch on
    /// purpose: having the pin and using TLS are the same fact, so they cannot
    /// drift into the state where one is on and the other is off.
    var usesTLS: Bool { !(fingerprint ?? "").isEmpty }

    /// Whether this host is on this device. Loopback is the ONLY place plaintext
    /// is acceptable, because nothing leaves the machine.
    var isLoopback: Bool {
        let h = host.trimmingCharacters(in: .whitespaces)
            .trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
            .lowercased()
        return h == "127.0.0.1" || h == "::1" || h == "localhost" || h.hasPrefix("127.")
    }

    /// The client half of the rule the bridge enforces on its side: plaintext is
    /// only ever legal on loopback.
    ///
    /// The Mac refusing to BIND a non-loopback address without TLS is not enough
    /// on its own — it protects a correctly-configured Mac, not this phone. The
    /// phone is what holds the bearer token, and it is the phone that would put it
    /// on the wire. Without this check, a pairing code with no `f=` for ANY host
    /// makes this app dial ws:// and hand a 64-hex credential — and everything
    /// typed into a session — to whatever answers that address.
    ///
    /// It is deliberately a property of the SETTINGS rather than a check inside the
    /// socket code, so no future call path can reach the wire without passing it.
    /// A blank host is NOT this: nothing is configured, so nothing is about to be
    /// sent anywhere. Conflating the two would label a fresh install "not secure",
    /// which is alarming and useless.
    var insecureBeyondThisDevice: Bool {
        !host.trimmingCharacters(in: .whitespaces).isEmpty && !usesTLS && !isLoopback
    }

    /// Configured enough to be worth dialling AND safe to dial.
    var isUsable: Bool {
        !host.trimmingCharacters(in: .whitespaces).isEmpty
            && port > 0 && port <= 65_535
            && !token.isEmpty
            && !insecureBeyondThisDevice
    }

    /// Whitespace stripped from both credentials-adjacent fields. Kept as a value
    /// transform (not a mutating setter) so it is impossible to forget on one path
    /// and remember on another.
    func normalized() -> BridgeSettings {
        let fp = (fingerprint ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        return BridgeSettings(
            host: host.trimmingCharacters(in: .whitespacesAndNewlines),
            port: port,
            token: token.trimmingCharacters(in: .whitespacesAndNewlines),
            // "" would mean wss:// with nothing to compare against — a connection
            // that refuses every certificate, forever, for a reason no screen
            // explains. Blank is absent.
            fingerprint: fp.isEmpty ? nil : fp
        )
    }

    /// ws://host:port/ — wss:// when there is a key to pin — with the bracket
    /// dance IPv6 needs, and tolerant of a host pasted with a scheme or a trailing
    /// slash already on it.
    var url: URL? {
        var h = host.trimmingCharacters(in: .whitespaces)
        for prefix in ["ws://", "wss://", "http://", "https://"] where h.hasPrefix(prefix) {
            h = String(h.dropFirst(prefix.count))
        }
        while h.hasSuffix("/") { h = String(h.dropLast()) }
        // A bare IPv6 literal needs brackets before it can go in a URL.
        if h.filter({ $0 == ":" }).count > 1, !h.hasPrefix("[") { h = "[\(h)]" }
        guard !h.isEmpty else { return nil }
        // The scheme follows the pin, not the host: the Mac refuses to bind
        // anything but loopback without TLS, so a pinned setting that dialled
        // ws:// would be talking to a listener that is not there.
        return URL(string: "\(usesTLS ? "wss" : "ws")://\(h):\(port)/")
    }
}

enum SettingsStore {
    private static let hostKey = "bridge.host"
    private static let portKey = "bridge.port"
    private static let fingerprintKey = "bridge.fingerprint"
    private static let fallbackTokenKey = "bridge.token.fallback"

    static func load() -> BridgeSettings {
        let d = UserDefaults.standard
        let host = d.string(forKey: hostKey) ?? ""
        let port = d.object(forKey: portKey) as? Int ?? BridgeSettings.defaultPort
        let fingerprint = d.string(forKey: fingerprintKey)
        return BridgeSettings(host: host,
                              port: port,
                              token: Keychain.load() ?? d.string(forKey: fallbackTokenKey) ?? "",
                              fingerprint: fingerprint)
            // A stored "" would survive as "TLS with nothing to pin"; normalising
            // on the way out means only ONE of the two spellings ever reaches the
            // client.
            .normalized()
    }

    static func save(_ s: BridgeSettings) {
        // Normalise HERE, at the one door everything goes through, rather than at
        // each caller. A token pasted with a trailing newline — the normal result
        // of copying from a terminal — is compared byte-for-byte by the bridge and
        // fails as "bad token", which reads as a wrong credential rather than as
        // stray whitespace. There is nothing on either screen that would ever tell
        // you which one it was.
        let s = s.normalized()
        let d = UserDefaults.standard
        d.set(s.host, forKey: hostKey)
        d.set(s.port, forKey: portKey)
        if let fingerprint = s.fingerprint {
            d.set(fingerprint, forKey: fingerprintKey)
        } else {
            // REMOVED, not left behind. Re-pairing with a plaintext (loopback)
            // bridge after a TLS one would otherwise keep dialling wss:// at a
            // listener that speaks ws://, and the failure — a TLS handshake that
            // never completes — names neither the stale pin nor the scheme.
            d.removeObject(forKey: fingerprintKey)
        }
        if Keychain.save(s.token) {
            d.removeObject(forKey: fallbackTokenKey)
        } else {
            d.set(s.token, forKey: fallbackTokenKey)
        }
    }
}

private enum Keychain {
    private static let service = "pro.felisai.kod.remote"
    private static let account = "bridge-token"

    static func load() -> String? {
        var q = baseQuery()
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var out: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess,
              let data = out as? Data, let s = String(data: data, encoding: .utf8), !s.isEmpty
        else { return nil }
        return s
    }

    @discardableResult
    static func save(_ token: String) -> Bool {
        let q = baseQuery()
        SecItemDelete(q as CFDictionary)
        guard !token.isEmpty else { return true }
        var add = q
        add[kSecValueData as String] = Data(token.utf8)
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
    }

    private static func baseQuery() -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: service,
         kSecAttrAccount as String: account]
    }
}
