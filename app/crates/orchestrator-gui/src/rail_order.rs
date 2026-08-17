use gpui::*;
use crate::*;

/// The drop-target wash (ACCENT at ~12%) — a bg tint rather than a border, so a
/// hovered row never shifts by a pixel mid-drag.
const DROP_TINT: u32 = 0x7EE2C01F;

/// The drag payload: GPUI matches a drop to its listeners by the payload's
/// TypeId, so this type IS the "a rail project is in flight" signal.
pub(crate) struct ProjectDrag {
    pub(crate) slug: String,
    pub(crate) name: String,
    /// which SECTION the row was picked up from (ACTIVE vs PROJECTS). Drives the
    /// drop-target WASH only: a cross-section target shows no tint, so a drop
    /// that `reorder_view` will refuse never advertises itself as legal. The
    /// authoritative refusal lives at the choke point (`reorder_project`), not
    /// here — a payload flag can go stale mid-drag when a session dies.
    pub(crate) active: bool,
}

/// The pill that follows the cursor while a rail row is dragged (GPUI renders
/// the ghost from an entity of its own). Mirrors the row it was picked up from.
pub(crate) struct RailGhost {
    slug: String,
    name: String,
}

impl Render for RailGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(9.))
            .px(px(8.))
            .py(px(5.))
            .rounded(px(8.))
            .bg(rgb(CARD2))
            .border_1()
            .border_color(rgb(0x346B54))
            .opacity(0.92)
            .child(project_badge(&self.name, &self.slug, 20.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT_STRONG))
                    .child(SharedString::from(self.name.clone())),
            )
    }
}

/// PICKUP: the row is the drag HANDLE. `on_drag` lives on
/// `StatefulInteractiveElement` (it rides the row's pending mouse-down), so this
/// one needs the `.id()`.
pub(crate) fn rail_draggable(
    row: Stateful<Div>,
    slug: &str,
    name: &str,
    active: bool,
) -> Stateful<Div> {
    row.on_drag(
        ProjectDrag {
            slug: slug.to_string(),
            name: name.to_string(),
            active,
        },
        |drag, _, _, cx| {
            let (slug, name) = (drag.slug.clone(), drag.name.clone());
            cx.new(|_| RailGhost { slug, name })
        },
    )
}

/// LANDING ZONE: make `el` a drop target for `slug`'s slot. Deliberately generic
/// and NOT stateful — `drag_over`/`on_drop` are on `InteractiveElement`, and a
/// non-empty drop-listener list already earns the element a hitbox — so the
/// WHOLE active card (header + the session rows under it) can be the target
/// while only the header is the handle. Before the split, an active card's drop
/// zone was just its ~33px header over ~110px of card body that ate the drag
/// silently (gpui clears `active_drag` on mouse-up: no snap-back, no feedback).
pub(crate) fn rail_drop_target<E: InteractiveElement>(
    el: E,
    slug: &str,
    active: bool,
    cx: &mut Context<Orchestrator>,
) -> E {
    let dst = slug.to_string();
    el.drag_over::<ProjectDrag>(move |s, drag: &ProjectDrag, _, _| {
        // no wash across the ACTIVE/PROJECTS boundary — that drop is refused.
        if drag.active == active {
            s.bg(rgba(DROP_TINT))
        } else {
            s
        }
    })
    .on_drop(cx.listener(move |this, drag: &ProjectDrag, _, cx| {
        this.reorder_project(&drag.slug, &dst, cx)
    }))
}

/// A PROJECTS row is both handle and target (its whole row is the card).
pub(crate) fn rail_reorderable(
    row: Stateful<Div>,
    slug: &str,
    name: &str,
    active: bool,
    cx: &mut Context<Orchestrator>,
) -> Stateful<Div> {
    rail_drop_target(rail_draggable(row, slug, name, active), slug, active, cx)
}

/// Move `src` onto `dst`'s slot (a remove-then-insert move). `None` when nothing
/// moved (dropped on itself, or an unknown slug).
pub(crate) fn reorder(slugs: &[String], src: &str, dst: &str) -> Option<Vec<String>> {
    let from = slugs.iter().position(|s| s == src)?;
    let to = slugs.iter().position(|s| s == dst)?;
    if from == to {
        return None;
    }
    let mut out = slugs.to_vec();
    let moved = out.remove(from);
    // insert at dst's PRE-removal index: dragging down lands after the target,
    // dragging up lands before it — either way src ends up exactly where dst was.
    out.insert(to, moved);
    Some(out)
}

/// The move the user actually performed: `view` is the RENDERED sequence
/// (ACTIVE rows first, then PROJECTS; `n_active` marks the split), NOT the global
/// `projects` vec. Doing the index arithmetic on the global vec instead moves the
/// row the OPPOSITE way whenever a live project sits behind a dormant one in the
/// vec — e.g. vec `[alpha, beta, gamma(live)]` renders `[gamma, alpha, beta]`, and
/// dragging alpha onto gamma (to the head) sent alpha to the TAIL, dragged beta up
/// with it, and persisted the inversion.
///
/// Refuses a drop that CROSSES the section boundary: the two sections are a VIEW
/// of one global order, and a cross-section drop cannot be honored faithfully —
/// the partition would immediately undo it (drag a dormant row above the live
/// ones and it falls straight back below them). Better a visibly-dead target than
/// a move that silently lands somewhere else.
pub(crate) fn reorder_view(
    view: &[String],
    n_active: usize,
    src: &str,
    dst: &str,
) -> Option<Vec<String>> {
    let from = view.iter().position(|s| s == src)?;
    let to = view.iter().position(|s| s == dst)?;
    if (from < n_active) != (to < n_active) {
        return None;
    }
    reorder(view, src, dst)
}

/// The rail's rendered sequence, as indices into `projects`: ACTIVE (the projects
/// this app hosts a live session in) then PROJECTS (everything else). `live` and
/// `needs` are the per-project flags, in `projects` order.
///
/// ONE definition of the rendered order, shared by `render_sidebar` (which paints
/// it) and `reorder_project` (which must move rows within the sequence the
/// user SEES).
pub(crate) fn rail_sections(live: &[bool], needs: &[bool]) -> (Vec<usize>, Vec<usize>) {
    let mut active: Vec<usize> = Vec::new();
    let mut rest: Vec<usize> = Vec::new();
    for i in 0..live.len() {
        if live[i] {
            active.push(i);
        } else {
            rest.push(i);
        }
    }
    // STABLE active order: needs-you projects float to the top, otherwise the
    // original (global) order. NOT by live busy/recency — those flap every few
    // seconds as sessions produce output, which is the "why is it reordering?"
    // reshuffle. The pin can transiently override the manual order while a
    // decision is pending; that is intended and visible.
    active.sort_by(|&a, &b| needs[b].cmp(&needs[a]).then(a.cmp(&b)));
    (active, rest)
}

/// Re-sort a fresh scan into the user's rail order. A slug the order doesn't
/// know (a brand-new project, an idea just created) ranks last and so keeps its
/// registry position at the TAIL — it appears at the end instead of jumping into
/// the middle. `sort_by_key` is stable, which is what makes that tail hold.
pub(crate) fn apply_order(projects: &mut [Project], order: &[String]) {
    let rank: std::collections::HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();
    projects.sort_by_key(|p| rank.get(p.slug.as_str()).copied().unwrap_or(usize::MAX));
}

/// The selection is a raw INDEX into `projects`, which `apply_order` PERMUTES —
/// so it has to be carried across a reorder by SLUG. `fallback` when the slug
/// isn't there (nothing was selected, or the project vanished).
pub(crate) fn index_of(projects: &[Project], slug: Option<&str>, fallback: usize) -> usize {
    slug.and_then(|s| projects.iter().position(|p| p.slug == s))
        .unwrap_or(fallback)
}

impl Orchestrator {
    /// The rail's rendered sequence (see `rail_sections`): (ACTIVE, PROJECTS) as
    /// indices into `self.projects`.
    pub(crate) fn rail_view(&self) -> (Vec<usize>, Vec<usize>) {
        // both flags off the per-frame infos cache — no host lock.
        let live: Vec<bool> = self
            .projects
            .iter()
            .map(|p| self.cached_infos(&p.slug).iter().any(|s| s.alive))
            .collect();
        let needs: Vec<bool> = self
            .projects
            .iter()
            .map(|p| {
                self.cached_infos(&p.slug)
                    .iter()
                    .any(|s| s.alive && s.phase == orchestrator_host::Phase::AwaitingDecision)
            })
            .collect();
        rail_sections(&live, &needs)
    }

    /// Land a rail drag: move `src` onto `dst`'s slot in the sequence the user
    /// SEES, re-anchor the selection by slug, and persist the resulting global
    /// order. The single choke point both drop sites route through.
    pub(crate) fn reorder_project(&mut self, src: &str, dst: &str, cx: &mut Context<Self>) {
        // Pre-scan the rail renders `seed_projects()`, whose slugs are BARE names
        // ("beta", "alpha") — not the registry keys ("path:/Users/…", "idea:…")
        // the real projects carry. Persisting THAT order would overwrite the
        // user's real `project_order` with slugs nothing can ever match again:
        // when the scan lands, `apply_order` finds no rank for any real slug,
        // every project ranks `usize::MAX`, and the saved order is gone from
        // SQLite with no undo. The seed rows even carry the real project NAMES,
        // so the destructive drag looks perfectly natural.
        if !self.scanned {
            return;
        }
        let (active, rest) = self.rail_view();
        let view: Vec<String> = active
            .iter()
            .chain(rest.iter())
            .map(|&i| self.projects[i].slug.clone())
            .collect();
        let Some(order) = reorder_view(&view, active.len(), src, dst) else {
            return;
        };
        let sel = self.projects.get(self.selected).map(|p| p.slug.clone());
        apply_order(&mut self.projects, &order);
        self.selected = index_of(&self.projects, sel.as_deref(), self.selected);
        // slugs are free-form (`path:/Users/…`, `idea:…`), so JSON — never a join.
        if let Ok(store) = self.store.lock() {
            if let Ok(json) = serde_json::to_string(&order) {
                let _ = store.set_setting("project_order", &json);
            }
        }
        self.project_order = order;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    // Selective imports: gpui is glob-imported above, and its `test` attr macro
    // would shadow the real one through a `use super::*`.
    use super::{apply_order, index_of, rail_sections, reorder, reorder_view};
    use orchestrator_core::Project;

    fn slugs(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn projects(names: &[&str]) -> Vec<Project> {
        names.iter().map(|s| Project::idea(s, s)).collect()
    }

    fn order_of(projects: &[Project]) -> Vec<String> {
        projects.iter().map(|p| p.slug.clone()).collect()
    }

    #[test]
    fn dragging_down_lands_on_the_targets_slot() {
        let out = reorder(&slugs(&["a", "b", "c", "d"]), "a", "c").unwrap();
        assert_eq!(out, slugs(&["b", "c", "a", "d"]));
    }

    #[test]
    fn dragging_up_lands_on_the_targets_slot() {
        let out = reorder(&slugs(&["a", "b", "c", "d"]), "d", "b").unwrap();
        assert_eq!(out, slugs(&["a", "d", "b", "c"]));
    }

    #[test]
    fn moves_to_head_and_to_tail() {
        let list = slugs(&["a", "b", "c", "d"]);
        assert_eq!(
            reorder(&list, "c", "a").unwrap(),
            slugs(&["c", "a", "b", "d"])
        );
        assert_eq!(
            reorder(&list, "a", "d").unwrap(),
            slugs(&["b", "c", "d", "a"])
        );
    }

    #[test]
    fn drop_on_self_or_unknown_slug_moves_nothing() {
        let list = slugs(&["a", "b", "c"]);
        assert!(reorder(&list, "b", "b").is_none());
        assert!(reorder(&list, "zz", "b").is_none());
        assert!(reorder(&list, "b", "zz").is_none());
    }

    #[test]
    fn idea_rows_reorder_like_any_other_row() {
        let list = slugs(&["path:/x", "idea:ship-it", "path:/y"]);
        let out = reorder(&list, "idea:ship-it", "path:/x").unwrap();
        assert_eq!(out, slugs(&["idea:ship-it", "path:/x", "path:/y"]));
    }

    // ── the rendered sequence IS the sequence a drag moves rows within ──

    #[test]
    fn the_rail_view_is_active_then_rest_with_needs_you_pinned() {
        // projects: a(dormant) b(live) c(live,needs) d(dormant)
        let (active, rest) = rail_sections(
            &[false, true, true, false],
            &[false, false, true, false],
        );
        assert_eq!(active, vec![2, 1]); // needs-you first, else global order
        assert_eq!(rest, vec![0, 3]); // rest keeps the global order verbatim
    }

    #[test]
    fn a_drag_moves_rows_within_the_rendered_order_not_the_global_vec() {
        // THE BUG (#28): vec [alpha, beta, gamma(live)] RENDERS as
        //   ACTIVE: gamma / PROJECTS: alpha, beta
        // Doing the move against the global vec sent alpha (dragged onto gamma,
        // i.e. to the HEAD) to the TAIL and dragged beta — never touched — up:
        assert_eq!(
            reorder(&slugs(&["alpha", "beta", "gamma"]), "alpha", "gamma").unwrap(),
            slugs(&["beta", "gamma", "alpha"]),
        );
        // Against the RENDERED sequence, alpha lands where gamma was — the head.
        let view = slugs(&["gamma", "alpha", "beta"]);
        let out = reorder_view(&view, 1, "alpha", "gamma");
        // …but this particular drop CROSSES the ACTIVE/PROJECTS boundary, so the
        // rail refuses it outright rather than move alpha somewhere it can't stay.
        assert!(out.is_none(), "a cross-section drop must not move anything");
        // With both rows in one section the move lands exactly on the target slot,
        // and alpha is NOT sent to the tail.
        let out = reorder_view(&view, 0, "alpha", "gamma").unwrap();
        assert_eq!(out, slugs(&["alpha", "gamma", "beta"]));
        assert_ne!(out.last().unwrap(), "alpha");
    }

    #[test]
    fn a_cross_section_drop_is_refused_in_both_directions() {
        // view: ACTIVE [gamma] | PROJECTS [alpha, beta]
        let view = slugs(&["gamma", "alpha", "beta"]);
        assert!(reorder_view(&view, 1, "alpha", "gamma").is_none()); // rest → active
        assert!(reorder_view(&view, 1, "gamma", "beta").is_none()); // active → rest
        // within PROJECTS it still works.
        assert_eq!(
            reorder_view(&view, 1, "beta", "alpha").unwrap(),
            slugs(&["gamma", "beta", "alpha"])
        );
    }

    #[test]
    fn a_within_active_drag_reorders_the_active_section() {
        // view: ACTIVE [g1, g2, g3] | PROJECTS [x]
        let view = slugs(&["g1", "g2", "g3", "x"]);
        assert_eq!(
            reorder_view(&view, 3, "g3", "g1").unwrap(),
            slugs(&["g3", "g1", "g2", "x"])
        );
    }

    #[test]
    fn apply_order_replays_a_full_permutation() {
        let mut ps = projects(&["a", "b", "c"]);
        apply_order(&mut ps, &slugs(&["c", "a", "b"]));
        assert_eq!(order_of(&ps), slugs(&["c", "a", "b"]));
    }

    #[test]
    fn unknown_slugs_keep_registry_order_at_the_tail() {
        // "new1"/"new2" aren't in the persisted order → they hold their scan
        // order after the known ones, instead of jumping into the middle.
        let mut ps = projects(&["a", "new1", "b", "new2"]);
        apply_order(&mut ps, &slugs(&["b", "a"]));
        assert_eq!(order_of(&ps), slugs(&["b", "a", "new1", "new2"]));
    }

    #[test]
    fn an_empty_order_leaves_the_registry_order_alone() {
        let mut ps = projects(&["a", "b", "c"]);
        apply_order(&mut ps, &[]);
        assert_eq!(order_of(&ps), slugs(&["a", "b", "c"]));
    }

    #[test]
    fn selection_follows_its_slug_not_its_index() {
        // "c" is selected (index 2); the drag permutes the vec around it.
        let mut ps = projects(&["a", "b", "c", "d"]);
        apply_order(&mut ps, &slugs(&["b", "c", "d", "a"]));
        assert_eq!(index_of(&ps, Some("c"), 2), 1);
        // a vanished / absent selection keeps the caller's fallback.
        assert_eq!(index_of(&ps, Some("gone"), 3), 3);
        assert_eq!(index_of(&ps, None, 0), 0);
    }

    #[test]
    fn a_seed_order_can_never_be_persisted_over_the_real_one() {
        // The pre-scan rail renders seed_projects(): BARE slugs. If such an order
        // were ever persisted, apply_order would rank every REAL slug usize::MAX
        // and the user's order would be gone. `reorder_project` refuses to run
        // before `scanned`; this pins WHY (the two slug sets are disjoint).
        let mut real = projects(&["path:/Users/t/local/beta", "idea:alpha"]);
        let before = order_of(&real);
        apply_order(&mut real, &slugs(&["beta", "alpha"]));
        assert_eq!(
            order_of(&real),
            before,
            "no real slug matches a seed slug — the saved order would be pure noise"
        );
    }
}
