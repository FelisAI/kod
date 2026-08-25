//  Cards.swift — the two ways a session appears outside its own tab.
//
//  A CARD when it wants something from the user (Standup's blocked / needs-you
//  tiers): big enough to answer "what is it asking?" without opening it.
//  A ROW when it is merely inventory (a project's session list, the picker).

import SwiftUI

struct AttentionCard: View {
    let session: Session
    /// How long it has been in this phase, already measured against the model's
    /// clock so every card on screen agrees.
    let ageMs: UInt64
    let tint: Color
    let leadLabel: String
    let onTap: () -> Void

    var body: some View {
        Button(action: onTap) {
            KodCard(tint: tint) {
                VStack(alignment: .leading, spacing: 9) {
                    HStack(alignment: .firstTextBaseline, spacing: 10) {
                        Text(session.displayTitle)
                            .font(KodFont.cardTitle)
                            .foregroundStyle(KodColor.strong)
                            .lineLimit(2)
                            .multilineTextAlignment(.leading)
                        Spacer(minLength: 4)
                        Text("\(leadLabel) \(TimeFmt.compact(ageMs))")
                            .font(KodFont.meta)
                            .monospacedDigit()
                            .foregroundStyle(tint)
                            .fixedSize()
                    }

                    HStack(spacing: 7) {
                        ProjectPill(slug: session.project)
                        MetaTag(text: session.cli.label)
                    }

                    if session.limitHit {
                        MetaTag(text: LimitLine.text(session), color: KodColor.red)
                    }

                    // The whole reason the card exists: what it is waiting on, in
                    // words, wrapping. Truncating this to one line would send the
                    // user to the Mac to read a sentence.
                    if let headline = session.pendingHeadline {
                        Text(headline)
                            .font(KodFont.body)
                            .foregroundStyle(KodColor.text)
                            .multilineTextAlignment(.leading)
                            .lineLimit(4)
                            .fixedSize(horizontal: false, vertical: true)
                    }

                    if let trouble = session.trouble {
                        Text(trouble)
                            .font(KodFont.meta)
                            .foregroundStyle(KodColor.red)
                            .lineLimit(2)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
        }
        .buttonStyle(.plain)
    }
}

struct SessionRow: View {
    let session: Session
    let ageMs: UInt64
    let onTap: () -> Void

    var body: some View {
        Button(action: onTap) {
            HStack(spacing: 10) {
                PhaseDot(phase: session.phase)
                VStack(alignment: .leading, spacing: 2) {
                    Text(session.displayTitle)
                        .font(.system(size: 14, weight: session.needsYou ? .semibold : .regular))
                        .foregroundStyle(session.needsYou ? KodColor.strong : KodColor.text)
                        .lineLimit(1)
                    MetaTag(text: "\(session.cli.label) · \(session.phase.label)")
                }
                Spacer(minLength: 6)
                Text(TimeFmt.compact(ageMs))
                    .font(KodFont.meta)
                    .monospacedDigit()
                    .foregroundStyle(session.needsYou ? KodColor.amber : KodColor.muted2)
                Image(systemName: "chevron.right")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(KodColor.muted2)
            }
            .padding(.vertical, 7)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

/// "limit hit · 92% · resets 3:00 PM" — one spelling, used by the card and the
/// Session tab so a percentage never appears two different ways.
enum LimitLine {
    static func text(_ s: Session) -> String {
        var parts = ["limit hit"]
        if let p = s.limitPercent { parts.append("\(p)%") }
        if let r = s.limitReset { parts.append("resets \(r)") }
        return parts.joined(separator: " · ")
    }
}
