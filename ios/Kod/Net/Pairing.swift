//  Pairing.swift — the QR payload the Mac shows, turned into BridgeSettings.
//
//  PURE: no UIKit, no AVFoundation, no camera. Scanner.swift hands it a string and
//  gets back either settings or a sentence to put on screen. That split is the
//  whole point: every rejection reason below is testable without a camera, and the
//  Simulator (which has no camera at all) can still exercise all of it.
//
//  The payload is a contract with the Mac, and it is ~100 ASCII bytes so it fits a
//  low-error-correction QR that scans from across a desk:
//
//      kod://pair?h=<host>&p=<port>&t=<64 lowercase hex>
//
//  Unknown query params are IGNORED on purpose. The Mac must be able to add a
//  field — a device name, a cert fingerprint — without bricking the phones already
//  in the wild, which cannot be updated in lockstep with the desktop app.

import Foundation

/// Which of the three required params a code was missing.
enum PairingField: String, Equatable {
    case host = "h"
    case port = "p"
    case token = "t"

    var human: String {
        switch self {
        case .host: return "host"
        case .port: return "port"
        case .token: return "token"
        }
    }
}

enum PairingError: Error, Equatable {
    case empty
    case notAPairingCode
    case missingField(PairingField)
    case tokenNotHex
    case tokenWrongLength(Int)
    case badPort(String)

    /// Exactly what the scanner shows. Each one names the thing that is wrong and
    /// implies the next move — a scanner that says "invalid code" and stops is how
    /// a user ends up back at typing 64 characters by hand.
    var message: String {
        switch self {
        case .empty:
            return "That code is empty."
        case .notAPairingCode:
            return "That is not a Kod pairing code. Scan the code Kod shows on your Mac."
        case .missingField(let f):
            return "This pairing code has no \(f.human) in it. Ask Kod on your Mac for a fresh code."
        case .tokenNotHex:
            return "The token in this code is not 64 lowercase hex characters (0-9, a-f)."
        case .tokenWrongLength(let n):
            return "The token in this code is \(n) characters long; a Kod token is 64."
        case .badPort(let raw):
            return "\"\(raw)\" is not a usable port. It must be a whole number from 1 to 65535."
        }
    }
}

enum Pairing {
    /// scheme + authority, all of it. Everything after this is query.
    private static let prefix = "kod://pair"

    static func parse(_ s: String) -> Result<BridgeSettings, PairingError> {
        let raw = s.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !raw.isEmpty else { return .failure(.empty) }

        // Scheme and authority are case-insensitive per RFC 3986, and a QR encoder
        // may uppercase the whole payload to reach alphanumeric mode (denser than
        // byte mode). Compared on a fixed-length prefix rather than lowercasing the
        // whole string, because lowercasing can change a string's length for some
        // Unicode and would then slice `rest` in the wrong place.
        guard raw.prefix(prefix.count).lowercased() == prefix else {
            return .failure(.notAPairingCode)
        }
        var rest = String(raw.dropFirst(prefix.count))
        // "kod://pair/?…" is the same code; URL normalisers insert that slash when
        // a payload makes a round trip through URL/URLComponents on the Mac.
        if rest.hasPrefix("/") { rest = String(rest.dropFirst()) }

        let query: String
        if rest.hasPrefix("?") {
            query = String(rest.dropFirst())
        } else if rest.isEmpty {
            // A bare "kod://pair" IS a Kod code — just an empty one. Falling through
            // to the missing-field errors below says which part is absent, where
            // "not a Kod pairing code" would be a lie.
            query = ""
        } else {
            // e.g. "kod://pairing?…" — matched the prefix but is not this scheme.
            return .failure(.notAPairingCode)
        }

        var fields: [String: String] = [:]
        for pair in query.split(separator: "&", omittingEmptySubsequences: true) {
            let parts = pair.split(separator: "=", maxSplits: 1, omittingEmptySubsequences: false)
            let key = String(parts[0]).trimmingCharacters(in: .whitespacesAndNewlines)
            let value = parts.count > 1 ? String(parts[1]) : ""
            // Trim AFTER decoding, so a token that arrived as "…%20" is as clean as
            // one that arrived with a literal trailing space. A token with invisible
            // whitespace on it fails as "unauthorized" three screens later, which is
            // the single least actionable error this app can produce.
            let decoded = (value.removingPercentEncoding ?? value)
                .trimmingCharacters(in: .whitespacesAndNewlines)
            // First occurrence wins. A duplicated key is ambiguous either way;
            // picking a side deterministically beats a parse whose result depends on
            // dictionary iteration or on which half of the string an attacker owns.
            if fields[key] == nil { fields[key] = decoded }
        }

        guard let host = fields[PairingField.host.rawValue], !host.isEmpty else {
            return .failure(.missingField(.host))
        }
        guard let portText = fields[PairingField.port.rawValue], !portText.isEmpty else {
            return .failure(.missingField(.port))
        }
        guard let port = Int(portText), (1...65_535).contains(port) else {
            return .failure(.badPort(portText))
        }
        guard let token = fields[PairingField.token.rawValue], !token.isEmpty else {
            return .failure(.missingField(.token))
        }
        // Byte-wise, not Character.isHexDigit: that property accepts "A" and even
        // fullwidth "１", and this token is compared byte-for-byte against the
        // bridge's KOD_BRIDGE_TOKEN. Anything but 32 bytes of lowercase ASCII hex
        // would be rejected by the server as a plain "unauthorized".
        guard token.utf8.allSatisfy({ (0x30...0x39).contains($0) || (0x61...0x66).contains($0) }) else {
            return .failure(.tokenNotHex)
        }
        guard token.count == 64 else {
            return .failure(.tokenWrongLength(token.count))
        }

        // The host is deliberately NOT validated as an IPv4 literal. The Mac sends
        // one today, but a MagicDNS or .local name is the obvious next step, and
        // BridgeSettings.url already copes; a stricter check here would reject a
        // code that works.
        return .success(BridgeSettings(host: host, port: port, token: token))
    }
}
