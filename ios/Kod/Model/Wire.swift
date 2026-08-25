//  Wire.swift — the v0 bridge protocol, verbatim.
//
//  This file is the ONLY place that knows the wire's spelling. Everything above it
//  speaks `Session` / `ServerMessage`; nothing else touches JSON. The contract is
//  pinned, so the rules that look paranoid here are the contract, not taste:
//
//    * 65536-byte frame cap, enforced BEFORE parsing.
//    * Unknown object fields are IGNORED (that is how this versions without lockstep).
//    * An unknown "t" is dropped SILENTLY — never an error, never a disconnect.
//    * v0 is READ-ONLY. There is no input message. Do not invent one.

import Foundation

/// Largest frame the phone will look at. Bigger than this is a desync, not a message.
let kMaxFrameBytes = 65_536

/// Protocol version this client speaks.
let kProtoVersion = 1

// MARK: - Value types

enum Cli: String, CaseIterable {
    case claude, codex, shell
    /// A cli the bridge learned about after this build shipped. Rendered, not crashed.
    case unknown

    init(wire: String) { self = Cli(rawValue: wire) ?? .unknown }

    var label: String { self == .unknown ? "cli" : rawValue }
}

enum Phase: String {
    case spawning, idle, busy, awaiting, dead
    /// Same forward-compat hatch as `Cli.unknown`.
    case unknown

    init(wire: String) { self = Phase(rawValue: wire) ?? .unknown }

    var label: String {
        switch self {
        case .spawning: return "starting"
        case .idle: return "idle"
        case .busy: return "working"
        case .awaiting: return "needs you"
        case .dead: return "ended"
        case .unknown: return "unknown"
        }
    }
}

/// One session, exactly as `<S>` defines it.
struct Session: Identifiable, Equatable {
    var sid: UInt64
    var cli: Cli
    var project: String
    var title: String
    var phase: Phase
    var phaseSince: UInt64
    var alive: Bool
    var lastMessage: String
    var pendingHeadline: String?
    var trouble: String?
    var limitHit: Bool
    var limitPercent: Int?
    var limitReset: String?

    var id: UInt64 { sid }

    /// Title that is never empty — an untitled row is unclickable in practice.
    var displayTitle: String {
        let t = title.trimmingCharacters(in: .whitespacesAndNewlines)
        return t.isEmpty ? "session \(sid)" : t
    }

    /// The two states that put a session in front of the user, in one place so
    /// Standup and Projects can never disagree about what "needs you" means.
    var needsYou: Bool { alive && (limitHit || phase == .awaiting) }
}

extension Session: Decodable {
    private enum K: String, CodingKey {
        case sid, cli, project, title, phase, alive, trouble
        case phaseSince = "phase_since"
        case lastMessage = "last_message"
        case pendingHeadline = "pending_headline"
        case limitHit = "limit_hit"
        case limitPercent = "limit_percent"
        case limitReset = "limit_reset"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        // `sid` is identity: without it there is nothing to key a cache on, so it
        // is the one field whose absence throws. Every other field degrades to a
        // default, because losing a whole snapshot over one odd value is worse
        // than rendering a session with a blank title.
        sid = try c.decode(UInt64.self, forKey: .sid)
        cli = Cli(wire: (try? c.decode(String.self, forKey: .cli)) ?? "")
        project = (try? c.decode(String.self, forKey: .project)) ?? ""
        title = (try? c.decode(String.self, forKey: .title)) ?? ""
        phase = Phase(wire: (try? c.decode(String.self, forKey: .phase)) ?? "")
        phaseSince = (try? c.decode(UInt64.self, forKey: .phaseSince)) ?? 0
        alive = (try? c.decode(Bool.self, forKey: .alive)) ?? false
        lastMessage = (try? c.decode(String.self, forKey: .lastMessage)) ?? ""
        // "" and null mean the same thing to every view above: nothing to show.
        // Normalising here keeps `if let` from opening an empty amber card.
        pendingHeadline = Self.nonEmpty(try? c.decodeIfPresent(String.self, forKey: .pendingHeadline))
        trouble = Self.nonEmpty(try? c.decodeIfPresent(String.self, forKey: .trouble))
        limitHit = (try? c.decode(Bool.self, forKey: .limitHit)) ?? false
        limitPercent = (try? c.decodeIfPresent(Int.self, forKey: .limitPercent)) ?? nil
        limitReset = Self.nonEmpty(try? c.decodeIfPresent(String.self, forKey: .limitReset))
    }

    private static func nonEmpty(_ s: String??) -> String? {
        guard let inner = s, let v = inner else { return nil }
        return v.isEmpty ? nil : v
    }
}

// MARK: - Messages

enum ServerMessage: Equatable {
    case helloOk(proto: Int, epoch: String, serverTime: UInt64, inputAllowed: Bool)
    case helloErr(code: String, message: String)
    case sessions(epoch: String, sessions: [Session])
    case session(epoch: String, rev: UInt64, session: Session)
    case gone(epoch: String, sid: UInt64)
    case pong
    case err(code: String, message: String)
    /// An unknown "t". Carried rather than thrown so the caller can log it; the
    /// store does nothing with it.
    case ignored(t: String)
}

enum ClientMessage {
    case hello(token: String)
    case ping

    var json: String {
        switch self {
        case .hello(let token):
            let escaped = ClientMessage.escape(token)
            return "{\"t\":\"hello\",\"proto\":\(kProtoVersion),\"token\":\"\(escaped)\"}"
        case .ping:
            return "{\"t\":\"ping\"}"
        }
    }

    /// Hand-rolled because the only variable is a token the user typed; going
    /// through JSONEncoder for one string field would be more code, not less.
    private static func escape(_ s: String) -> String {
        var out = ""
        for ch in s.unicodeScalars {
            switch ch {
            case "\"": out += "\\\""
            case "\\": out += "\\\\"
            case "\n": out += "\\n"
            case "\r": out += "\\r"
            case "\t": out += "\\t"
            default:
                if ch.value < 0x20 {
                    out += String(format: "\\u%04x", ch.value)
                } else {
                    out.unicodeScalars.append(ch)
                }
            }
        }
        return out
    }
}

enum WireError: Error, Equatable {
    case frameTooLarge(Int)
    case notJSON
    case missingType
    case malformed(String)
}

enum Wire {
    static func parse(frame: String) throws -> ServerMessage {
        try parse(data: Data(frame.utf8))
    }

    static func parse(data: Data) throws -> ServerMessage {
        // BEFORE parsing, per the contract: a 4 MB frame must not become a 4 MB
        // allocation in the JSON decoder just to be rejected afterwards.
        guard data.count <= kMaxFrameBytes else { throw WireError.frameTooLarge(data.count) }
        let d = JSONDecoder()
        guard let head = try? d.decode(TypeOnly.self, from: data) else { throw WireError.missingType }

        do {
            switch head.t {
            case "hello_ok":
                let m = try d.decode(HelloOkFrame.self, from: data)
                return .helloOk(proto: m.proto ?? kProtoVersion,
                                epoch: m.epoch,
                                serverTime: m.server_time ?? 0,
                                inputAllowed: m.caps?.input ?? false)
            case "hello_err":
                let m = try d.decode(CodeFrame.self, from: data)
                return .helloErr(code: m.code ?? "", message: m.message ?? "")
            case "sessions":
                let m = try d.decode(SessionsFrame.self, from: data)
                return .sessions(epoch: m.epoch, sessions: m.sessions ?? [])
            case "session":
                let m = try d.decode(SessionFrame.self, from: data)
                return .session(epoch: m.epoch, rev: m.rev ?? 0, session: m.session)
            case "gone":
                let m = try d.decode(GoneFrame.self, from: data)
                return .gone(epoch: m.epoch, sid: m.sid)
            case "pong":
                return .pong
            case "err":
                let m = try d.decode(CodeFrame.self, from: data)
                return .err(code: m.code ?? "", message: m.message ?? "")
            default:
                // Unknown "t" at the phone: ignore it silently.
                return .ignored(t: head.t)
            }
        } catch {
            throw WireError.malformed(head.t)
        }
    }

    // Private mirrors of the wire shapes. Unknown fields fall on the floor for
    // free — that is Codable's default and exactly what the contract wants.
    private struct TypeOnly: Decodable { let t: String }
    private struct Caps: Decodable { let input: Bool? }
    private struct HelloOkFrame: Decodable {
        let proto: Int?
        let epoch: String
        let server_time: UInt64?
        let caps: Caps?
    }
    private struct CodeFrame: Decodable { let code: String?; let message: String? }
    private struct SessionsFrame: Decodable { let epoch: String; let sessions: [Session]? }
    private struct SessionFrame: Decodable { let epoch: String; let rev: UInt64?; let session: Session }
    private struct GoneFrame: Decodable { let epoch: String; let sid: UInt64 }
}
