//  RootView.swift — Standup · Projects · Session, in that order.
//
//  The order is the argument: Standup is home because the question the phone is
//  for is "does anything need me?", Projects is the map you fall back to, and
//  Session is where you land after tapping something — never where you start.

import SwiftUI

struct RootView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var m = model

        TabView(selection: $m.tab) {
            NavigationStack { StandupView() }
                .tabItem { Label("Standup", systemImage: "list.bullet") }
                .badge(model.attentionCount)
                .tag(RootTab.standup)

            NavigationStack { ProjectsView() }
                .tabItem { Label("Projects", systemImage: "square.grid.2x2") }
                .tag(RootTab.projects)

            NavigationStack { SessionView() }
                .tabItem { Label("Session", systemImage: "text.bubble") }
                .tag(RootTab.session)
        }
        .toolbarBackground(KodColor.panel, for: .tabBar)
        .toolbarBackground(.visible, for: .tabBar)
        .sheet(isPresented: $m.showConnectionSheet) {
            ConnectionView()
        }
    }
}
