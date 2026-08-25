//  KodApp.swift — entry point.
//
//  The socket is dropped on background and redialled on foreground. iOS would
//  kill it within seconds anyway; doing it deliberately means the app comes back
//  with a fresh attach (and a fresh epoch) instead of a stale cache that looks
//  live for the first second the user is looking at it.

import SwiftUI

@main
struct KodApp: App {
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(model)
                .preferredColorScheme(.dark)
                .tint(KodColor.accent)
        }
        .onChange(of: scenePhase) { _, phase in
            switch phase {
            case .active: model.start()
            case .background: model.stop()
            default: break
            }
        }
    }
}
