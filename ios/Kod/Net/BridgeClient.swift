//  BridgeClient.swift — one WebSocket to the bridge, kept alive forever.
//
//  Shape of a connection, in order:
//      dial -> send hello -> WAIT for hello_ok (the bridge sends NOTHING before
//      it) -> pump frames until something breaks -> back off -> dial again.
//
//  Two deliberate asymmetries:
//    * A wrong token is TERMINAL. Retrying it every few seconds would hammer a
//      constant-time comparison with a credential that will never work, and bury
//      the one message the user needs to see.
//    * Every receive has a deadline. The keepalive ping means silence longer than
//      the deadline is a dead link, not a quiet one — TCP alone would hold a
//      half-open socket open for minutes on a phone that changed networks.
//
//  `send` is the one thing that pushes the other way. It reports its own failure
//  instead of swallowing it: what it carries is text a person typed, and text
//  that silently never left the phone is the worst outcome this file can produce.
//
//  IDENTITY IS A KEY, NOT A NAME. Off loopback the bridge speaks wss:// with a
//  SELF-SIGNED certificate — no CA will ever issue one for 192.168.0.71 or for a
//  100.64/10 tailnet address — so the only thing this phone knows about who is on
//  the other end is the SHA-256 of the server's public key, carried out of band in
//  the pairing QR and pinned by `PinnedTrust` below. Nothing else about the
//  certificate is consulted, hostname and SANs included; `KeyPin` says why.

import CryptoKit
import Foundation
import Security

enum ConnectionState: Equatable {
    case unconfigured
    /// Configured, but pointing at an off-device address with no key to pin.
    ///
    /// Its own case rather than `.unconfigured`, because the user DID configure
    /// it and "no bridge configured" sends them to re-enter the same details that
    /// were never the problem. The address is typable by hand; the key is not, so
    /// the only way out is the pairing code — and the message has to say that.
    case insecure
    case connecting
    case connected
    /// Carries WHY, not just how long: the failure text would otherwise flash
    /// for one frame and be replaced by a countdown that explains nothing.
    case reconnecting(seconds: Int, reason: String)
    case unauthorized(String)
    case failed(String)

    var isConnected: Bool { self == .connected }
}

enum BridgeError: Error, Equatable {
    case badURL
    case timeout
    case unauthorized(String)
    case handshake(String)
    case oversized(Int)
    /// The stored fingerprint is not something that can be pinned. Thrown BEFORE
    /// dialling: settings that say "use TLS" but carry an unusable pin must not
    /// fall back to an unpinned connection, so this is the fail-closed path.
    case badPin
}

@MainActor
final class BridgeClient {
    /// Told about every state change, including the retry countdown.
    var onState: (ConnectionState) -> Void = { _ in }
    /// Told about every well-formed frame, in arrival order.
    var onMessage: (ServerMessage) -> Void = { _ in }
    /// Told what the Mac did with something this phone sent. Separate from
    /// `onMessage` because it is not news about a session — it is the answer the
    /// person who typed is waiting for.
    var onInputResult: (InputResult) -> Void = { _ in }
    /// Fired when a connection drops, so the cache can stop pretending it is live.
    var onDisconnect: () -> Void = {}

    private var settings: BridgeSettings?
    private var loop: Task<Void, Never>?
    /// True from `start` until the loop actually exits — NOT `loop != nil`, which
    /// stays true after the loop returns on a terminal unauthorized.
    private var running = false
    private var socket: URLSessionWebSocketTask?
    /// True only between `hello_ok` and the end of that same connection. `socket`
    /// alone is not enough: it is set the instant the task is created, and a frame
    /// written before the handshake finishes is answered with `err` and dropped.
    private var ready = false

    /// The plaintext path — loopback and dev only. No delegate, because there is
    /// no TLS to inspect and nothing to pin.
    private let plainSession = BridgeClient.makeSession(pinning: nil)
    /// The pinned path, rebuilt whenever the pinned key changes. Held so a
    /// reconnect reuses one session rather than one per attempt, and so the old
    /// one can be INVALIDATED when the pin changes: a URLSession keeps a strong
    /// reference to its delegate until it is invalidated, so simply dropping it
    /// would leak the previous pin's delegate for the life of the app.
    private var pinnedSession: URLSession?
    /// The live pin. Also where a refusal is recorded, because URLSession reports
    /// a cancelled auth challenge as a plain "cancelled" — see `run`.
    private var pinning: PinnedTrust?

    private static func makeSession(pinning: PinnedTrust?) -> URLSession {
        let c = URLSessionConfiguration.ephemeral
        c.waitsForConnectivity = false
        c.timeoutIntervalForRequest = 15
        guard let pinning else { return URLSession(configuration: c) }
        return URLSession(configuration: c, delegate: pinning, delegateQueue: nil)
    }

    /// The session to dial `s` with. Fingerprint present means wss:// and exactly
    /// one acceptable key; absent means plaintext.
    private func session(for s: BridgeSettings) -> URLSession {
        guard let want = s.fingerprint, !want.isEmpty else {
            pinnedSession?.invalidateAndCancel()
            pinnedSession = nil
            pinning = nil
            return plainSession
        }
        if let live = pinnedSession, pinning?.expected == want { return live }
        pinnedSession?.invalidateAndCancel()
        let delegate = PinnedTrust(expected: want)
        let session = Self.makeSession(pinning: delegate)
        pinning = delegate
        pinnedSession = session
        return session
    }

    /// Seconds between attempts. Capped at 30: past that the user has walked away,
    /// and the foreground-resume path will reconnect the moment they come back.
    private static let backoff: [Int] = [1, 2, 4, 8, 15, 30]
    private static let helloTimeout: TimeInterval = 10
    private static let idleTimeout: TimeInterval = 45
    private static let pingEvery: TimeInterval = 20

    func start(_ s: BridgeSettings) {
        stop()
        settings = s
        guard !s.insecureBeyondThisDevice else { onState(.insecure); return }
        guard s.isUsable else { onState(.unconfigured); return }
        running = true
        loop = Task { [weak self] in
            await self?.run(s)
            self?.running = false
        }
    }

    /// Foreground resume calls this on every `.active`, including the one after a
    /// glance at Control Center. Re-dialling a healthy socket there would cost a
    /// new epoch and a full snapshot for nothing, and blink the whole UI.
    func startIfNeeded(_ s: BridgeSettings) {
        if running, settings == s { return }
        start(s)
    }

    func stop() {
        loop?.cancel()
        loop = nil
        running = false
        ready = false
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
    }

    deinit {
        // A URLSession keeps its delegate alive until it is invalidated, so a
        // client that is dropped without this leaves its pin — and the session's
        // own queue — behind for the life of the process.
        pinnedSession?.invalidateAndCancel()
    }

    /// Explicit user retry — also the way out of the terminal unauthorized state.
    func retry() {
        if let s = settings { start(s) }
    }

    /// Put one message on the live socket. Returns nil when the socket accepted
    /// it, or the reason it could not.
    ///
    /// Handing it to the socket is NOT delivery — the daemon's answer arrives
    /// later as `input_result`, and that is what says whether anything was typed.
    /// This return value only covers the half the phone can see.
    func send(_ msg: ClientMessage) async -> String? {
        guard let ws = socket, ready else { return "not connected to your Mac" }
        do {
            try await ws.send(.string(msg.json))
            return nil
        } catch {
            // Same substitution as in `run`: if the socket died because the Mac
            // presented the wrong key, say THAT, not "cancelled".
            return pinning?.refusal ?? Self.describe(error)
        }
    }

    // MARK: - The loop

    private func run(_ s: BridgeSettings) async {
        var attempt = 0
        var reason = "connection lost"
        while !Task.isCancelled {
            let startedAt = Date()
            do {
                try await connectOnce(s)
            } catch is CancellationError {
                return
            } catch BridgeError.unauthorized(let msg) {
                // Terminal by design. Nothing about retrying makes a bad token good.
                onState(.unauthorized(msg.isEmpty ? "token rejected" : msg))
                return
            } catch {
                // A refused pin arrives here as NSURLErrorCancelled — the challenge
                // WAS cancelled, by us — which renders as "cancelled" and reads
                // like the user backgrounded the app. A key that is not the paired
                // key is a security event, so the delegate's own words win over
                // whatever URLSession called the resulting failure.
                reason = pinning?.refusal ?? Self.describe(error)
                onState(.failed(reason))
            }
            onDisconnect()
            if Task.isCancelled { return }

            // A connection that held for a while was healthy; the next drop should
            // retry fast rather than inherit the long tail of an old outage.
            if Date().timeIntervalSince(startedAt) > 30 { attempt = 0 }
            let wait = Self.backoff[min(attempt, Self.backoff.count - 1)]
            attempt += 1
            for remaining in stride(from: wait, through: 1, by: -1) {
                onState(.reconnecting(seconds: remaining, reason: reason))
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                if Task.isCancelled { return }
            }
        }
    }

    /// One connection, start to finish. Returns only by throwing.
    private func connectOnce(_ s: BridgeSettings) async throws {
        guard let url = s.url else { throw BridgeError.badURL }
        // Checked here rather than at the socket, because a fingerprint that
        // cannot be decoded would otherwise refuse every certificate and read as
        // "the Mac changed its key" — which would send the user hunting for an
        // attacker instead of re-pairing.
        if let want = s.fingerprint, !want.isEmpty, KeyPin.decode(fingerprint: want) == nil {
            throw BridgeError.badPin
        }
        onState(.connecting)

        let transport = session(for: s)
        // Clear any refusal left by the previous attempt, so the reason reported
        // for THIS failure is this attempt's.
        pinning?.arm()
        let ws = transport.webSocketTask(with: url)
        // The cap is the contract's, and URLSession enforces it in the framing
        // layer — before any of this code sees a byte.
        ws.maximumMessageSize = kMaxFrameBytes
        socket = ws
        ws.resume()
        defer {
            ws.cancel(with: .goingAway, reason: nil)
            ready = false
            if socket === ws { socket = nil }
        }

        try await ws.send(.string(ClientMessage.hello(token: s.token).json))

        switch try Wire.parse(frame: try await receive(ws, timeout: Self.helloTimeout)) {
        case .helloOk(let proto, let epoch, let serverTime, let input):
            onMessage(.helloOk(proto: proto, epoch: epoch, serverTime: serverTime, inputAllowed: input))
            ready = true
            onState(.connected)
        case .helloErr(let code, let message):
            if code == "unauthorized" { throw BridgeError.unauthorized(message) }
            throw BridgeError.handshake(message.isEmpty ? code : "\(code): \(message)")
        default:
            throw BridgeError.handshake("bridge answered hello with something else")
        }

        let pinger = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: UInt64(Self.pingEvery * 1_000_000_000))
                if Task.isCancelled { return }
                guard self != nil else { return }
                try? await ws.send(.string(ClientMessage.ping.json))
            }
        }
        defer { pinger.cancel() }

        while !Task.isCancelled {
            let frame = try await receive(ws, timeout: Self.idleTimeout)
            // Checked before parsing, because this answer belongs to the sender
            // and not to the session cache.
            if let answer = Wire.inputResult(frame: frame) {
                onInputResult(answer)
                continue
            }
            do {
                let msg = try Wire.parse(frame: frame)
                if case .ignored = msg { continue }  // unknown "t": drop it, stay connected
                onMessage(msg)
            } catch {
                // A frame we cannot read is not a reason to tear down a link that is
                // otherwise delivering. Oversize is the exception: it means the two
                // sides disagree about framing, so start over.
                if case WireError.frameTooLarge(let n) = error { throw BridgeError.oversized(n) }
            }
        }
    }

    /// `receive()` with a deadline. The loser of the race is abandoned; the socket
    /// is torn down by `connectOnce`'s defer either way, so no receive outlives it.
    private func receive(_ ws: URLSessionWebSocketTask, timeout: TimeInterval) async throws -> String {
        try await withThrowingTaskGroup(of: String.self) { group in
            group.addTask {
                switch try await ws.receive() {
                case .string(let s): return s
                case .data(let d): return String(decoding: d, as: UTF8.self)
                @unknown default: return ""
                }
            }
            group.addTask {
                try await Task.sleep(nanoseconds: UInt64(timeout * 1_000_000_000))
                throw BridgeError.timeout
            }
            guard let first = try await group.next() else { throw BridgeError.timeout }
            group.cancelAll()
            return first
        }
    }

    private static func describe(_ error: Error) -> String {
        switch error {
        case BridgeError.badURL: return "bad host or port"
        case BridgeError.timeout: return "no answer from bridge"
        case BridgeError.handshake(let m): return m
        case BridgeError.oversized: return "oversized frame"
        case BridgeError.badPin: return "the paired key is unreadable — scan a fresh code from your Mac"
        default:
            let ns = error as NSError
            return ns.localizedDescription
        }
    }
}

// MARK: - Pinning

/// Everything about "is this the Mac I paired with", as pure functions over bytes.
///
/// Pure on purpose: a pinning callback that accepts everything is indistinguishable
/// from one that works, right up until someone is on the same coffee-shop LAN. The
/// only way to see the difference is to run the comparison against a certificate
/// that carries a DIFFERENT key and watch it fail, and that has to be possible
/// without a server, a CA or a network — so the decision lives here, and
/// `PinnedTrust` is a twelve-line adapter over it.
///
/// What is pinned is the SubjectPublicKeyInfo — the KEY, not the certificate. That
/// is what lets the Mac re-issue a certificate (new serial, new validity, new SANs
/// after a DHCP renewal or a tailnet change) without every paired phone having to
/// be re-paired, and it is why nothing here looks at the hostname, the SANs, the
/// validity dates or the issuer. A self-signed certificate can never chain to a
/// trusted root, so ANY code path that accepted on chain validity would be either
/// dead or a hole; there is deliberately no such path.
enum KeyPin {
    /// What to do about one certificate chain.
    enum Verdict: Equatable {
        case trust
        /// Carries the sentence the user sees. A refusal is not an outage.
        case refuse(String)
    }

    /// The one thing the user must not read as "your wifi is flaky".
    static let mismatch =
        "this Mac is presenting a different key than the one you paired with — pair again from Kod on your Mac"
    static let noCertificate =
        "this Mac offered no certificate to check against the key you paired with"

    /// `chain` is the server's certificates, DER, leaf first.
    static func verdict(expected: String, chain: [Data]) -> Verdict {
        // An empty chain is not "nothing to check" — it is a connection whose
        // identity was never established, and it must refuse exactly as loudly as
        // a wrong key would.
        guard let leaf = chain.first else { return .refuse(noCertificate) }
        return certificate(leaf, matches: expected) ? .trust : .refuse(mismatch)
    }

    /// True when `der` carries EXACTLY the public key `fingerprint` names.
    static func certificate(_ der: Data, matches fingerprint: String) -> Bool {
        guard let want = decode(fingerprint: fingerprint),
              let spki = publicKeyInfo(inCertificate: der)
        else { return false }
        return equalInConstantTime(want, Data(SHA256.hash(data: spki)))
    }

    /// base64url, unpadded, of SHA-256(SubjectPublicKeyInfo) — the exact spelling
    /// the Mac puts in the pairing QR. Nil when the bytes are not a certificate.
    static func fingerprint(ofCertificate der: Data) -> String? {
        publicKeyInfo(inCertificate: der).map { base64url(Data(SHA256.hash(data: $0))) }
    }

    /// The 32 raw bytes a fingerprint stands for, or nil if it is not one.
    ///
    /// STRICT, and the same check the pairing parser uses: 43 characters of the
    /// base64url alphabet is the only encoding of a 32-byte digest this app will
    /// accept. Accepting standard base64 (`+`, `/`) or padding here would let a
    /// code parse that the Mac never emits, and a fingerprint that parses but
    /// cannot be compared is a phone that refuses its own Mac forever.
    static func decode(fingerprint: String) -> Data? {
        guard fingerprint.count == 43 else { return nil }
        var standard = ""
        standard.reserveCapacity(44)
        for ch in fingerprint.unicodeScalars {
            switch ch {
            case "-": standard.append("+")
            case "_": standard.append("/")
            case "A"..."Z", "a"..."z", "0"..."9": standard.unicodeScalars.append(ch)
            default: return nil
            }
        }
        standard.append("=")  // 43 base64url chars are 44 padded ones
        guard let raw = Data(base64Encoded: standard), raw.count == 32 else { return nil }
        return raw
    }

    static func base64url(_ d: Data) -> String {
        d.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }

    /// Byte comparison that does not return early on the first difference.
    ///
    /// The length check IS allowed to short-circuit: both sides are always a
    /// 32-byte digest, so the length leaks nothing an attacker does not know.
    static func equalInConstantTime(_ a: Data, _ b: Data) -> Bool {
        guard a.count == b.count else { return false }
        var diff: UInt8 = 0
        for (x, y) in zip(a, b) { diff |= x ^ y }
        return diff == 0
    }

    /// The DER SubjectPublicKeyInfo out of a DER certificate, header included —
    /// byte-for-byte what `openssl x509 -pubkey | openssl pkey -pubin -outform DER`
    /// produces, which is what the Mac hashes.
    ///
    /// Walked BY POSITION, never by searching for something that looks like a key:
    /// a certificate is attacker-supplied, and any parser that hunts for a matching
    /// SPKI could be handed a certificate that carries the victim's key in an
    /// extension while being signed by the attacker's.
    ///
    ///     Certificate    ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
    ///     TBSCertificate ::= SEQUENCE { [0] version DEFAULT v1, serialNumber,
    ///                                   signature, issuer, validity, subject,
    ///                                   subjectPublicKeyInfo, ... }
    ///
    /// Key-type agnostic on purpose. The alternative on Apple platforms —
    /// `SecKeyCopyExternalRepresentation` plus a hard-coded ASN.1 header per key
    /// type — silently pins the wrong bytes the day the Mac mints an RSA key
    /// instead of a P-256 one.
    static func publicKeyInfo(inCertificate der: Data) -> Data? {
        let b = [UInt8](der)
        guard let cert = tlv(b, 0), cert.tag == 0x30,
              let tbs = tlv(b, cert.content.lowerBound), tbs.tag == 0x30
        else { return nil }

        var i = tbs.content.lowerBound
        // [0] EXPLICIT version. Optional, and genuinely absent from a v1
        // certificate — dropping this branch shifts every field by one.
        if let version = tlv(b, i), version.tag == 0xA0 { i = version.end }
        // serialNumber, signature, issuer, validity, subject.
        for _ in 0..<5 {
            guard let field = tlv(b, i), field.end <= tbs.content.upperBound else { return nil }
            i = field.end
        }
        guard let spki = tlv(b, i), spki.tag == 0x30, spki.end <= tbs.content.upperBound else { return nil }
        return Data(b[i..<spki.end])
    }

    /// One DER tag-length-value at `i`. Nil for anything malformed or truncated —
    /// there is no recovery worth attempting, because a certificate this cannot
    /// read is a certificate that will not be trusted.
    private static func tlv(_ b: [UInt8], _ i: Int) -> (tag: UInt8, content: Range<Int>, end: Int)? {
        guard i >= 0, i + 1 < b.count else { return nil }
        let tag = b[i]
        var p = i + 1
        let first = b[p]
        p += 1
        var length = 0
        if first < 0x80 {
            length = Int(first)
        } else {
            // 0x80 is BER's indefinite length, which DER forbids; more than four
            // length bytes is a certificate larger than any this app will see.
            let count = Int(first & 0x7F)
            guard (1...4).contains(count), p + count <= b.count else { return nil }
            for _ in 0..<count {
                length = (length << 8) | Int(b[p])
                p += 1
            }
        }
        guard length >= 0, p + length <= b.count else { return nil }
        return (tag, p..<(p + length), p + length)
    }
}

/// The URLSession side of the pin: turn a server-trust challenge into a `Verdict`,
/// and remember a refusal so the client can say what really happened.
///
/// Not `@MainActor`: URLSession calls this on its own delegate queue, so the one
/// piece of mutable state is behind a lock.
final class PinnedTrust: NSObject, URLSessionDelegate {
    /// base64url SHA-256 of the SPKI this connection will accept, and nothing else.
    let expected: String

    private let lock = NSLock()
    private var refusalText: String?

    init(expected: String) { self.expected = expected }

    /// Called before each dial. Without it a refusal from an earlier attempt would
    /// be reported as the reason a later, ordinary failure happened.
    func arm() {
        lock.lock()
        refusalText = nil
        lock.unlock()
    }

    /// Why the last connection was refused, if it was. Read, not consumed: both
    /// the reconnect loop and an in-flight `send` want the same answer.
    var refusal: String? {
        lock.lock()
        defer { lock.unlock() }
        return refusalText
    }

    func urlSession(_ session: URLSession,
                    didReceive challenge: URLAuthenticationChallenge,
                    completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust else {
            // Not ours to answer — and NOT an accept.
            completionHandler(.performDefaultHandling, nil)
            return
        }
        guard let trust = challenge.protectionSpace.serverTrust else {
            record(KeyPin.noCertificate)
            completionHandler(.cancelAuthenticationChallenge, nil)
            return
        }
        let chain = (SecTrustCopyCertificateChain(trust) as? [SecCertificate] ?? [])
            .map { SecCertificateCopyData($0) as Data }

        switch KeyPin.verdict(expected: expected, chain: chain) {
        case .trust:
            // The pin IS the evaluation. `SecTrustEvaluate` is not called and its
            // answer is not consulted: a self-signed certificate can never chain,
            // so a path that accepted on chain validity would never run — and one
            // that accepted on either would accept any CA-issued certificate for
            // an attacker's name.
            completionHandler(.useCredential, URLCredential(trust: trust))
        case .refuse(let why):
            record(why)
            completionHandler(.cancelAuthenticationChallenge, nil)
        }
    }

    private func record(_ why: String) {
        lock.lock()
        refusalText = why
        lock.unlock()
    }
}
