//  SessionView.swift — one session: what it said, and what you say back.
//
//  There is still no terminal grid, and that stays a product decision rather than
//  a missing feature: on a phone the useful thing is the sentence the agent just
//  said and the question it is waiting on, as native wrapping text you can select
//  and scroll. An 80x24 character grid squeezed onto a 390pt screen is unreadable.
//
//  What has changed is the bottom of the screen. The daemon now accepts typing
//  from a phone into an AGENT session — never a shell, never a dead one — and
//  marks each session `can_input`, so the composer appears on the Mac's answer
//  rather than on this build's opinion. Every state where it cannot appear says
//  why, in words, instead of leaving a screen that quietly does nothing.

import SwiftUI

struct SessionView: View {
    @Environment(AppModel.self) private var model
    @FocusState private var composing: Bool

    var body: some View {
        Group {
            if let s = model.selected {
                reader(s)
            } else if model.selectedSid != nil {
                EmptyNote(title: "That session is gone",
                          detail: "It ended, or the bridge reattached. Pick another from Standup or Projects.")
            } else {
                EmptyNote(title: "No session open",
                          detail: "Tap a card in Standup or a session in Projects.")
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(KodColor.bg)
        .kodChrome(title: "Session")
        .toolbar {
            ToolbarItem(placement: .topBarLeading) { picker }
        }
    }

    @ViewBuilder
    private func reader(_ s: Session) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                VStack(alignment: .leading, spacing: 9) {
                    Text(s.displayTitle)
                        .font(.system(size: 22, weight: .semibold))
                        .foregroundStyle(KodColor.strong)
                        .fixedSize(horizontal: false, vertical: true)
                    HStack(spacing: 8) {
                        ProjectPill(slug: s.project)
                        HStack(spacing: 5) {
                            PhaseDot(phase: s.phase)
                            MetaTag(text: s.phase.label, color: KodColor.phase(s.phase))
                        }
                        MetaTag(text: TimeFmt.ago(model.age(since: s.phaseSince)))
                        MetaTag(text: s.cli.label)
                    }
                }

                if s.limitHit {
                    banner(LimitLine.text(s), detail: nil, color: KodColor.red)
                }
                if let trouble = s.trouble {
                    banner(trouble, detail: nil, color: KodColor.red)
                }
                if let headline = s.pendingHeadline {
                    banner(headline,
                           detail: canType(s) ? nil : "Answer it in Kod on your Mac — this one cannot be answered from the phone.",
                           color: KodColor.amber,
                           heading: "WAITING ON YOU")
                }

                VStack(alignment: .leading, spacing: 10) {
                    TierHeading(text: "LAST MESSAGE", color: KodColor.muted)
                    KodCard {
                        if s.lastMessage.isEmpty {
                            Text("nothing said yet")
                                .font(KodFont.body)
                                .foregroundStyle(KodColor.muted2)
                        } else {
                            Text(s.lastMessage)
                                .font(.system(size: 15))
                                .foregroundStyle(KodColor.text)
                                .lineSpacing(3)
                                .textSelection(.enabled)
                                .fixedSize(horizontal: false, vertical: true)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                }

                MetaTag(text: canType(s) ? "session \(s.sid)" : "session \(s.sid) · read-only")
            }
            .padding(16)
        }
        // With a short session there is nothing to scroll, so this is not the only
        // way out of the keyboard — see the Done button on the field itself.
        .scrollDismissesKeyboard(.interactively)
        // safeAreaInset, not an overlay: SwiftUI lifts it above the keyboard AND
        // reserves its height in the scroll view, so neither the field nor the
        // last line of the message ends up under something.
        .safeAreaInset(edge: .bottom, spacing: 0) { footer(s) }
    }

    // MARK: - The bottom of the screen

    /// True only when this Mac takes typing at all AND marked this session as
    /// answerable. `inputAllowed` is the bridge's coarse announcement, `canInput`
    /// the daemon's per-session one; the composer needs both to be true.
    private func canType(_ s: Session) -> Bool { model.inputAllowed && s.canInput }

    @ViewBuilder
    private func footer(_ s: Session) -> some View {
        VStack(spacing: 0) {
            Rectangle()
                .fill(KodColor.hair)
                .frame(height: 1)
            Group {
                // Order matters: the most specific true thing first. A dead shell
                // is dead before it is a shell.
                if !s.alive || s.phase == .dead {
                    note("This session has ended",
                         "Nothing is listening on the other end. Start it again from Kod on your Mac.")
                } else if s.cli == .shell {
                    note("Kod does not let a phone type into a shell",
                         "A shell runs whatever it is handed. claude and codex ask before they act, so those you can answer from here.")
                } else if !model.inputAllowed {
                    note("This Mac is not taking typing from the phone",
                         "Its Kod is older than this app, or the bridge has input switched off. Answer it there instead.")
                } else if !s.canInput {
                    note("Kod is not offering input for this session",
                         "Your Mac did not mark this one as answerable from a phone.")
                } else {
                    composer(s)
                }
            }
            .padding(.horizontal, 14)
            .padding(.top, 10)
            .padding(.bottom, 8)
        }
        .background(KodColor.panel)
    }

    private func note(_ title: String, _ detail: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(KodFont.body)
                .foregroundStyle(KodColor.muted)
                .fixedSize(horizontal: false, vertical: true)
            Text(detail)
                .font(KodFont.meta)
                .foregroundStyle(KodColor.muted2)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func composer(_ s: Session) -> some View {
        @Bindable var m = model
        return VStack(alignment: .leading, spacing: 9) {
            keys
            HStack(alignment: .bottom, spacing: 9) {
                TextField("answer \(s.cli.label)…", text: $m.draft, axis: .vertical)
                    .lineLimit(1...5)
                    .font(.system(size: 16))
                    .foregroundStyle(KodColor.strong)
                    .tint(KodColor.accent)
                    .submitLabel(.send)
                    .focused($composing)
                    .onSubmit { model.sendDraft() }
                    .padding(10)
                    .background(KodColor.card, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                    .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).stroke(KodColor.hair, lineWidth: 1))
                    .toolbar {
                        ToolbarItemGroup(placement: .keyboard) {
                            Spacer()
                            Button("Done") { composing = false }
                                .foregroundStyle(KodColor.accent)
                        }
                    }
                send
            }
            if let failure = model.composer.failure {
                // The text is still in the box — the model only empties it on an
                // acceptance — so this reads as "try again", not "it vanished".
                HStack(alignment: .top, spacing: 6) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.system(size: 11))
                        .foregroundStyle(KodColor.red)
                    Text(failure)
                        .font(KodFont.meta)
                        .foregroundStyle(KodColor.red)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }

    /// The keys the daemon accepts, and nothing else. Arrow-then-Enter is how a
    /// claude permission prompt gets answered, which is the single most likely
    /// thing anyone does from a phone — so they are one tap away, above the field,
    /// rather than hidden behind the text you would otherwise have to type.
    private var keys: some View {
        HStack(spacing: 7) {
            key("esc", .escape)
            key("↑", .up)
            key("↓", .down)
            key("tab", .tab)
            Spacer(minLength: 6)
            key("enter", .enter, primary: true)
        }
    }

    private func key(_ label: String, _ which: PhoneKey, primary: Bool = false) -> some View {
        let tint = primary ? KodColor.accent : KodColor.muted
        return Button {
            model.press(which)
        } label: {
            Text(label)
                .font(.system(size: 13, weight: .medium, design: .monospaced))
                .foregroundStyle(model.composer.busy ? KodColor.muted2 : tint)
                .frame(minWidth: 42, minHeight: 34)
                .padding(.horizontal, 6)
                .background(KodColor.card, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .stroke(primary ? KodColor.accent.opacity(0.35) : KodColor.hair, lineWidth: 1))
        }
        .buttonStyle(.plain)
        .disabled(model.composer.busy)
        .accessibilityLabel(which.rawValue)
    }

    @ViewBuilder
    private var send: some View {
        if model.composer.busy {
            // Something is on the wire. The control keeps its place rather than
            // disappearing, so nothing under the thumb moves mid-tap.
            ProgressView()
                .tint(KodColor.muted)
                .frame(width: 38, height: 38)
        } else {
            Button {
                model.sendDraft()
            } label: {
                Image(systemName: "arrow.up.circle.fill")
                    .font(.system(size: 30))
                    .foregroundStyle(model.composer.canSend ? KodColor.accent : KodColor.hair)
                    .frame(width: 38, height: 38)
            }
            .buttonStyle(.plain)
            .disabled(!model.composer.canSend)
            .accessibilityLabel("Send")
        }
    }

    // MARK: - Pieces

    @ViewBuilder
    private func banner(_ text: String, detail: String?, color: Color, heading: String? = nil) -> some View {
        KodCard(tint: color) {
            VStack(alignment: .leading, spacing: 7) {
                if let heading {
                    TierHeading(text: heading, color: color)
                }
                Text(text)
                    .font(.system(size: 15))
                    .foregroundStyle(KodColor.text)
                    .lineSpacing(3)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                if let detail {
                    Text(detail)
                        .font(KodFont.meta)
                        .foregroundStyle(KodColor.muted2)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }

    private var picker: some View {
        Menu {
            ForEach(model.pickable) { s in
                Button {
                    model.selectedSid = s.sid
                } label: {
                    Label("\(s.displayTitle) — \(s.project)", systemImage: s.needsYou ? "exclamationmark.circle" : "circle")
                }
            }
            if model.pickable.isEmpty {
                Text("No live sessions")
            }
        } label: {
            Image(systemName: "list.bullet")
                .foregroundStyle(KodColor.accent)
        }
    }
}

#if DEBUG
#Preview("Session") {
    NavigationStack { SessionView() }
        .environment(AppModel.preview())
        .preferredColorScheme(.dark)
}
#endif
