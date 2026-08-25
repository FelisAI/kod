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

import Foundation

enum ConnectionState: Equatable {
    case unconfigured
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

    private let urlSession: URLSession = {
        let c = URLSessionConfiguration.ephemeral
        c.waitsForConnectivity = false
        c.timeoutIntervalForRequest = 15
        return URLSession(configuration: c)
    }()

    /// Seconds between attempts. Capped at 30: past that the user has walked away,
    /// and the foreground-resume path will reconnect the moment they come back.
    private static let backoff: [Int] = [1, 2, 4, 8, 15, 30]
    private static let helloTimeout: TimeInterval = 10
    private static let idleTimeout: TimeInterval = 45
    private static let pingEvery: TimeInterval = 20

    func start(_ s: BridgeSettings) {
        stop()
        settings = s
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
            return Self.describe(error)
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
                reason = Self.describe(error)
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
        onState(.connecting)

        let ws = urlSession.webSocketTask(with: url)
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
        default:
            let ns = error as NSError
            return ns.localizedDescription
        }
    }
}
