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
            .safeAreaInset(edge: .top, spacing: 0) { ConnectionBanner() }
    }
}

extension View {
    func kodChrome(title: String) -> some View { modifier(KodChrome(title: title)) }
}
