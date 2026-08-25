//  Settings.swift — where the bridge lives and how we prove we may talk to it.
//
//  Host and port are ordinary preferences. The token is a bearer credential for a
//  process that can see every session title on the user's machine, so it goes in
//  the Keychain; UserDefaults is only the fallback for the (basically impossible)
//  case where the Keychain refuses.

import Foundation
import Security

struct BridgeSettings: Equatable {
    var host: String
    var port: Int
    var token: String

    /// Must equal the bridge's `ws::DEFAULT_PORT`. It lives here once, as a named
    /// constant, because it previously existed as a bare 8765 in four places and
    /// silently drifted away from the 8787 the bridge actually binds — so the app
    /// dialled a port nothing was listening on. `defaultPortMatchesTheBridge`
    /// pins it.
    static let defaultPort = 8787

    static let empty = BridgeSettings(host: "", port: defaultPort, token: "")

    /// Configured enough to be worth dialling.
    var isUsable: Bool {
        !host.trimmingCharacters(in: .whitespaces).isEmpty && port > 0 && port <= 65_535 && !token.isEmpty
    }

    var displayEndpoint: String { "\(host):\(port)" }

    /// Whitespace stripped from both credentials-adjacent fields. Kept as a value
    /// transform (not a mutating setter) so it is impossible to forget on one path
    /// and remember on another.
    func normalized() -> BridgeSettings {
        BridgeSettings(
            host: host.trimmingCharacters(in: .whitespacesAndNewlines),
            port: port,
            token: token.trimmingCharacters(in: .whitespacesAndNewlines)
        )
    }

    /// ws://host:port/ — with the bracket dance IPv6 needs, and tolerant of a host
    /// pasted with a scheme or a trailing slash already on it.
    var url: URL? {
        var h = host.trimmingCharacters(in: .whitespaces)
        for prefix in ["ws://", "wss://", "http://", "https://"] where h.hasPrefix(prefix) {
            h = String(h.dropFirst(prefix.count))
        }
        while h.hasSuffix("/") { h = String(h.dropLast()) }
        // A bare IPv6 literal needs brackets before it can go in a URL.
        if h.filter({ $0 == ":" }).count > 1, !h.hasPrefix("[") { h = "[\(h)]" }
        guard !h.isEmpty else { return nil }
        return URL(string: "ws://\(h):\(port)/")
    }
}

enum SettingsStore {
    private static let hostKey = "bridge.host"
    private static let portKey = "bridge.port"
    private static let fallbackTokenKey = "bridge.token.fallback"

    static func load() -> BridgeSettings {
        let d = UserDefaults.standard
        let host = d.string(forKey: hostKey) ?? ""
        let port = d.object(forKey: portKey) as? Int ?? BridgeSettings.defaultPort
        return BridgeSettings(host: host, port: port, token: Keychain.load() ?? d.string(forKey: fallbackTokenKey) ?? "")
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
