//  Atoms.swift — the handful of pieces every screen is built from.
//
//  Deliberately dependency-free apart from the palette: nothing here reads
//  AppModel, which is what lets all three tabs share them without a cycle.

import SwiftUI

/// "⛔ BLOCKED", "⚠ NEEDS YOU", "● LIVE", "ACTIVE" — one look for all of them.
struct TierHeading: View {
    let text: String
    var color: Color = KodColor.muted

    var body: some View {
        Text(text)
            .font(KodFont.tierHeading)
            .tracking(1.4)
            .foregroundStyle(color)
            .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// A project's tag. Hue is a stable hash of the slug, so a project looks the same
/// on every screen and in every session of the app — ported from the desktop.
struct ProjectPill: View {
    let slug: String

    private static let hues: [UInt32] = [0x7EE2C0, 0xE6C07A, 0x8AB4F8, 0xC896E6, 0xF09696, 0x8FD2AE, 0x8FBEDC]

    static func hue(for slug: String) -> Color {
        var h: UInt32 = 2_166_136_261
        for b in slug.utf8 { h = (h ^ UInt32(b)) &* 16_777_619 }
        return Color(hex: hues[Int(h % UInt32(hues.count))])
    }

    var body: some View {
        // Hue on the FULL key, label with the short name: two projects can share a
        // basename ("app" under two roots) and must not also share a colour.
        let c = Self.hue(for: slug)
        Text(ProjectName.short(slug))
            .font(KodFont.pill)
            .foregroundStyle(c)
            .lineLimit(1)
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background(c.opacity(0.13), in: RoundedRectangle(cornerRadius: 5, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 5, style: .continuous).stroke(c.opacity(0.28), lineWidth: 1))
    }
}

struct PhaseDot: View {
    let phase: Phase
    var size: CGFloat = 7

    var body: some View {
        Circle()
            .fill(KodColor.phase(phase))
            .frame(width: size, height: size)
    }
}

/// A quiet label — cli name, phase, age. Text, not chrome.
struct MetaTag: View {
    let text: String
    var color: Color = KodColor.muted2

    var body: some View {
        Text(text)
            .font(KodFont.meta)
            .foregroundStyle(color)
            .lineLimit(1)
    }
}

/// The card every tier row sits in. `tint` paints the left edge, which is how a
/// blocked card reads as blocked from across the room.
struct KodCard<Content: View>: View {
    var tint: Color?
    @ViewBuilder var content: Content

    var body: some View {
        content
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(12)
            .background(KodColor.card, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            .overlay(alignment: .leading) {
                if let tint {
                    UnevenRoundedRectangle(topLeadingRadius: 12, bottomLeadingRadius: 12, style: .continuous)
                        .fill(tint)
                        .frame(width: 3)
                }
            }
            .overlay {
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .stroke(tint?.opacity(0.30) ?? KodColor.hair, lineWidth: 1)
            }
    }
}

/// Wrapping row of equal-ish items — used for the LIVE strip's dots, where a
/// fixed grid would leave a ragged hole and an HStack would clip at 40 sessions.
struct FlowLayout: Layout {
    var spacing: CGFloat = 6
    var lineSpacing: CGFloat = 6

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout Void) -> CGSize {
        let maxW = proposal.width ?? .greatestFiniteMagnitude
        var x: CGFloat = 0, y: CGFloat = 0, lineH: CGFloat = 0, widest: CGFloat = 0
        for s in subviews {
            let sz = s.sizeThatFits(.unspecified)
            if x > 0, x + sz.width > maxW {
                x = 0
                y += lineH + lineSpacing
                lineH = 0
            }
            x += sz.width + spacing
            lineH = max(lineH, sz.height)
            widest = max(widest, x - spacing)
        }
        return CGSize(width: min(widest, maxW), height: y + lineH)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout Void) {
        let maxW = bounds.width
        var x: CGFloat = 0, y: CGFloat = 0, lineH: CGFloat = 0
        for s in subviews {
            let sz = s.sizeThatFits(.unspecified)
            if x > 0, x + sz.width > maxW {
                x = 0
                y += lineH + lineSpacing
                lineH = 0
            }
            s.place(at: CGPoint(x: bounds.minX + x, y: bounds.minY + y), proposal: ProposedViewSize(sz))
            x += sz.width + spacing
            lineH = max(lineH, sz.height)
        }
    }
}

/// Centred nothing-to-see state.
struct EmptyNote: View {
    let title: String
    var detail: String?

    var body: some View {
        VStack(spacing: 6) {
            Text(title)
                .font(.system(size: 17, weight: .semibold))
                .foregroundStyle(KodColor.muted)
            if let detail {
                Text(detail)
                    .font(KodFont.body)
                    .foregroundStyle(KodColor.muted2)
                    .multilineTextAlignment(.center)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 36)
    }
}
