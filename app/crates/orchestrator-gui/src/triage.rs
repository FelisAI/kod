use gpui::prelude::FluentBuilder;
use gpui::*;
use crate::*;


impl Orchestrator {

    /// Enter the triage sweep (docs/019 T7): a keyboard mode walking nodes j/k,
    /// single-key status stamps. Starts the cursor at the top of the walk.
    pub(crate) fn enter_triage(&mut self, cx: &mut Context<Self>) {
        let slug = self.project().slug.clone();
        let order = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let tree = orchestrator_store::build_tree(&store.load_tree(&slug).unwrap_or_default());
            cockpit::triage_walk(&tree)
        };
        self.triage_active = true;
        self.triage_done_armed = false;
        // resume where the user is, else the top of the sweep.
        self.triage_cursor = self
            .focused_part
            .filter(|c| order.contains(c))
            .or_else(|| order.first().copied());
        if let Some(c) = self.triage_cursor {
            self.focused_part = Some(c);
        }
        cx.notify();
    }

    pub(crate) fn exit_triage(&mut self, cx: &mut Context<Self>) {
        self.triage_active = false;
        self.triage_done_armed = false;
        cx.notify();
    }

    /// Move the triage cursor (docs/019 T7): +1 = j (next), -1 = k (prev),
    /// clamped at the ends. The map focus follows so the node is on the glass.
    fn triage_move(&mut self, delta: i64, cx: &mut Context<Self>) {
        let slug = self.project().slug.clone();
        let order = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let tree = orchestrator_store::build_tree(&store.load_tree(&slug).unwrap_or_default());
            cockpit::triage_walk(&tree)
        };
        if let Some(next) = cockpit::triage_step(&order, self.triage_cursor, delta) {
            self.triage_cursor = Some(next);
            self.focused_part = Some(next);
        }
        self.triage_done_armed = false; // moving off a node cancels a pending DONE
        cx.notify();
    }

    /// Stamp the triage cursor's status as a DIRECT human edit (docs/019 T7):
    /// instant, journaled origin `human:triage`, undoable. status_source=
    /// human:triage weights it below a deliberate hand-set in later audits.
    /// After stamping, `advance` steps j so a sweep flows without a second key.
    fn triage_stamp(&mut self, lifecycle: Lifecycle, advance: bool, cx: &mut Context<Self>) {
        let Some(id) = self.triage_cursor else { return };
        let slug = self.project().slug.clone();
        // the cursor must still belong to THIS project before we write (review:
        // a stale cursor from a prior project would mark the WRONG node — part.id
        // is global — and misfile its undo; a deleted cursor would journal an
        // empty-inverse 'dead ⌘Z'). Check + write in one scoped lock, then act.
        let wrote = {
            let mut store = match self.store.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            let alive = store
                .load_tree(&slug)
                .map(|ps| ps.iter().any(|p| p.id == id))
                .unwrap_or(false);
            if alive {
                let _ = store.accept_diff_from(
                    &slug,
                    &[DiffOp::SetStatus {
                        id,
                        lifecycle,
                        source: orchestrator_store::StatusSource::Triage,
                    }],
                    "human:triage",
                    None,
                );
            }
            alive
        };
        if !wrote {
            // the cursor node vanished — re-anchor to the walk, don't stamp a ghost.
            self.triage_cursor = None;
            self.triage_move(0, cx);
            return;
        }
        if advance {
            self.triage_move(1, cx);
        } else {
            cx.notify();
        }
    }

    /// The triage sweep's keyboard grammar — j/k walk, t/i single-key stamp,
    /// x = confirm DONE (deliberately a distinct key: done can lie expensively),
    /// Esc/q leaves. Returns true when it consumed the key.
    pub(crate) fn triage_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if !self.triage_active {
            return false;
        }
        match ev.keystroke.key.as_str() {
            "j" | "down" => self.triage_move(1, cx),
            "k" | "up" => self.triage_move(-1, cx),
            "t" => {
                self.triage_done_armed = false;
                self.triage_stamp(Lifecycle::Todo, true, cx);
            }
            "i" => {
                self.triage_done_armed = false;
                self.triage_stamp(Lifecycle::Idea, true, cx);
            }
            // DONE costs one EXTRA deliberate keystroke (docs/019 T7): the first
            // `x` arms, a second confirms — done is the assertion that lies
            // expensively, so it never lands on a single reflexive tap.
            "x" => {
                if self.triage_done_armed {
                    self.triage_done_armed = false;
                    self.triage_stamp(Lifecycle::Done, true, cx);
                } else {
                    self.triage_done_armed = true;
                    cx.notify();
                }
            }
            "escape" | "q" => self.exit_triage(cx),
            _ => return false,
        }
        true
    }

    /// The triage-sweep banner (docs/019 T7): the keyboard grammar, on the glass
    /// while the sweep is active. Every stamp is a DIRECT human edit
    /// (human:triage) — instant, journaled, ⌘Z-undoable.
    pub(crate) fn render_triage_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        // when a DONE is armed, the banner says so — the second `x` confirms.
        let (hint, ink) = if self.triage_done_armed {
            (
                "press x again to confirm DONE · j/k cancels".to_string(),
                AMBER,
            )
        } else {
            (
                "j/k move · t todo · i idea · x done (×2) · q done sweeping".to_string(),
                MUTED,
            )
        };
        // the cursor node's name rides the banner so the user always sees
        // what the next key stamps — even if it drilled out of the canvas view.
        let cursor_name = self.triage_cursor.and_then(|id| {
            let slug = self.project().slug.clone();
            self.store
                .lock()
                .ok()
                .and_then(|s| s.load_tree(&slug).ok())
                .and_then(|ps| ps.iter().find(|p| p.id == id).map(|p| p.name.clone()))
        });
        div()
            .id("triage-banner")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .mx(px(14.))
            .mt(px(8.))
            .px(px(12.))
            .py(px(7.))
            .rounded(px(10.))
            .bg(rgb(CARD2))
            .border_1()
            .border_color(rgb(if self.triage_done_armed {
                AMBER
            } else {
                ACCENT
            }))
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.5))
                    .text_color(rgb(ACCENT))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("⌦ Triage sweep"),
            )
            .when_some(cursor_name, |c, n| {
                c.child(
                    div()
                        .flex_none()
                        .max_w(px(220.))
                        .text_size(px(12.))
                        .text_color(rgb(TEXT_STRONG))
                        .child(SharedString::from(format!("▸ {}", termview::trim(&n, 30)))),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(11.))
                    .font_family("Menlo")
                    .text_color(rgb(ink))
                    .child(SharedString::from(hint)),
            )
            .child(
                div()
                    .id("triage-exit")
                    .flex_none()
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED2))
                    .hover(|h| h.text_color(rgb(TEXT)).bg(rgb(CARD)))
                    .child("done")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.exit_triage(cx))),
            )
            .into_any_element()
    }

}
