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
        case .insecure: return KodColor.amber
        }
    }

    var shortLabel: String {
        switch self {
        case .connected: return "live"
        case .connecting: return "connecting"
        case .reconnecting(let s, _, _): return "retry \(s)s"
        case .unauthorized: return "rejected"
        case .failed: return "offline"
        case .unconfigured: return "set up"
        case .insecure: return "not secure"
        }
    }

    /// `endpoint` is the CONFIGURED address, used only by the states that have no
    /// address of their own. Every state that is about a connection carries the
    /// address it was actually about — a phone can be paired with a Mac at two
    /// addresses, and naming the wrong one is worse than naming none.
    func longLabel(endpoint: String) -> String {
        switch self {
        case .connected(let at): return "connected to \(at)"
        case .connecting(let at): return "connecting to \(at)…"
        case .reconnecting(let s, let why, let at):
            // The ADDRESS, not just the reason. This is the state a stuck phone
            // spends almost all of its time in, and "which address is it even
            // dialling" is the question it has to answer.
            return "can't reach \(at) — \(why) Retrying in \(s)s."
        case .unauthorized(let m): return "token rejected — \(m)"
        case .failed(let at, let m): return "can't reach \(at) — \(m)"
        case .unconfigured: return "not paired with a Mac yet"
        case .insecure:
            return "won't send your token in the clear to \(endpoint)"
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
                    .fill(model.demoMode ? KodColor.accent : model.connection.tint)
                    .frame(width: 7, height: 7)
                // Never "live" over sample data — that is the one word this chip
                // must not say about something that is not your Mac.
                Text(model.demoMode ? "sample" : model.connection.shortLabel)
                    .font(KodFont.pill)
                    .foregroundStyle(KodColor.muted)
            }
        }
        // Its LABEL is the connection state, which is the point of the chip and
        // useless as an address — a UI test cannot tap "the way in" if the way in
        // is named after whatever went wrong today.
        .accessibilityIdentifier("connection-chip")
    }
}

/// The louder version, shown only when something is wrong.
struct ConnectionBanner: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        if !model.connection.isConnected {
            HStack(spacing: 8) {
                Circle().fill(model.connection.tint).frame(width: 6, height: 6)
                // THREE lines, not one. Every reason worth printing here is
                // longer than the ~47 characters this row can hold at 12pt, and
                // the half that got cut was always the half that said what to do
                // about it — "…pair again from Kod on your Mac", "…allowed under
                // Settings › Kod › Local Network".
                Text(model.connection.longLabel(endpoint: model.settings.displayEndpoint))
                    .font(KodFont.meta)
                    .foregroundStyle(KodColor.muted)
                    .lineLimit(3)
                    .fixedSize(horizontal: false, vertical: true)
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
    /// Why the last pasted link was refused, shown under the paste button.
    @State private var pasteRefusal: String?

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    status

                    scan
                    paste
                    TierHeading(text: "OR ENTER IT BY HAND", color: KodColor.muted2)

                    field("HOST", text: $host, placeholder: "192.168.1.20", keyboard: .URL)
                    alternates
                    field("PORT", text: $port, placeholder: "\(BridgeSettings.defaultPort)", keyboard: .numberPad)
                    if !port.isEmpty && !portIsValid {
                        Text("A port is a whole number from 1 to 65535.")
                            .font(KodFont.meta)
                            .foregroundStyle(KodColor.red)
                    }
                    secureField("TOKEN", text: $token)

                    Text("The token is the t= part of the pairing link. It is stored in the iOS keychain and sent only to your Mac. To reach a Mac beyond this phone, the link's key is needed too — so pair with the code or link once, and edit the address here after that.")
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

                    sampleData
                }
                .padding(16)
            }
            // The number pad has no return key, so dragging the form is the way
            // to put the keyboard away.
            .scrollDismissesKeyboard(.interactively)
            .background(KodColor.bg)
            .navigationTitle("Your Mac")
            .navigationBarTitleDisplayMode(.inline)
            .toolbarBackground(KodColor.panel, for: .navigationBar)
            .toolbarBackground(.visible, for: .navigationBar)
            // THE TOOLBAR BUTTON COMMITS.
            //
            // It used to be a bare "Done" wired to `dismiss()`, with the only
            // writer at the BOTTOM of a scroll view — under three fields and a
            // paragraph, i.e. off-screen with a keyboard up. So the button in the
            // position iOS has meant "commit" since 2007 silently threw away a
            // typed address and token, and the app went on dialling the old one.
            // Nothing on any screen said so.
            //
            // Now the trailing button says which of the two it is, and there is a
            // Cancel to discard on purpose rather than by accident.
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    if isDirty {
                        Button("Cancel") { dismiss() }.foregroundStyle(KodColor.muted)
                    }
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Button(isDirty ? "Save" : "Done") {
                        if isDirty { save() } else { dismiss() }
                    }
                    .fontWeight(isDirty ? .semibold : .regular)
                    .foregroundStyle(canCommit ? KodColor.accent : KodColor.muted2)
                    .disabled(!canCommit)
                }
            }
        }
        // A swipe would otherwise be a third way to lose the token silently — the
        // one gesture with no label on it at all. Only while there is something
        // to lose: an unedited sheet still swipes away.
        .interactiveDismissDisabled(isDirty)
        .presentationBackground(KodColor.bg)
        .sheet(isPresented: $showScanner) {
            ScannerView(onPaired: paired)
        }
        .onAppear {
            host = model.settings.host
            port = String(model.settings.port)
            token = model.settings.token
        }
        // A pairing link pasted where an address goes — the field a user reaches
        // for first — is taken for what it is rather than saved as a hostname.
        // Only a PASTE (many characters at once): a link typed key by key passes
        // through prefixes that parse — complete up to the token, the key not
        // yet typed — and would pair a moment early, without the key.
        .onChange(of: host) { before, typed in
            let t = typed.trimmingCharacters(in: .whitespacesAndNewlines)
            guard typed.count - before.count > 1, t.lowercased().hasPrefix("kod://") else { return }
            pairFromLink(t)
        }
    }

    /// Kod on the Mac offers "Copy pairing link" beside the QR code, and with
    /// Universal Clipboard that link is already on this phone. It is the way in
    /// for a phone that cannot scan — the Simulator, a denied camera — and,
    /// unlike the form below, it carries the key, so it works beyond loopback.
    ///
    /// `PasteButton`, not a button that reads `UIPasteboard`: the system draws it,
    /// so pasting asks nothing — reading the pasteboard from an ordinary button
    /// puts up the "Allow Paste" prompt every time.
    private var paste: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 12) {
                PasteButton(payloadType: String.self) { strings in
                    guard let text = strings.first else { return }
                    Task { @MainActor in pairFromLink(text) }
                }
                .buttonBorderShape(.capsule)
                .tint(KodColor.accent)
                .labelStyle(.titleAndIcon)
                .accessibilityIdentifier("paste-pairing-link")
                Text("Copy pairing link on your Mac, then paste it here.")
                    .font(KodFont.meta)
                    .foregroundStyle(KodColor.muted)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let pasteRefusal {
                Text(pasteRefusal)
                    .font(KodFont.meta)
                    .foregroundStyle(KodColor.red)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    /// A link is a code in another form, and is judged by the same parser.
    private func pairFromLink(_ text: String) {
        switch Pairing.parse(text) {
        case .success(let settings):
            pasteRefusal = nil
            paired(settings)
        case .failure(let error):
            pasteRefusal = error.message
        }
    }

    /// For a phone with no Mac to pair with yet — App Review, or anyone deciding
    /// whether to set Kod up — and the way back when the sample data is up.
    @ViewBuilder
    private var sampleData: some View {
        if model.demoMode {
            Button("Leave the sample data") {
                model.exitDemo()
                dismiss()
            }
            .font(.system(size: 14, weight: .semibold))
            .foregroundStyle(KodColor.accent)
            .frame(maxWidth: .infinity)
        } else if !model.connection.isConnected {
            Button("No Mac nearby? Explore with sample data") {
                model.enterDemo()
                dismiss()
            }
            .font(.system(size: 14, weight: .semibold))
            .foregroundStyle(KodColor.accent)
            .frame(maxWidth: .infinity)
        }
    }

    /// The Mac's OTHER addresses, which came from the pairing code and are tried
    /// after the one above.
    ///
    /// Read-only, and shown rather than hidden: without this the phone silently
    /// dials an address that appears nowhere on the screen, and "connecting to
    /// 100.68.100.56" under a HOST field reading 192.168.0.71 looks like a bug
    /// rather than the fallback working.
    @ViewBuilder
    private var alternates: some View {
        if !model.settings.altHosts.isEmpty {
            Text("Also tries \(model.settings.altHosts.joined(separator: ", ")) — the same Mac, "
                 + "from the pairing code. Whichever answers first is used.")
                .font(KodFont.meta)
                .foregroundStyle(KodColor.muted2)
                .fixedSize(horizontal: false, vertical: true)
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

    /// A port the bridge could actually be on. `Int(port) != nil` alone let 0,
    /// -1 and 65536 through: saved, the sheet closed, and the phone fell silent
    /// as "not paired" with nothing pointing at the field that was wrong.
    private var portIsValid: Bool { Int(port).map { (1...65_535).contains($0) } ?? false }

    private var canSave: Bool {
        !host.trimmingCharacters(in: .whitespaces).isEmpty
            && portIsValid
            && !token.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    /// Whether the form holds anything the model does not. `model.settings` is
    /// normalised on the way in (`AppModel.apply`), so these comparisons are
    /// against stored values and not against whatever whitespace was typed.
    private var isDirty: Bool {
        host.trimmingCharacters(in: .whitespacesAndNewlines) != model.settings.host
            || Int(port) != model.settings.port
            || token.trimmingCharacters(in: .whitespacesAndNewlines) != model.settings.token
    }

    /// Whether the trailing button does anything. Dismissing is always allowed;
    /// saving something incomplete is not.
    private var canCommit: Bool { isDirty ? canSave : true }

    private func save() {
        guard let p = Int(port) else { return }
        // The rule that used to live here — drop the pin whenever the host changes
        // — is gone, and `BridgeSettings.edited` says why at length. Short version:
        // the pin is over the KEY, so it identifies the Mac and not the address,
        // and dropping it made typing the address that works the one action that
        // guaranteed nothing would work again.
        model.apply(settings: BridgeSettings.edited(host: host, port: p, token: token,
                                                    from: model.settings))
        dismiss()
    }

    private var status: some View {
        KodCard(tint: model.demoMode ? KodColor.accent : model.connection.tint) {
            VStack(alignment: .leading, spacing: 6) {
                TierHeading(text: "STATUS", color: KodColor.muted)
                Text(model.demoMode
                     ? "showing sample data — scan or paste your Mac's pairing code to see your own sessions"
                     : model.connection.longLabel(endpoint: model.settings.displayEndpoint))
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
                if case .insecure = model.connection {
                    // The one refusal a user CANNOT resolve in this form. Typing an
                    // address is possible; typing a 43-character key fingerprint is
                    // not, and there is no field for it — so saying "check your
                    // settings" would send them round the same loop forever. Name
                    // the only way out.
                    Text("Your Mac only accepts encrypted connections, and the key "
                         + "for that can't be typed in — it comes from the pairing "
                         + "code. Scan the code, or paste the pairing link, from "
                         + "Kod › Settings › Mobile on your Mac.")
                        .font(KodFont.meta)
                        .foregroundStyle(KodColor.muted2)
                        .fixedSize(horizontal: false, vertical: true)
                    // It used to say "typing an address by hand only works for
                    // 127.0.0.1", which was true and is not any more: the key is
                    // kept when the address is edited, because it identifies the
                    // Mac and not the address. This state now means this phone has
                    // never held a key at all, so scanning is genuinely the only
                    // way in — say that, and nothing wider.
                    Text("Once you have scanned a code, you can retype the address "
                         + "here freely — the key stays, and it is the key that "
                         + "identifies your Mac.")
                        .font(KodFont.meta)
                        .foregroundStyle(KodColor.muted2)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }

    @ViewBuilder
    private func field(_ label: String, text: Binding<String>, placeholder: String, keyboard: UIKeyboardType) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            TierHeading(text: label, color: KodColor.muted)
            TextField(placeholder, text: text)
                .accessibilityIdentifier("bridge-\(label.lowercased())")
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
            SecureField("64-character token", text: text)
                .accessibilityIdentifier("bridge-token")
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
