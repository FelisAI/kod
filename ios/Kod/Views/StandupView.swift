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
                    EmptyNote(title: "Waiting for your Mac",
                              detail: "Sessions appear as soon as Kod on your Mac answers.")
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
        let (title, sub, loud) = headerText(plan)
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.system(size: 28, weight: .semibold))
                .foregroundStyle(loud ? KodColor.strong : KodColor.text)
            Text(sub)
                .font(KodFont.body)
                .foregroundStyle(KodColor.muted)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.top, 4)
    }

    /// "All quiet" is a claim about sessions, so it is only made about sessions
    /// the Mac has actually sent. With nothing synced — never paired, still
    /// connecting, or a dropped link that flushed the cache — it used to say "All
    /// quiet · nothing needs you right now" in the largest type on screen, while
    /// three agents might be waiting on a Mac it could not reach.
    private func headerText(_ plan: StandupPlan) -> (title: String, sub: String, loud: Bool) {
        if !model.hasEverSynced {
            if !model.settings.isUsable {
                return ("Kod Remote", "your coding agents, from your phone", false)
            }
            if case .connecting = model.connection {
                return ("Connecting…", "to Kod on your Mac", false)
            }
            return ("Not connected", "nothing here is current until your Mac answers", false)
        }
        if plan.isQuiet { return ("All quiet", "nothing needs you right now", false) }
        return (headline(plan), subhead(plan), true)
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

    /// The ambient strip: one dot per running session, then the sentence. Orange
    /// is working and green is idle, as on the Mac — the states that are not
    /// waiting on a decision.
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

    /// What a phone that has never been paired sees — which includes App Review,
    /// opening it cold with no Mac. So it says what the app is for, how to pair,
    /// and offers a way to see it working without one.
    private var setupPrompt: some View {
        KodCard(tint: KodColor.accent) {
            VStack(alignment: .leading, spacing: 10) {
                Text("Pair with your Mac")
                    .font(KodFont.cardTitle)
                    .foregroundStyle(KodColor.strong)
                Text("Kod Remote shows the coding agents running in Kod on your Mac — which ones need you — and lets you answer them from here.")
                    .font(KodFont.body)
                    .foregroundStyle(KodColor.muted)
                    .fixedSize(horizontal: false, vertical: true)
                Text("On your Mac, open Kod › Settings › Mobile, turn on “Serve my sessions to my phone”, then scan the code it shows.")
                    .font(KodFont.meta)
                    .foregroundStyle(KodColor.muted2)
                    .fixedSize(horizontal: false, vertical: true)
                HStack(spacing: 10) {
                    Button("Pair") { model.showConnectionSheet = true }
                        .font(.system(size: 14, weight: .semibold))
                        .foregroundStyle(KodColor.bg)
                        .padding(.horizontal, 16)
                        .padding(.vertical, 8)
                        .background(KodColor.accent, in: Capsule())
                        .accessibilityIdentifier("pair")
                    Button("Explore with sample data") { model.enterDemo() }
                        .font(.system(size: 14, weight: .semibold))
                        .foregroundStyle(KodColor.accent)
                        .padding(.horizontal, 14)
                        .padding(.vertical, 8)
                        .overlay(Capsule().stroke(KodColor.accent.opacity(0.5), lineWidth: 1))
                        .accessibilityIdentifier("explore-sample-data")
                }
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
