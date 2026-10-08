//  Fixtures.swift — sample state, SHIPPED.
//
//  Two jobs. The Xcode canvas and the tests use it to show every tier at once,
//  including the ones a healthy machine rarely has. And it is the app's sample
//  data: without a Mac running Kod this app is a connection screen and nothing
//  else, which is unevaluable for anyone deciding whether to set it up — App
//  Review included. So it compiles into every build, and "Explore with sample
//  data" puts it on screen behind a banner that says what it is.
//
//  Everything here is made up. No name, path or project in it is anyone's.

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

    // Project keys in the two shapes the Mac really sends.
    private static let storefront = "github:acme/storefront"
    private static let payments = "github:acme/payments-api"
    private static let iosApp = "path:/Users/sam/code/ios-app"
    private static let blog = "path:/Users/sam/code/blog"
    private static let dotfiles = "github:sam/dotfiles"

    /// The session sample data opens on: the one that has waited longest.
    static let firstToAnswer: UInt64 = 2

    static var everyTier: [Session] {
        [
            session(1, iosApp, "Dark mode for the settings screens", phase: .idle, ageMin: 32,
                    last: "SettingsView and AccountView are done. NotificationsView and PrivacyView are next.",
                    limitHit: true, limitPercent: 100, limitReset: "4:00 PM"),
            session(2, storefront, "Move checkout to the new payments API", phase: .awaiting, ageMin: 14,
                    last: "The two new columns and the backfill script are ready. Next is running the migration on staging.",
                    pending: "npm run db:migrate -- --env staging"),
            session(3, payments, "Fix the flaky webhook retry test", phase: .awaiting, cli: .codex, ageMin: 6,
                    last: "The test races the retry timer: the third attempt can back off for 60s, and the test only waits 30s.",
                    pending: "Should the retry backoff cap at 30s, as the docs say, or stay at 60s?"),
            session(4, storefront, "Product search with Postgres full-text", phase: .busy, ageMin: 3,
                    last: "Running the search test suite (48 tests)…"),
            session(5, payments, "Upgrade to Rust 1.90", phase: .busy, cli: .codex, ageMin: 8,
                    last: "cargo build is green. Fixing two new clippy lints."),
            session(6, blog, "Release notes for v2.3", phase: .idle, ageMin: 22,
                    last: "The draft is in posts/v2-3.md: 640 words in three sections."),
            session(7, iosApp, "zsh", phase: .idle, cli: .shell, ageMin: 50),
            session(8, storefront, "Review PR #482", phase: .busy, cli: .codex, ageMin: 1,
                    trouble: "overloaded"),
            session(9, dotfiles, "Profile slow shell startup", phase: .dead, ageMin: 300,
                    last: "Startup went from 820ms to 140ms. The culprit was nvm's eager load.",
                    alive: false),
        ]
    }

    static var allQuiet: [Session] {
        everyTier.filter { !$0.needsYou }
    }

    // MARK: - Terminals

    /// A terminal for a sample session, the way the bridge would send one: plain
    /// rows at the session's real width, trailing blank rows dropped.
    static func grid(for sid: UInt64) -> TerminalGrid? {
        let lines: [String]
        switch sid {
        case 2: lines = claudeAsking
        case 3: lines = codexAsking
        case 4: lines = claudeTesting
        case 7: lines = shellPrompt
        default: return nil
        }
        return TerminalGrid(sid: sid, cols: 80, rows: 24, lines: lines,
                            cursorRow: nil, cursorCol: nil)
    }

    /// What the same session looks like once it has been answered.
    static func gridAfterAnswer(for sid: UInt64) -> TerminalGrid? {
        let lines: [String]
        switch sid {
        case 2:
            lines = Array(claudeAsking.prefix(7)) + [
                "  ⎿  Running…",
                "",
                "✻ Migrating staging… (4s · esc to interrupt)",
            ]
        case 3:
            lines = Array(codexAsking.prefix(9)) + [
                "",
                "• Working (3s • esc to interrupt)",
            ]
        default:
            return grid(for: sid)
        }
        return TerminalGrid(sid: sid, cols: 80, rows: 24, lines: lines,
                            cursorRow: nil, cursorCol: nil)
    }

    private static let claudeAsking: [String] = [
        "> Move checkout to the new payments API",
        "",
        "⏺ The two new columns and the backfill script are ready. Next is running",
        "  the migration on staging.",
        "",
        "⏺ Bash(npm run db:migrate -- --env staging)",
        "",
    ] + box([
        "Bash command",
        "",
        "  npm run db:migrate -- --env staging",
        "  Apply the two pending migrations to the staging database",
        "",
        "Do you want to proceed?",
        "❯ 1. Yes",
        "  2. Yes, and don't ask again for npm run db:migrate commands",
        "  3. No, and tell Claude what to do differently (esc)",
    ])

    private static let codexAsking: [String] = [
        "› Fix the flaky webhook retry test",
        "",
        "• Read tests/webhook_retry.rs, src/webhooks/retry.rs",
        "",
        "• The test races the retry timer: the third attempt can back off for",
        "  60s, and the test only advances the clock by 30s.",
        "",
        "  1. Cap the backoff at 30s, which is what the docs promise.",
        "  2. Keep 60s and advance the test clock further.",
        "",
        "  Should the retry backoff cap at 30s, as the docs say, or stay at 60s?",
        "",
        "▌ ",
    ]

    private static let claudeTesting: [String] = [
        "> Add product search backed by Postgres full-text search",
        "",
        "⏺ Search now ranks title matches above description matches. Running",
        "  the suite.",
        "",
        "⏺ Bash(npm test -- search)",
        "  ⎿  PASS  src/search/query.test.ts (31 tests)",
        "     PASS  src/search/rank.test.ts (9 tests)",
        "     RUNS  src/search/index.test.ts",
        "",
        "✻ Running the search test suite… (1m 12s · esc to interrupt)",
    ]

    private static let shellPrompt: [String] = [
        "sam@mac ios-app % git status --short",
        " M Sources/Settings/SettingsView.swift",
        " M Sources/Settings/AccountView.swift",
        "?? Sources/Settings/Palette+Dark.swift",
        "sam@mac ios-app % ",
    ]

    /// A rounded box at one fixed width, the way claude draws its dialogs. Built
    /// rather than typed so every row is the same width and the edges line up.
    private static func box(_ body: [String]) -> [String] {
        let inner = body.map(\.count).max() ?? 0
        let rule = String(repeating: "─", count: inner + 2)
        let rows = body.map { "│ " + $0 + String(repeating: " ", count: inner - $0.count) + " │" }
        return ["╭" + rule + "╮"] + rows + ["╰" + rule + "╯"]
    }
}
