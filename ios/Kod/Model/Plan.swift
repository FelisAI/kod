//  Plan.swift — what Standup and Projects show, decided as pure data.
//
//  Kept out of the views so the ordering rules ("oldest first", "never by
//  recency") are testable and can't drift between two screens that both claim to
//  know what needs the user.

import Foundation

enum TimeFmt {
    /// "12m", "3h", "2d" — for a badge next to a title, where "ago" is implied.
    static func compact(_ ageMs: UInt64) -> String {
        let mins = ageMs / 60_000
        if mins < 1 { return "<1m" }
        if mins < 60 { return "\(mins)m" }
        if mins < 60 * 24 { return "\(mins / 60)h" }
        return "\(mins / (60 * 24))d"
    }

    /// "just now", "12m ago", "3h ago", "2d ago" — matching the desktop's wording.
    static func ago(_ ageMs: UInt64) -> String {
        let mins = ageMs / 60_000
        if mins < 1 { return "just now" }
        if mins < 60 { return "\(mins)m ago" }
        if mins < 60 * 24 { return "\(mins / 60)h ago" }
        return "\(mins / (60 * 24))d ago"
    }

    /// Saturating, so a phone clock a few seconds behind the Mac reads "just now"
    /// instead of underflowing UInt64 into a 584-million-year age.
    static func age(since ts: UInt64, now: UInt64) -> UInt64 { now >= ts ? now - ts : 0 }
}

/// Turning a storage KEY into something worth putting on a phone screen.
///
/// The bridge sends the canonical project key the daemon threads sessions by —
/// "path:/Users/me/local/orchestrator", "github:owner/repo". That is an identity,
/// not a name: it is unreadable at pill size, it eats the whole row width, and it
/// puts the user's home directory on a screen they might hold up in a cafe.
///
/// The desktop shows the basename (or the name the user typed), so this shows the
/// basename too. It deliberately does NOT try to reproduce a user-set display
/// name: the bridge does not carry one, and inventing a second naming rule that
/// disagrees with the Mac would be worse than being plainly consistent.
enum ProjectName {
    /// "path:/a/b/orchestrator" -> "orchestrator", "github:owner/repo" -> "repo".
    /// Anything unrecognised is returned untouched rather than mangled.
    static func short(_ key: String) -> String {
        let body: String
        if let r = key.range(of: "path:", options: .anchored) {
            body = String(key[r.upperBound...])
        } else if let r = key.range(of: "github:", options: .anchored) {
            body = String(key[r.upperBound...])
        } else {
            body = key
        }
        // split(separator:) drops empty components, so a trailing slash is handled
        // and a body that is nothing BUT slashes yields no components at all — in
        // which case there is no name to show and the raw key is the honest label.
        guard let last = body.split(separator: "/").last else { return key }
        return String(last)
    }

    /// The owner/parent, shown next to the short name so two repos that share a
    /// basename stay tellable apart. Empty when there is nothing to disambiguate.
    static func qualifier(_ key: String) -> String {
        if let r = key.range(of: "github:", options: .anchored) {
            let parts = key[r.upperBound...].split(separator: "/")
            return parts.count >= 2 ? String(parts[parts.count - 2]) : ""
        }
        if let r = key.range(of: "path:", options: .anchored) {
            let parts = key[r.upperBound...].split(separator: "/")
            return parts.count >= 2 ? String(parts[parts.count - 2]) : ""
        }
        return ""
    }
}

/// The home screen's three tiers, in the order they are read.
struct StandupPlan: Equatable {
    /// limit_hit — the wall. Nothing else the user does matters until it clears.
    var blocked: [Session] = []
    /// phase == awaiting, OLDEST FIRST: the one that has been stuck longest is the
    /// one costing the most, and it must not sink as newer ones arrive.
    var needsYou: [Session] = []
    /// Everything else alive. Rendered as ONE ambient strip, never one row each.
    var live: [Session] = []

    var busy = 0
    var idle = 0
    var starting = 0
    var projectCount = 0

    var attentionCount: Int { blocked.count + needsYou.count }
    var isQuiet: Bool { attentionCount == 0 }

    /// "6 working · 5 idle across 9 projects".
    var ambientSentence: String {
        var parts: [String] = []
        if busy > 0 { parts.append("\(busy) working") }
        if idle > 0 { parts.append("\(idle) idle") }
        if starting > 0 { parts.append("\(starting) starting") }
        if parts.isEmpty { return "nothing else running" }
        let noun = projectCount == 1 ? "project" : "projects"
        return parts.joined(separator: " · ") + " across \(projectCount) \(noun)"
    }

    init() {}

    init(sessions: [Session]) {
        let alive = sessions.filter { $0.alive && $0.phase != .dead }
        blocked = alive.filter { $0.limitHit }.sorted(by: Self.oldestFirst)
        needsYou = alive.filter { !$0.limitHit && $0.phase == .awaiting }.sorted(by: Self.oldestFirst)
        live = alive.filter { !$0.limitHit && $0.phase != .awaiting }.sorted { $0.sid < $1.sid }

        busy = live.filter { $0.phase == .busy }.count
        idle = live.filter { $0.phase == .idle }.count
        starting = live.filter { $0.phase == .spawning }.count
        projectCount = Set(live.map(\.project)).count
    }

    /// phase_since ascending, sid as the tie-break so equal timestamps can't make
    /// two rows swap places between frames.
    private static func oldestFirst(_ a: Session, _ b: Session) -> Bool {
        a.phaseSince == b.phaseSince ? a.sid < b.sid : a.phaseSince < b.phaseSince
    }
}

/// One project's sessions, plus the counts its row shows.
struct ProjectGroup: Identifiable, Equatable {
    let project: String
    let sessions: [Session]

    var id: String { project }

    var liveCount: Int { sessions.filter { $0.alive && $0.phase != .dead }.count }
    var busyCount: Int { sessions.filter { $0.alive && $0.phase == .busy }.count }
    var idleCount: Int { sessions.filter { $0.alive && $0.phase == .idle }.count }
    var attentionCount: Int { sessions.filter(\.needsYou).count }
    var hasLive: Bool { liveCount > 0 }

    /// "4 sessions · 2 working · 1 idle" — counts only. Deliberately no "updated
    /// 2m ago": the moment a row shows recency, the list starts flapping.
    var subtitle: String {
        var parts = ["\(sessions.count) session\(sessions.count == 1 ? "" : "s")"]
        if busyCount > 0 { parts.append("\(busyCount) working") }
        if idleCount > 0 { parts.append("\(idleCount) idle") }
        if !hasLive { parts.append("no live session") }
        return parts.joined(separator: " · ")
    }
}

/// Exactly two sections, in this order. No third bucket, no recency sort.
struct ProjectsPlan: Equatable {
    /// Projects with a live session. Within it, the ones with a needs-you session
    /// float to the top; everything else is alphabetical and therefore still.
    var active: [ProjectGroup] = []
    /// Everything else, alphabetical.
    var rest: [ProjectGroup] = []

    var isEmpty: Bool { active.isEmpty && rest.isEmpty }

    init() {}

    init(sessions: [Session]) {
        var byProject: [String: [Session]] = [:]
        for s in sessions {
            let key = s.project.isEmpty ? "(no project)" : s.project
            byProject[key, default: []].append(s)
        }
        let groups = byProject.map { key, list in
            ProjectGroup(project: key, sessions: list.sorted(by: Self.withinProject))
        }
        active = groups.filter(\.hasLive).sorted(by: Self.activeOrder)
        rest = groups.filter { !$0.hasLive }.sorted { Self.name($0) < Self.name($1) }
    }

    /// Needs-you projects first, then alphabetical. Two keys only — a third
    /// (counts, freshness) would reintroduce movement for no information.
    private static func activeOrder(_ a: ProjectGroup, _ b: ProjectGroup) -> Bool {
        let aWants = a.attentionCount > 0, bWants = b.attentionCount > 0
        if aWants != bWants { return aWants }
        return name(a) < name(b)
    }

    private static func withinProject(_ a: Session, _ b: Session) -> Bool {
        if a.needsYou != b.needsYou { return a.needsYou }
        let aLive = a.alive && a.phase != .dead, bLive = b.alive && b.phase != .dead
        if aLive != bLive { return aLive }
        return a.sid < b.sid
    }

    private static func name(_ g: ProjectGroup) -> String { g.project.lowercased() }
}
