//  Wire.swift — the v0 bridge protocol, verbatim.
//
//  This file is the ONLY place that knows the wire's spelling. Everything above it
//  speaks `Session` / `ServerMessage`; nothing else touches JSON. The contract is
//  pinned, so the rules that look paranoid here are the contract, not taste:
//
//    * 65536-byte frame cap, enforced BEFORE parsing.
//    * Unknown object fields are IGNORED (that is how this versions without lockstep).
//    * An unknown "t" is dropped SILENTLY — never an error, never a disconnect.
//    * The phone may say four things: hello, ping, input and key. The last two go
//      only to a session the Mac marked `can_input`, and the DAEMON — not this
//      file, not the bridge — is what enforces that. All this file can do is
//      spell the request correctly and report the answer honestly.

import Foundation

/// Largest frame the phone will look at. Bigger than this is a desync, not a message.
let kMaxFrameBytes = 65_536

/// Protocol version this client speaks. 2, not 1: a bridge that accepts typing
/// turns a proto-1 phone away at hello rather than advertise a composer whose
/// answers that build has no code to read.
let kProtoVersion = 2

// MARK: - Value types

enum Cli: String, CaseIterable {
    case claude, codex, shell
    /// A cli the bridge learned about after this build shipped. Rendered, not crashed.
    case unknown

    init(wire: String) { self = Cli(rawValue: wire) ?? .unknown }

    var label: String { self == .unknown ? "cli" : rawValue }
}

/// The only keys the daemon will take from a phone (`protocol::PhoneKey`). An
/// enum rather than a String because a misspelled key is not a compile error on
/// the wire — it is a refusal the user has to read and cannot act on.
enum PhoneKey: String, CaseIterable {
    case enter, escape, up, down, tab
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
    /// Whether the Mac will accept typing into THIS session. Defaults to false,
    /// which is the whole point of the default: a Mac too old to send the field
    /// is a Mac that would refuse the input, so its sessions must not grow a
    /// composer that cannot work.
    var canInput: Bool = false

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
        case canInput = "can_input"
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
        canInput = (try? c.decode(Bool.self, forKey: .canInput)) ?? false
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

/// What became of one `input` or `key` this phone sent.
///
/// Deliberately NOT a `ServerMessage`: every one of those is a statement about
/// sessions and goes into the cache, while this is an answer about a frame this
/// phone put on the wire. It belongs to whoever sent it and to nothing else, so
/// the cache cannot see it and cannot be disturbed by it.
struct InputResult: Equatable {
    /// Echoes the `rid` the phone sent, so a late answer can be DISCARDED rather
    /// than applied to whatever happens to be in flight now.
    let rid: UInt64
    let sid: UInt64
    /// False unless the Mac SAID true. The composer empties the user's box on an
    /// acceptance, so silence must never read as one.
    let ok: Bool
    /// The Mac's own words for a refusal, shown verbatim. Empty on an
    /// acceptance, where the bridge sends `reason: null`.
    let message: String
}

enum ClientMessage {
    case hello(token: String)
    case ping
    /// A composed line for one session. It is PASTED, not submitted — the daemon
    /// sends `KeyInput::Paste` and nothing else — so the Enter that submits it is
    /// a separate `key`, and a caller that forgets leaves the text sitting in the
    /// agent's prompt.
    /// `rid` is a client-chosen id the Mac echoes back. Without it an answer can
    /// only be matched by session, so a LATE reply to one send settles a newer
    /// one — dispatching the Enter against a paste that never landed, which
    /// submits the wrong text at the agent. A phone backgrounded mid-send hits
    /// that on ordinary use, so this is not a theoretical race.
    case input(sid: UInt64, text: String, rid: UInt64)
    case key(sid: UInt64, key: PhoneKey, rid: UInt64)

    var json: String {
        switch self {
        case .hello(let token):
            let escaped = ClientMessage.escape(token)
            return "{\"t\":\"hello\",\"proto\":\(kProtoVersion),\"token\":\"\(escaped)\"}"
        case .ping:
            return "{\"t\":\"ping\"}"
        case .input(let sid, let text, let rid):
            return "{\"t\":\"input\",\"sid\":\(sid),\"text\":\"\(ClientMessage.escape(text))\",\"rid\":\(rid)}"
        case .key(let sid, let key, let rid):
            // The key is an enum, so it needs no escaping — nothing a user types
            // can reach this string.
            return "{\"t\":\"key\",\"sid\":\(sid),\"key\":\"\(key.rawValue)\",\"rid\":\(rid)}"
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
                // Unknown "t" at the phone: ignore it silently. `input_result`
                // lands here too, and that is on purpose — it is read by
                // `inputResult(frame:)` before this is ever called, and the cache
                // must never be handed an answer to the phone's own typing.
                return .ignored(t: head.t)
            }
        } catch {
            throw WireError.malformed(head.t)
        }
    }

    /// Read one frame as an answer to something this phone sent. Nil for every
    /// other frame — including one over the size cap, which `parse` then rejects
    /// as the desync it is, so this cannot become a way around the limit.
    static func inputResult(frame: String) -> InputResult? {
        let data = Data(frame.utf8)
        guard data.count <= kMaxFrameBytes else { return nil }
        let d = JSONDecoder()
        // The type is checked FIRST: `gone` also carries a `sid`, and would
        // otherwise decode cleanly into the shape below and be answered as if the
        // user's typing had been refused.
        guard let head = try? d.decode(TypeOnly.self, from: data), head.t == "input_result",
              let m = try? d.decode(InputResultFrame.self, from: data) else { return nil }
        // An absent rid decodes to 0, which is the id no send ever uses — so an
        // older Mac's answers are ignored rather than misapplied.
        return InputResult(rid: m.rid ?? 0, sid: m.sid, ok: m.ok ?? false, message: m.reason ?? "")
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
    /// `sid` is required for the same reason it is on `Session`: an answer that
    /// cannot be matched to a session is an answer that could clear the wrong
    /// composer.
    private struct InputResultFrame: Decodable { let rid: UInt64?; let sid: UInt64; let ok: Bool?; let reason: String? }
}
