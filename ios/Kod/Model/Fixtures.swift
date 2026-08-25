//  Fixtures.swift — sample state, DEBUG only.
//
//  Exists so the Xcode canvas can show every tier at once (including the ones a
//  healthy machine rarely has) and so the tests have a realistic shape to plan
//  against. Never compiled into a release build.

#if DEBUG
import Foundation

enum Fixtures {
    static func session(
        _ sid: UInt64,
        _ project: String,
        _ title: String,
        phase: Phase = .busy,
        cli: Cli = .claude,
        ageMin: UInt64 = 3,
        last: String = "",
        pending: String? = nil,
        trouble: String? = nil,
        limitHit: Bool = false,
        limitPercent: Int? = nil,
        limitReset: String? = nil,
        alive: Bool = true,
        now: UInt64 = 1_700_000_000_000
    ) -> Session {
        Session(sid: sid,
                cli: cli,
                project: project,
                title: title,
                phase: phase,
                phaseSince: now - ageMin * 60_000,
                alive: alive,
                lastMessage: last,
                pendingHeadline: pending,
                trouble: trouble,
                limitHit: limitHit,
                limitPercent: limitPercent,
                limitReset: limitReset)
    }

    static let now: UInt64 = 1_700_000_000_000

    static var everyTier: [Session] {
        [
            session(1, "kod", "sign + notarize the app bundle", phase: .idle, ageMin: 41,
                    last: "Notarization succeeded. The stapler attached the ticket to Kod.app.",
                    limitHit: true, limitPercent: 100, limitReset: "3:00 PM"),
            session(2, "orchestrator", "bridge: websocket listener", phase: .awaiting, cli: .codex, ageMin: 26,
                    last: "I can bind the listener to 0.0.0.0 or keep it on loopback and require a tunnel.",
                    pending: "Bind the bridge to 0.0.0.0 so the phone can reach it directly? (y/n)"),
            session(3, "notes", "index rebuild", phase: .awaiting, ageMin: 4,
                    last: "The rebuild will drop and recreate the embeddings table.",
                    pending: "Overwrite the existing index at ~/.notes/index.sqlite?"),
            session(4, "kod", "standup tier ordering", phase: .busy, ageMin: 2,
                    last: "Running the store suite (115 tests)…"),
            session(5, "kod", "settings window polish", phase: .idle, cli: .codex, ageMin: 55),
            session(6, "site", "landing copy", phase: .busy, ageMin: 9,
                    last: "Rewrote the hero to lead with the two pain points."),
            session(7, "spikes", "gpui text input spike", phase: .spawning, cli: .shell, ageMin: 1),
            session(8, "notes", "eval harness", phase: .busy, ageMin: 17,
                    trouble: "cargo test exited 101 twice in a row"),
            session(9, "dotfiles", "zsh startup profiling", phase: .dead, ageMin: 300, alive: false),
        ]
    }

    static var allQuiet: [Session] {
        everyTier.filter { !$0.needsYou }
    }
}
#endif
