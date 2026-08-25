//  Theme.swift — Kod's Focused Dark palette, on the phone.
//
//  Same hexes as the desktop app's theme.rs, to the digit. There is no light
//  theme: the app is for glancing at a screen in a dim room, and a second palette
//  would be a second set of contrast bugs.

import SwiftUI

enum KodColor {
    static let bg = Color(hex: 0x0F1218)
    static let panel = Color(hex: 0x141820)
    static let card = Color(hex: 0x191D27)
    static let hair = Color(hex: 0x2B303B)
    static let text = Color(hex: 0xD3D8E1)
    static let strong = Color(hex: 0xF2F5FA)
    static let muted = Color(hex: 0x9AA3B1)
    static let muted2 = Color(hex: 0x757E8A)
    static let accent = Color(hex: 0x7EE2C0)
    static let amber = Color(hex: 0xE6C07A)
    static let green = Color(hex: 0x5BB99B)
    static let red = Color(hex: 0xE68A8A)

    /// The colour a phase dot takes. Busy is green because green means "running";
    /// idle is grey because an idle session is not news, it is furniture.
    static func phase(_ p: Phase) -> Color {
        switch p {
        case .busy: return green
        case .awaiting: return amber
        case .idle: return muted2
        case .spawning: return muted2
        case .dead, .unknown: return hair
        }
    }
}

extension Color {
    init(hex: UInt32) {
        self.init(
            .sRGB,
            red: Double((hex >> 16) & 0xFF) / 255,
            green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255,
            opacity: 1
        )
    }
}

enum KodFont {
    static let tierHeading = Font.system(size: 12, weight: .semibold)
    static let cardTitle = Font.system(size: 16, weight: .semibold)
    static let body = Font.system(size: 14)
    static let meta = Font.system(size: 12)
    static let pill = Font.system(size: 11, weight: .medium)
}
