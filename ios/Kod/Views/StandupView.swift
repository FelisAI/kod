//  StandupView.swift — the home tab, and the whole point of the app.
//
//  It answers one question, in one screenful, in tier order: is anything stuck on
//  me? Blocked first (a wall — nothing else matters until it clears), then the
//  needs-you queue oldest first, then everything still running as ONE ambient
//  strip. The strip is deliberately not a list: a row per running session would
//  bury the two or three that actually want something under twenty that do not.

import SwiftUI

struct StandupView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let plan = model.standup

        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                header(plan)

                if !model.settings.isUsable && !model.connection.isConnected {
                    setupPrompt
                } else if !model.hasEverSynced && plan.attentionCount == 0 {
                    EmptyNote(title: "Waiting for the bridge",
                              detail: "Sessions appear as soon as the bridge answers.")
                }

                if !plan.blocked.isEmpty {
                    tier(heading: "⛔ BLOCKED", color: KodColor.red) {
                        ForEach(plan.blocked) { s in
                            AttentionCard(session: s,
                                          ageMs: model.age(since: s.phaseSince),
                                          tint: KodColor.red,
                                          leadLabel: "blocked") { model.open(s) }
                        }
                    }
                }

                if !plan.needsYou.isEmpty {
                    tier(heading: "⚠ NEEDS YOU", color: KodColor.amber) {
                        ForEach(plan.needsYou) { s in
                            AttentionCard(session: s,
                                          ageMs: model.age(since: s.phaseSince),
                                          tint: KodColor.amber,
                                          leadLabel: "waiting") { model.open(s) }
                        }
                    }
                }

                if !plan.live.isEmpty {
                    tier(heading: "● LIVE", color: KodColor.muted) {
                        ambientStrip(plan)
                    }
                }
            }
            .padding(16)
        }
        .background(KodColor.bg)
        .kodChrome(title: "Standup")
    }

    // MARK: - Pieces

    @ViewBuilder
    private func header(_ plan: StandupPlan) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(plan.isQuiet ? "All quiet" : headline(plan))
                .font(.system(size: 28, weight: .semibold))
                .foregroundStyle(plan.isQuiet ? KodColor.text : KodColor.strong)
            Text(plan.isQuiet ? "nothing needs you right now" : subhead(plan))
                .font(KodFont.body)
                .foregroundStyle(KodColor.muted)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.top, 4)
    }

    private func headline(_ plan: StandupPlan) -> String {
        let n = plan.attentionCount
        return n == 1 ? "1 needs you" : "\(n) need you"
    }

    private func subhead(_ plan: StandupPlan) -> String {
        var parts: [String] = []
        if !plan.blocked.isEmpty { parts.append("\(plan.blocked.count) blocked") }
        if !plan.needsYou.isEmpty { parts.append("\(plan.needsYou.count) waiting") }
        return parts.joined(separator: " · ")
    }

    @ViewBuilder
    private func tier<Content: View>(heading: String, color: Color, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            TierHeading(text: heading, color: color)
            content()
        }
    }

    /// The ambient strip: one dot per running session, then the sentence. Green is
    /// working, grey is idle — the only two states that do not want anything.
    @ViewBuilder
    private func ambientStrip(_ plan: StandupPlan) -> some View {
        Button {
            model.tab = .projects
        } label: {
            KodCard {
                VStack(alignment: .leading, spacing: 10) {
                    FlowLayout(spacing: 7, lineSpacing: 7) {
                        // Capped: past a few dozen dots the strip stops being a
                        // glance and becomes a wall, and the sentence carries the
                        // count anyway.
                        ForEach(plan.live.prefix(48)) { s in
                            PhaseDot(phase: s.phase, size: 8)
                        }
                        if plan.live.count > 48 {
                            MetaTag(text: "+\(plan.live.count - 48)")
                        }
                    }
                    Text(plan.ambientSentence)
                        .font(KodFont.body)
                        .foregroundStyle(KodColor.muted)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
        .buttonStyle(.plain)
    }

    private var setupPrompt: some View {
        KodCard(tint: KodColor.accent) {
            VStack(alignment: .leading, spacing: 10) {
                Text("Not connected")
                    .font(KodFont.cardTitle)
                    .foregroundStyle(KodColor.strong)
                Text("Point Kod at the bridge running on your Mac to see your sessions.")
                    .font(KodFont.body)
                    .foregroundStyle(KodColor.muted)
                    .fixedSize(horizontal: false, vertical: true)
                Button("Set up connection") { model.showConnectionSheet = true }
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(KodColor.bg)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 8)
                    .background(KodColor.accent, in: Capsule())
            }
        }
    }
}

#if DEBUG
#Preview("Standup — busy") {
    NavigationStack { StandupView() }
        .environment(AppModel.preview())
        .preferredColorScheme(.dark)
}

#Preview("Standup — all quiet") {
    NavigationStack { StandupView() }
        .environment(AppModel.preview(Fixtures.allQuiet))
        .preferredColorScheme(.dark)
}
#endif
