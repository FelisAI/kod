//  Scanner.swift — the camera half of pairing.
//
//  Four things can go wrong before a code is ever seen — permission not asked yet,
//  permission denied, no camera on this device, and a QR that isn't ours — and all
//  four used to look identical: a black rectangle. Every one of them is a sentence
//  on screen here, with the button that fixes it where a button can.

import AVFoundation
import SwiftUI
import UIKit

/// What the user is looking at. There is no case that renders a blank rectangle.
enum ScannerPhase: Equatable {
    case checking
    case needsPermission
    case denied
    case restricted
    case unavailable
    case scanning
}

/// Owns the AVCaptureSession. A separate object from the view because a capture
/// session must be configured and started OFF the main thread: `startRunning()`
/// blocks until the camera is live (tens to hundreds of ms cold), and on the main
/// queue that lands squarely inside the sheet's presentation animation.
final class ScanSession: NSObject, AVCaptureMetadataOutputObjectsDelegate {
    let session = AVCaptureSession()

    private let output = AVCaptureMetadataOutput()
    private let queue = DispatchQueue(label: "pro.felisai.kod.scanner")
    private var configured = false

    /// Called on the main queue with the raw payload of every QR in frame.
    var onCode: ((String) -> Void)?

    /// False on every Simulator — there is no camera device to enumerate. Device
    /// discovery does not need camera permission, so this is safe to ask first.
    static var hasCamera: Bool { camera() != nil }

    private static func camera() -> AVCaptureDevice? {
        AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: .back)
            ?? AVCaptureDevice.default(for: .video)
    }

    func start() {
        queue.async { [self] in
            if !configured { configured = configure() }
            guard configured, !session.isRunning else { return }
            session.startRunning()
        }
    }

    func stop() {
        queue.async { [self] in
            if session.isRunning { session.stopRunning() }
        }
    }

    private func configure() -> Bool {
        guard let device = Self.camera(),
              let input = try? AVCaptureDeviceInput(device: device) else { return false }

        session.beginConfiguration()
        guard session.canAddInput(input), session.canAddOutput(output) else {
            session.commitConfiguration()
            return false
        }
        session.addInput(input)
        session.addOutput(output)
        session.commitConfiguration()

        // AFTER commit, and load-bearing: .qr only appears in
        // availableMetadataObjectTypes once the output is attached to a session
        // that already has a video input, and assigning a type that is not
        // available raises an Objective-C exception Swift cannot catch — a crash,
        // not an error.
        guard output.availableMetadataObjectTypes.contains(.qr) else { return false }
        output.metadataObjectTypes = [.qr]
        // Main queue on purpose: the callback only parses ~100 bytes and touches
        // SwiftUI state. Hopping to a background queue would buy nothing and cost a
        // data race on `phase`.
        output.setMetadataObjectsDelegate(self, queue: .main)
        return true
    }

    func metadataOutput(_ output: AVCaptureMetadataOutput,
                        didOutput metadataObjects: [AVMetadataObject],
                        from connection: AVCaptureConnection) {
        for object in metadataObjects {
            guard let code = object as? AVMetadataMachineReadableCodeObject,
                  code.type == .qr,
                  let value = code.stringValue else { continue }
            onCode?(value)
            return
        }
    }
}

/// The live camera rectangle. SwiftUI has no way to host an
/// AVCaptureVideoPreviewLayer, so this is the smallest possible UIKit bridge.
struct CameraPreview: UIViewRepresentable {
    let session: AVCaptureSession

    func makeUIView(context: Context) -> PreviewView {
        let view = PreviewView()
        view.previewLayer.session = session
        view.previewLayer.videoGravity = .resizeAspectFill
        return view
    }

    func updateUIView(_ uiView: PreviewView, context: Context) {}

    /// layerClass, not an added sublayer: the layer then resizes with the view for
    /// free, so rotation cannot leave the preview a wrong-sized rectangle.
    final class PreviewView: UIView {
        override class var layerClass: AnyClass { AVCaptureVideoPreviewLayer.self }
        var previewLayer: AVCaptureVideoPreviewLayer { layer as! AVCaptureVideoPreviewLayer }
    }
}

struct ScannerView: View {
    /// Handed settings from a code that parsed. Called at most once.
    let onPaired: (BridgeSettings) -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var scanner = ScanSession()
    @State private var phase: ScannerPhase = .checking
    @State private var rejection: String?
    @State private var paired = false

    var body: some View {
        NavigationStack {
            ZStack {
                KodColor.bg.ignoresSafeArea()
                switch phase {
                case .checking:
                    ProgressView().tint(KodColor.muted)
                case .scanning:
                    camera
                case .needsPermission:
                    notice(title: "Kod needs the camera",
                           detail: "Just to read the pairing code on your Mac's screen. Nothing is recorded.",
                           action: "Allow camera",
                           run: requestAccess)
                case .denied:
                    notice(title: "Camera access is off",
                           detail: "Turn Camera on for Kod in Settings, or close this and type the host, port and token by hand.",
                           action: "Open Settings",
                           run: openSettings)
                case .restricted:
                    notice(title: "Camera access is restricted",
                           detail: "Screen Time or a device-management profile is blocking the camera on this device. Close this and type the host, port and token by hand.")
                case .unavailable:
                    notice(title: "This device has no camera",
                           detail: "The iOS Simulator never has one. Close this and type the host, port and token by hand.")
                }
            }
            .navigationTitle("Scan pairing code")
            .navigationBarTitleDisplayMode(.inline)
            .toolbarBackground(KodColor.panel, for: .navigationBar)
            .toolbarBackground(.visible, for: .navigationBar)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button("Cancel") { dismiss() }.foregroundStyle(KodColor.accent)
                }
            }
        }
        .presentationBackground(KodColor.bg)
        .onAppear(perform: begin)
        // Hand the camera back the moment this leaves the screen: a session left
        // running keeps the capture hardware (and the recording indicator) alive.
        .onDisappear { scanner.stop() }
    }

    // MARK: - Camera

    private var camera: some View {
        VStack(spacing: 0) {
            ZStack {
                CameraPreview(session: scanner.session)
                RoundedRectangle(cornerRadius: 16, style: .continuous)
                    .stroke(rejection == nil ? KodColor.accent : KodColor.red, lineWidth: 2)
                    .frame(width: 230, height: 230)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .clipped()

            VStack(spacing: 6) {
                if let rejection {
                    // Stays on screen while the offending code is still in frame:
                    // the fix is to aim somewhere else, and a message that flashed
                    // for 200ms would be no message at all.
                    Text(rejection)
                        .font(KodFont.body)
                        .foregroundStyle(KodColor.red)
                } else {
                    Text("Point the camera at the pairing code")
                        .font(KodFont.body)
                        .foregroundStyle(KodColor.text)
                }
                // Deliberately no menu path: the Mac side does not show a code yet,
                // and naming a menu that does not exist is worse than naming none.
                Text("Kod on your Mac shows one when you pair a phone.")
                    .font(KodFont.meta)
                    .foregroundStyle(KodColor.muted2)
            }
            .multilineTextAlignment(.center)
            .frame(maxWidth: .infinity)
            .padding(.horizontal, 16)
            .padding(.vertical, 14)
            .background(KodColor.panel)
            .overlay(alignment: .top) { Rectangle().fill(KodColor.hair).frame(height: 1) }
        }
    }

    @ViewBuilder
    private func notice(title: String, detail: String, action: String? = nil, run: (() -> Void)? = nil) -> some View {
        VStack(spacing: 10) {
            Image(systemName: "qrcode.viewfinder")
                .font(.system(size: 42, weight: .light))
                .foregroundStyle(KodColor.muted2)
            EmptyNote(title: title, detail: detail)
            if let action, let run {
                Button(action: run) {
                    Text(action)
                        .font(.system(size: 15, weight: .semibold))
                        .foregroundStyle(KodColor.bg)
                        .padding(.horizontal, 20)
                        .padding(.vertical, 12)
                        .background(KodColor.accent, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                }
            }
        }
        .padding(24)
    }

    // MARK: - Permission and lifecycle

    private func begin() {
        // Device first, permission second, and that order is load-bearing. Measured
        // on the iPhone 17 Pro simulator: every camera device is nil, yet
        // authorizationStatus is .notDetermined — so permission-first would show
        // "Kod needs the camera", then a prompt that grants access to nothing and
        // lands on a black rectangle. This is the common dev case; it has to read
        // as a fact about the device.
        guard ScanSession.hasCamera else {
            phase = .unavailable
            return
        }
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized: startScanning()
        case .notDetermined: phase = .needsPermission
        case .denied: phase = .denied
        case .restricted: phase = .restricted
        @unknown default: phase = .denied
        }
    }

    private func requestAccess() {
        AVCaptureDevice.requestAccess(for: .video) { granted in
            DispatchQueue.main.async {
                if granted { startScanning() } else { phase = .denied }
            }
        }
    }

    private func startScanning() {
        scanner.onCode = handle
        scanner.start()
        phase = .scanning
    }

    private func handle(_ payload: String) {
        // stop() is async on the session queue, so callbacks already in flight will
        // still arrive; without this latch onPaired fires several times and the
        // presenter dismisses a sheet that is already gone.
        guard !paired else { return }

        switch Pairing.parse(payload) {
        case .success(let settings):
            paired = true
            scanner.stop()
            UINotificationFeedbackGenerator().notificationOccurred(.success)
            onPaired(settings)
        case .failure(let error):
            rejection = error.message
        }
    }

    private func openSettings() {
        guard let url = URL(string: UIApplication.openSettingsURLString) else { return }
        UIApplication.shared.open(url)
    }
}
