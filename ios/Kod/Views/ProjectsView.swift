//  ProjectsView.swift — every project, in an order that does not move.
//
//  EXACTLY two sections: ACTIVE (has a live session) and PROJECTS (does not).
//  Inside ACTIVE, projects with a needs-you session float to the top and the rest
//  are alphabetical. There is no recency sort anywhere on this screen on purpose:
//  ordering by "most recently updated" makes rows swap under the thumb every time
//  an agent prints a line, which is how a status list becomes unusable.

import SwiftUI

struct ProjectsView: View {
    @Environment(AppModel.self) private var model
    @State private var expanded: Set<String> = []

    var body: some View {
        let plan = model.projects

        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                if plan.isEmpty {
                    EmptyNote(title: "No projects yet",
                              detail: model.connection.isConnected
                                  ? "The bridge reported no sessions."
                                  : "Connect to your Mac to see them.")
                }

                if !plan.active.isEmpty {
                    section("ACTIVE", groups: plan.active)
                }
                if !plan.rest.isEmpty {
                    section("PROJECTS", groups: plan.rest)
                }
            }
            .padding(16)
        }
        .background(KodColor.bg)
        .kodChrome(title: "Projects")
    }

    @ViewBuilder
    private func section(_ heading: String, groups: [ProjectGroup]) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            TierHeading(text: heading, color: KodColor.muted)
            ForEach(groups) { group in
                projectCard(group)
            }
        }
    }

    @ViewBuilder
    private func projectCard(_ group: ProjectGroup) -> some View {
        let isOpen = expanded.contains(group.project)

        KodCard(tint: group.attentionCount > 0 ? KodColor.amber : nil) {
            VStack(alignment: .leading, spacing: 0) {
                Button {
                    // Expand in place rather than push a screen: the whole value of
                    // this tab is seeing the shape of everything at once.
                    if isOpen { expanded.remove(group.project) } else { expanded.insert(group.project) }
                } label: {
                    HStack(spacing: 10) {
                        VStack(alignment: .leading, spacing: 5) {
                            HStack(spacing: 7) {
                                ProjectPill(slug: group.project)
                                if group.attentionCount > 0 {
                                    Text(group.attentionCount == 1 ? "needs you" : "\(group.attentionCount) need you")
                                        .font(KodFont.pill)
                                        .foregroundStyle(KodColor.amber)
                                }
                            }
                            // The pill now shows only the basename, so the parent
                            // (owner, or containing folder) rides along here —
                            // otherwise two projects called "app" are the same row.
                            HStack(spacing: 6) {
                                let parent = ProjectName.qualifier(group.project)
                                if !parent.isEmpty {
                                    MetaTag(text: parent)
                                    Text("·").font(KodFont.meta).foregroundStyle(KodColor.muted2)
                                }
                                MetaTag(text: group.subtitle)
                            }
                        }
                        Spacer(minLength: 6)
                        Image(systemName: isOpen ? "chevron.down" : "chevron.right")
                            .font(.system(size: 12, weight: .semibold))
                            .foregroundStyle(KodColor.muted2)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)

                if isOpen {
                    Divider()
                        .overlay(KodColor.hair)
                        .padding(.top, 10)
                    ForEach(group.sessions) { s in
                        SessionRow(session: s, ageMs: model.age(since: s.phaseSince)) { model.open(s) }
                        if s.id != group.sessions.last?.id {
                            Divider().overlay(KodColor.hair.opacity(0.5))
                        }
                    }
                }
            }
        }
    }
}

#if DEBUG
#Preview("Projects") {
    NavigationStack { ProjectsView() }
        .environment(AppModel.preview())
        .preferredColorScheme(.dark)
}
#endif
