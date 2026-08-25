//  Connection.swift — the link's state, always visible, plus the sheet that sets it.
//
//  A status app that silently stops updating is worse than one that never worked,
//  so the state is never hidden: a chip in the nav bar at all times, and a
//  full-width banner the moment it is anything other than connected.

import SwiftUI

extension ConnectionState {
    var tint: Color {
        switch self {
        case .connected: return KodColor.green
        case .connecting, .reconnecting: return KodColor.amber
        case .unauthorized, .failed: return KodColor.red
        case .unconfigured: return KodColor.muted2
        }
    }

    var shortLabel: String {
        switch self {
        case .connected: return "live"
        case .connecting: return "connecting"
        case .reconnecting(let s, _): return "retry \(s)s"
        case .unauthorized: return "rejected"
        case .failed: return "offline"
        case .unconfigured: return "set up"
        }
    }

    func longLabel(endpoint: String) -> String {
        switch self {
        case .connected: return "connected to \(endpoint)"
        case .connecting: return "connecting to \(endpoint)…"
        case .reconnecting(let s, let why): return "\(why) — retrying in \(s)s"
        case .unauthorized(let m): return "token rejected — \(m)"
        case .failed(let m): return "can't reach \(endpoint) — \(m)"
        case .unconfigured: return "no bridge configured"
        }
    }
}

/// The always-there nav-bar chip.
struct ConnectionChip: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Button {
            model.showConnectionSheet = true
        } label: {
            HStack(spacing: 5) {
                Circle()
                    .fill(model.connection.tint)
                    .frame(width: 7, height: 7)
                Text(model.connection.shortLabel)
                    .font(KodFont.pill)
                    .foregroundStyle(KodColor.muted)
            }
        }
    }
}

/// The louder version, shown only when something is wrong.
struct ConnectionBanner: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        if !model.connection.isConnected {
            HStack(spacing: 8) {
                Circle().fill(model.connection.tint).frame(width: 6, height: 6)
                Text(model.connection.longLabel(endpoint: model.settings.displayEndpoint))
                    .font(KodFont.meta)
                    .foregroundStyle(KodColor.muted)
                    .lineLimit(1)
                Spacer(minLength: 6)
                Button(action: { model.retry() }) {
                    Text("retry")
                        .font(KodFont.pill)
                        .foregroundStyle(KodColor.accent)
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 7)
            .frame(maxWidth: .infinity)
            .background(KodColor.panel)
            .overlay(alignment: .bottom) { Rectangle().fill(KodColor.hair).frame(height: 1) }
        }
    }
}

struct ConnectionView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    @State private var host = ""
    @State private var port = "\(BridgeSettings.defaultPort)"
    @State private var token = ""
    @State private var showScanner = false

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    status

                    scan
                    TierHeading(text: "OR ENTER IT BY HAND", color: KodColor.muted2)

                    field("HOST", text: $host, placeholder: "192.168.1.20", keyboard: .URL)
                    field("PORT", text: $port, placeholder: "\(BridgeSettings.defaultPort)", keyboard: .numberPad)
                    secureField("TOKEN", text: $token)

                    Text("The token is the KOD_BRIDGE_TOKEN the bridge was started with. It is stored in the iOS keychain and sent only to the host above.")
                        .font(KodFont.meta)
                        .foregroundStyle(KodColor.muted2)
                        .fixedSize(horizontal: false, vertical: true)

                    Button(action: save) {
                        Text("Save & connect")
                            .font(.system(size: 15, weight: .semibold))
                            .foregroundStyle(KodColor.bg)
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 12)
                            .background(canSave ? KodColor.accent : KodColor.hair, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                    }
                    .disabled(!canSave)
                }
                .padding(16)
            }
            .background(KodColor.bg)
            .navigationTitle("Bridge")
            .navigationBarTitleDisplayMode(.inline)
            .toolbarBackground(KodColor.panel, for: .navigationBar)
            .toolbarBackground(.visible, for: .navigationBar)
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button("Done") { dismiss() }.foregroundStyle(KodColor.accent)
                }
            }
        }
        .presentationBackground(KodColor.bg)
        .sheet(isPresented: $showScanner) {
            ScannerView(onPaired: paired)
        }
        .onAppear {
            host = model.settings.host
            port = String(model.settings.port)
            token = model.settings.token
        }
    }

    /// The way in. Typing a 64-character token into a masked field is the flow
    /// this replaces, so the scanner goes ABOVE the fields, not beside them.
    private var scan: some View {
        Button {
            showScanner = true
        } label: {
            KodCard(tint: KodColor.accent) {
                HStack(spacing: 12) {
                    Image(systemName: "qrcode.viewfinder")
                        .font(.system(size: 26, weight: .light))
                        .foregroundStyle(KodColor.accent)
                    VStack(alignment: .leading, spacing: 3) {
                        Text("Scan QR code")
                            .font(KodFont.cardTitle)
                            .foregroundStyle(KodColor.strong)
                        Text("Point the phone at the pairing code on your Mac.")
                            .font(KodFont.meta)
                            .foregroundStyle(KodColor.muted)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    Spacer(minLength: 0)
                }
            }
        }
    }

    /// A scanned code carries all three fields, so there is nothing left to fill
    /// in — connect immediately. The sheet stays up on purpose: STATUS is the only
    /// place that says whether the token was ACCEPTED, and `unauthorized` is
    /// terminal, so dismissing here would hide the one outcome worth seeing.
    private func paired(_ scanned: BridgeSettings) {
        host = scanned.host
        port = String(scanned.port)
        token = scanned.token
        showScanner = false
        model.apply(settings: scanned)
    }

    private var canSave: Bool {
        !host.trimmingCharacters(in: .whitespaces).isEmpty && Int(port) != nil && !token.isEmpty
    }

    private func save() {
        guard let p = Int(port) else { return }
        model.apply(settings: BridgeSettings(host: host.trimmingCharacters(in: .whitespaces), port: p, token: token))
        dismiss()
    }

    private var status: some View {
        KodCard(tint: model.connection.tint) {
            VStack(alignment: .leading, spacing: 6) {
                TierHeading(text: "STATUS", color: KodColor.muted)
                Text(model.connection.longLabel(endpoint: model.settings.displayEndpoint))
                    .font(KodFont.body)
                    .foregroundStyle(KodColor.text)
                    .fixedSize(horizontal: false, vertical: true)
                if case .unauthorized = model.connection {
                    // Terminal state: the loop stopped on purpose, so the way back
                    // is a new token or an explicit retry.
                    Text("Fix the token and save to try again.")
                        .font(KodFont.meta)
                        .foregroundStyle(KodColor.muted2)
                }
            }
        }
    }

    @ViewBuilder
    private func field(_ label: String, text: Binding<String>, placeholder: String, keyboard: UIKeyboardType) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            TierHeading(text: label, color: KodColor.muted)
            TextField(placeholder, text: text)
                .keyboardType(keyboard)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .font(.system(size: 16, design: .monospaced))
                .foregroundStyle(KodColor.strong)
                .padding(12)
                .background(KodColor.card, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).stroke(KodColor.hair, lineWidth: 1))
        }
    }

    @ViewBuilder
    private func secureField(_ label: String, text: Binding<String>) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            TierHeading(text: label, color: KodColor.muted)
            SecureField("paste KOD_BRIDGE_TOKEN", text: text)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .font(.system(size: 16, design: .monospaced))
                .foregroundStyle(KodColor.strong)
                .padding(12)
                .background(KodColor.card, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).stroke(KodColor.hair, lineWidth: 1))
        }
    }
}
