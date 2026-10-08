//  Chrome.swift — the frame all three tabs share.
//
//  One modifier so the dark nav bar, the connection chip and the trouble banner
//  can never drift apart between tabs.

import SwiftUI

struct KodChrome: ViewModifier {
    let title: String

    func body(content: Content) -> some View {
        content
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbarBackground(KodColor.panel, for: .navigationBar)
            .toolbarBackground(.visible, for: .navigationBar)
            .toolbarColorScheme(.dark, for: .navigationBar)
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) { ConnectionChip() }
            }
            .safeAreaInset(edge: .top, spacing: 0) {
                VStack(spacing: 0) {
                    DemoBanner()
                    ConnectionBanner()
                }
            }
    }
}

/// Always on screen while the sample data is: a demo that does not announce
/// itself is a lie, and the way back out has to be one tap from every tab.
struct DemoBanner: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        if model.demoMode && !model.demoChromeHidden {
            HStack(spacing: 8) {
                Image(systemName: "sparkles")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(KodColor.accent)
                Text("Sample data — not from your Mac. Typing here is answered by the app.")
                    .font(KodFont.meta)
                    .foregroundStyle(KodColor.muted)
                    .lineLimit(2)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 6)
                Button("Exit") { model.exitDemo() }
                    .font(KodFont.pill)
                    .foregroundStyle(KodColor.accent)
                    .accessibilityIdentifier("exit-sample-data")
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 7)
            .frame(maxWidth: .infinity)
            .background(KodColor.panel)
            .overlay(alignment: .bottom) { Rectangle().fill(KodColor.hair).frame(height: 1) }
        }
    }
}

extension View {
    func kodChrome(title: String) -> some View { modifier(KodChrome(title: title)) }
}
