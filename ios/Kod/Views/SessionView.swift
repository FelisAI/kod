//  SessionView.swift — one session, to READ.
//
//  v0 has no terminal grid and no input box, and that is a product decision, not a
//  missing feature: on a phone the useful thing is the sentence the agent just
//  said and the question it is waiting on, as native wrapping text you can select
//  and scroll. A 80x24 character grid squeezed onto a 390pt screen is unreadable,
//  and caps.input is false, so there is nothing to type into.

import SwiftUI

struct SessionView: View {
    @Environment(AppModel.self) private var model

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
                           detail: model.inputAllowed ? nil : "Answer it in Kod on your Mac — this app is read-only.",
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

                MetaTag(text: "session \(s.sid) · read-only in v0")
            }
            .padding(16)
        }
    }

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
