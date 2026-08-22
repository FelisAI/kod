use gpui::prelude::FluentBuilder;
use gpui::*;
use crate::*;


/// Which rail section is showing (#54). FIXED set — the founder chose these five,
/// and each one is a *place*, not a filter: the rail names the group, so the
/// bodies no longer repeat it as a `settings_group_header`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    General,
    Profiles,
    Sessions,
    Automation,
    BackgroundAi,
}

impl SettingsSection {
    pub(crate) const ALL: [Self; 5] = [
        Self::General,
        Self::Profiles,
        Self::Sessions,
        Self::Automation,
        Self::BackgroundAi,
    ];

    /// Stable element-id fragment — also the per-section scroll key, so each
    /// section keeps its OWN scroll offset (see the scroller in `render`).
    fn key(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Profiles => "profiles",
            Self::Sessions => "sessions",
            Self::Automation => "automation",
            Self::BackgroundAi => "background-ai",
        }
    }

    /// The rail row's text AND the content header's title — one word for one
    /// place, said once.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Profiles => "Profiles",
            Self::Sessions => "Sessions",
            Self::Automation => "Automation",
            Self::BackgroundAi => "Background AI",
        }
    }

    /// The one muted line under the header title: what this place is FOR, in the
    /// user's words. "Background AI" earns the longest one — #57 was filed
    /// precisely because "In-app LLM" never said what it governed.
    fn blurb(self) -> &'static str {
        match self {
            Self::General => "Where Kod puts new projects on disk.",
            Self::Profiles => {
                "Named accounts — a separate claude/codex login per profile — and which one new sessions use."
            }
            Self::Sessions => "What every session Kod starts inherits, and how it asks for you.",
            Self::Automation => "What Kod is allowed to do while you're away from it.",
            Self::BackgroundAi => {
                "The model Kod calls on its OWN behalf — summaries, Recover previews, map proposals. Never your sessions."
            }
        }
    }
}

/// The Settings window's root view (#54). Deliberately THIN. Everything Settings
/// writes is live Orchestrator state — `claude_effort` rides every SpawnSpec,
/// `toast_secs` drives tick_needs, `summaries_on` gates the summarizer,
/// `projects_root` decides where a new project's directory lands — so this view
/// keeps NO copy of any of it. A second copy would be a second source of truth
/// that silently drifts (and would persist to sqlite while the running app kept
/// the old value until restart).
pub(crate) struct SettingsWindow {
    /// the app's one Orchestrator. WEAK on purpose: this window must never be the
    /// reason the whole app state (host, store, every session cache) stays alive.
    orch: WeakEntity<Orchestrator>,
    /// this window's OWN keyboard sink. `root_focus`/`term_focus` are nodes in the
    /// MAIN window's element tree; focusing one here would leave this window's
    /// focus pointing at a node its dispatch tree does not contain.
    focus: FocusHandle,
    /// which rail section is showing. WINDOW-local: it dies with the window, which
    /// is the honest lifetime for "where I was". (The old `settings_tab` lived on
    /// the Orchestrator only because Settings was a Screen that kept coming back.)
    section: SettingsSection,
}

impl SettingsWindow {
    pub(crate) fn new(
        orch: WeakEntity<Orchestrator>,
        focus: FocusHandle,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            orch,
            focus,
            // ORCH_DEMO=settings:<section> lands directly on one pane. Each pane is
            // reachable only by clicking its rail row, so without this every check
            // of a non-General pane — screenshot, layout eyeball, truncation hunt —
            // needs a human with a mouse. Dev affordance only; unset = General.
            section: std::env::var("ORCH_DEMO")
                .ok()
                .and_then(|v| v.strip_prefix("settings:").map(str::to_string))
                .and_then(|name| {
                    SettingsSection::ALL
                        .into_iter()
                        .find(|s| s.label().eq_ignore_ascii_case(&name))
                })
                .unwrap_or(SettingsSection::General),
        }
    }

    /// Every PROGRAMMATIC close path (Esc, ⌘,, ⌘W).
    fn close(&self, window: &mut Window, cx: &mut App) {
        close_settings(&self.orch, window, cx);
    }
}

/// The one programmatic way out of the Settings window (Esc, ⌘,, ⌘W).
///
/// `remove_window()` does NOT fire `on_window_should_close`, so both of the
/// things that hook does must happen here explicitly: the geometry is sampled
/// while the window still exists (#62 — the 500ms poll needs a sample to REPEAT
/// before it persists one, so a resize followed straight by ⌘W would be lost),
/// and the inline editors this window owns are dropped, or a half-typed model id
/// or an unnamed profile draft survives on the Orchestrator with no field to
/// show it in. A free fn rather than a method because the Esc path routes keys
/// through the Orchestrator's own weak handle and has no `&SettingsWindow`.
fn close_settings(orch: &WeakEntity<Orchestrator>, window: &mut Window, cx: &mut App) {
    let wb = crate::winchrome::WinSample::of(window);
    let _ = orch.update(cx, |o, cx| {
        o.persist_settings_window_bounds(Some(wb));
        o.close_settings_state(cx);
    });
    window.remove_window();
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // the main window is gone (its ⌘W / red button) — there is nothing left to
        // configure, and every control below would be dead chrome.
        let Some(orch) = self.orch.upgrade() else {
            window.remove_window();
            return div().into_any_element();
        };
        if !self.focus.is_focused(window) {
            self.focus.focus(window);
        }
        let section = self.section;
        // `Entity::update` LEASES the Orchestrator, and gpui records that lease as
        // a window-invalidator edge — so one `cx.notify()` inside an Orchestrator
        // setter repaints BOTH windows, for free, with no manual plumbing. It is
        // also why the body is built here rather than copied into this view.
        // NOTE: deliberately NOT `Orchestrator::render` — that rebuilds infos_cache
        // and calls reflow_terminal(window), which sizes the live PTY from
        // `window.viewport_size()`; running it here would resize the user's
        // terminal to the Settings window.
        let body = orch.update(cx, |o, cx| o.render_settings_section(section, cx));
        // The two Settings branches lifted verbatim out of the main window's root
        // key router (minus their `screen == Screen::Settings` guard, which this
        // window now IS), plus Esc-to-close. This is `Context::listener`'s body
        // written out against the ORCHESTRATOR's weak handle — the state these
        // keys edit lives there, and a listener holds no window of its own, so it
        // fires correctly from this window's dispatch tree.
        let orch_keys = self.orch.clone();
        let keys = move |ev: &KeyDownEvent, window: &mut Window, cx: &mut App| {
            let mut closing = false;
            let _ = orch_keys.update(cx, |this: &mut Orchestrator, cx| {
                if this
                    .profile_draft
                    .as_ref()
                    .is_some_and(|d| d.editing.is_some())
                {
                    this.route_inline_key(ev, InlineTarget::ProfileField, cx);
                } else if this.setting_edit.is_some() {
                    this.route_inline_key(ev, InlineTarget::SettingText, cx);
                } else if ev.keystroke.key.as_str() == "escape" {
                    // route_inline_key gets first refusal on Esc above (it closes
                    // an open editor), so only a SECOND Esc — with no editor left
                    // — closes the window.
                    closing = true;
                }
            });
            // OUTSIDE the update: close_settings takes its own lease on the
            // Orchestrator, and a nested `update` on a leased entity panics.
            if closing {
                close_settings(&orch_keys, window, cx);
            }
        };

        let mut rail = div()
            .flex_none()
            .w(px(198.))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(2.))
            .pt(px(18.))
            .px(px(10.))
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(HAIR))
            .child(
                div()
                    .px(px(8.))
                    .pb(px(10.))
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(MUTED2))
                    .child("SETTINGS"),
            );
        for s in SettingsSection::ALL {
            rail = rail.child(settings_rail_row(
                s,
                s == section,
                cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                    this.section = s;
                    // a section change hides whatever field an inline editor was
                    // attached to, so drop it — otherwise it keeps capturing the
                    // keystream for a control nobody can see.
                    let _ = this.orch.update(cx, |o, cx| {
                        o.setting_edit = None;
                        if let Some(d) = o.profile_draft.as_mut() {
                            d.editing = None;
                        }
                        cx.notify();
                    });
                    cx.notify();
                }),
            ));
        }

        let header = div()
            .flex_none()
            .px(px(26.))
            .pt(px(20.))
            .pb(px(12.))
            .border_b_1()
            .border_color(rgb(HAIR_SOFT))
            .flex()
            .flex_col()
            .gap(px(3.))
            .child(
                div()
                    .text_size(px(19.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT_STRONG))
                    .child(section.label()),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child(section.blurb()),
            );

        div()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_row()
            .bg(rgb(APP_BG))
            .text_color(rgb(TEXT))
            // ⌘, inside Settings TOGGLES it shut — the same verb it always was.
            // It also keeps the GLOBAL ToggleSettings listener from re-entering:
            // a KEY-driven action dispatches SYNCHRONOUSLY while this window is
            // taken out of `cx.windows`, so the global's liveness probe would fail
            // and open a SECOND Settings window. A div's on_action runs in the
            // Bubble phase with propagation already stopped, so the global never
            // runs here.
            .on_action(cx.listener(|this: &mut Self, _: &ToggleSettings, window, cx| {
                this.close(window, cx)
            }))
            // ⌘W: the macOS verb for "close this window". Bound app-wide but
            // handled ONLY here, so it is inert in the main window (which has no
            // close-to-quit semantics) exactly as it is today.
            .on_action(cx.listener(|this: &mut Self, _: &CloseSettings, window, cx| {
                this.close(window, cx)
            }))
            // MUST sit on the focused node (or an ancestor): gpui's dispatch path
            // runs focused-node → root, so a handler on a CHILD of the focused div
            // would never be on the path and would never fire.
            .on_key_down(keys)
            .child(rail)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(header)
                    .child(
                        // per-section scroll id, not one shared "settings-scroll":
                        // gpui keys the scroll offset by element id and only CLAMPS
                        // it on shrink, so a single id would carry a deep offset out
                        // of Profiles into a two-card section and land you mid-page.
                        div()
                            .id(SharedString::from(format!(
                                "settings-scroll-{}",
                                section.key()
                            )))
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px(px(26.))
                            .pt(px(16.))
                            .pb(px(28.))
                            .flex()
                            .flex_col()
                            .items_center()
                            .child(body),
                    ),
            )
            .into_any_element()
    }
}


impl Orchestrator {

    /// Drop both inline editors when the Settings window goes away, so a
    /// half-typed model id or profile label can never commit later against a
    /// field nobody can see — and forget the window so ⌘, opens a fresh one.
    pub(crate) fn close_settings_state(&mut self, cx: &mut Context<Self>) {
        self.setting_edit = None;
        self.profile_draft = None;
        self.settings_window = None;
        self.settings_focus = None;
        cx.notify();
    }

    /// Put the keystream back on the Settings window's own sink after a control
    /// opens an inline editor. The four call sites used to focus `root_focus` —
    /// a node in the MAIN window's element tree. With Settings in its own window
    /// that `window` is the SETTINGS window, and gpui would silently fall back to
    /// its root node: half-working, and a maddening bug later.
    fn focus_settings_input(&self, window: &mut Window) {
        if let Some(f) = self.settings_focus.as_ref() {
            f.focus(window);
        }
    }

    /// Persist + apply the default claude effort (Settings picker).
    fn set_claude_effort(&mut self, val: &str, cx: &mut Context<Self>) {
        self.claude_effort = val.to_string();
        if let Ok(store) = self.store.lock() {
            let _ = store.set_setting("claude_effort", val);
        }
        cx.notify();
    }

    /// Persist + apply the needs-you toast lifetime in seconds; `0` = permanent
    /// (never auto-expires — only ✕ or "Open in terminal ▸" dismiss it). Read by tick_needs (#2).
    fn set_toast_secs(&mut self, secs: u64, cx: &mut Context<Self>) {
        self.toast_secs = secs;
        if let Ok(store) = self.store.lock() {
            let _ = store.set_setting("toast_secs", &secs.to_string());
        }
        cx.notify();
    }

    /// Persist + apply the provider for isolated app prompts, reconciling the two
    /// stored model ids against it (#57 review). A model id is passed STRAIGHT
    /// THROUGH to the CLI (`claude --model <v>` / `codex -m <v>`), and the stored
    /// value wins over the provider's default — so a claude id left armed under
    /// the codex provider makes `codex exec -m claude-haiku-4-5` the shape of
    /// EVERY summary and Recover preview, all of which fail, with nothing in the
    /// UI saying why. Only an id we ship as a preset for the OTHER provider is
    /// cleared: that's the one-click path the preset rows opened up, and it can't
    /// misfire on a hand-typed Custom… id, which stays the user's own business.
    fn set_prompt_provider(&mut self, provider: extract::PromptProvider, cx: &mut Context<Self>) {
        let mut plumbing_model = None;
        let mut structural_model = None;
        if let Ok(store) = self.store.lock() {
            let _ = store.set_setting("prompt_provider", provider.key());
            for k in ["prompt_plumbing_model", "prompt_structural_model"] {
                if store
                    .get_setting(k)
                    .is_some_and(|v| belongs_to_other_provider(v.trim(), provider))
                {
                    let _ = store.set_setting(k, "");
                }
            }
            plumbing_model = store.get_setting("prompt_plumbing_model");
            structural_model = store.get_setting("prompt_structural_model");
        }
        let config = extract::PromptConfig::from_settings(
            Some(provider.key()),
            plumbing_model.as_deref(),
            structural_model.as_deref(),
        );
        self.prompt_provider = config.provider;
        extract::set_prompt_config(config);
        cx.notify();
    }

    /// Persist + apply the global auto-continue-on-limit-reset flag (docs/019).
    /// The daemon is storage-free, so the GUI is the source of truth: persist it
    /// AND push it to the host so a live daemon caches it immediately. Default OFF.
    fn set_auto_continue(&mut self, on: bool, cx: &mut Context<Self>) {
        self.auto_continue = on;
        if let Ok(store) = self.store.lock() {
            let _ = store.set_setting("auto_continue", if on { "1" } else { "0" });
        }
        self.host.set_auto_continue(on, self.ac_fire_on_reset);
        cx.notify();
    }

    /// Persist + apply the fire-on-reset config (audit B2 / task #31). Default OFF:
    /// with it off the master auto-continue arms but never FIRES (its cleared-banner
    /// edge is unreachable for an idle session); on, it fires when the resolved reset
    /// instant arrives + the session is quiet.
    fn set_ac_fire_on_reset(&mut self, on: bool, cx: &mut Context<Self>) {
        self.ac_fire_on_reset = on;
        if let Ok(store) = self.store.lock() {
            let _ = store.set_setting("ac_fire_on_reset", if on { "1" } else { "0" });
        }
        self.host.set_auto_continue(self.auto_continue, on);
        cx.notify();
    }

    /// Ask macOS for a folder (gpui's native NSOpenPanel) and adopt it as the
    /// projects root. A path is exactly the kind of string you PICK, not type —
    /// and the app has no real text field (the rail's inline buffer has no
    /// cursor, no selection, no ⌘V), so a picker is the honest control here.
    fn choose_projects_root(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(dir) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        // $HOME (or any ancestor of it: /, /Users) as the projects
                        // folder would put every future project directory — and
                        // therefore every session's cwd — straight in the home
                        // folder. That is the one thing #29 exists to prevent, so
                        // the picker refuses it out loud instead of accepting it.
                        let home = orchestrator_core::scan::home();
                        if home.starts_with(&dir) {
                            this.projects_root_err = Some(format!(
                                "{} would put every project in your home folder — pick a subfolder",
                                dir.display()
                            ));
                            cx.notify();
                            return;
                        }
                        this.projects_root_err = None;
                        this.set_projects_root(dir, cx);
                    });
                }
            }
        })
        .detach();
    }

    /// One rail section's body column (#54), rendered from the ORCHESTRATOR's own
    /// context so every `cx.listener(|this, …| this.set_…())` below stays a plain
    /// Orchestrator listener. There is no "Done" button: the window's own close
    /// button IS Done — nothing here is transactional, every control persists on
    /// click, and a Done button inside an OS window reads as an unsaved-changes
    /// dialog it isn't.
    pub(crate) fn render_settings_section(
        &self,
        section: SettingsSection,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match section {
            SettingsSection::General => self.render_settings_general(cx).into_any_element(),
            SettingsSection::Profiles => self.render_settings_profiles(cx).into_any_element(),
            SettingsSection::Sessions => self.render_settings_sessions(cx).into_any_element(),
            SettingsSection::Automation => self.render_settings_automation(cx).into_any_element(),
            SettingsSection::BackgroundAi => self.render_settings_bg_ai(cx).into_any_element(),
        }
    }

    /// General — the projects folder (#29), the one setting that is about the
    /// user's disk rather than about agents.
    fn render_settings_general(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The RESOLVED path, always — so the user can see exactly where a new
        // project's directory will land. Default (unset) is ~/local, the root the
        // scanner has always trusted; `exists` is shown because the folder is
        // created lazily (a boot must never mkdir in his home).
        let root = self.projects_root.clone();
        let root_shown = root.display().to_string();
        let is_default = root == orchestrator_core::default_projects_root();
        let missing = !root.is_dir();
        let root_note = match (is_default, missing) {
            (true, true) => "the default — created when you make your first project".to_string(),
            (true, false) => "the default".to_string(),
            (false, true) => "doesn't exist yet — created when you make your first project".to_string(),
            (false, false) => String::new(),
        };
        let projects_folder = div()
            .flex()
            .flex_col()
            .gap(px(5.))
            // NOT a setting_radio_row. Those truncate their label (correctly — a
            // long option name shouldn't reflow the whole pane), but this row's
            // entire job is showing the full resolved path. Truncating it hides
            // exactly the thing the user opened the row to read, and a path has no
            // useful prefix to keep. So it gets its own row, and it WRAPS.
            .child(
                div()
                    .id("projects-root")
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(11.))
                    .py(px(8.))
                    .rounded(px(9.))
                    .cursor_pointer()
                    .bg(rgb(CARD2))
                    .border_1()
                    .border_color(rgb(0x346B54))
                    .hover(|h| h.border_color(rgb(0x2C5246)))
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.choose_projects_root(cx)),
                    )
                    .child(
                        div()
                            .text_size(px(13.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT_STRONG))
                            .child(SharedString::from(root_shown)),
                    )
                    .when(!root_note.is_empty(), |c| {
                        c.child(
                            div()
                                .text_size(px(11.5))
                                .text_color(rgb(MUTED2))
                                .child(SharedString::from(root_note)),
                        )
                    }),
            )
            .child(
                div()
                    .id("projects-root-choose")
                    .px(px(11.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(0x2C5246))
                    .text_size(px(12.5))
                    .text_color(rgb(ACCENT))
                    .hover(|h| h.bg(rgb(0x16231D)))
                    .child("Choose folder…")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.choose_projects_root(cx)
                    })),
            )
            // a REFUSED pick ($HOME itself) says so — a silently ignored click is
            // how the user ends up believing a setting took.
            .when_some(self.projects_root_err.clone(), |d, err| {
                d.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(0xE68A8A))
                        .child(SharedString::from(err)),
                )
            });

        settings_body().child(settings_section(
            "Projects folder",
            // Names the CURRENT affordance (＋ New… → New project) and drops the
            // "idea with no code yet" sentence: Idea is behind the map feature now,
            // so in a default build that described something the user cannot reach.
            "Where a new project gets its own directory. “＋ New… → New project” creates <folder>/<name>/, and every session for that project runs there — never in your home folder.",
            projects_folder,
        ))
    }

    /// Sessions — what every session Kod starts inherits (default claude effort)
    /// and how loudly a session asks for you (needs-you toast lifetime).
    fn render_settings_sessions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // (SpawnSpec.effort value, label, note) — "" = off. Applied host-side in
        // the per-session --settings file; "ultracode" is the mode flag, the
        // rest are effortLevel values (settings.json rejects "max"/"auto").
        let cur = self.claude_effort.clone();
        let opts: [(&str, &str, &str); 4] = [
            (
                "ultracode",
                "Ultracode",
                "xhigh reasoning + auto workflow orchestration",
            ),
            ("xhigh", "Extra high", ""),
            ("high", "High", ""),
            // "" IS the default (boot.rs reads the setting with unwrap_or_default),
            // but nothing on screen said so: this row sits last in a strongest-first
            // list, and "claude's own default" reads as "this option uses claude's
            // default effort" rather than "this is the option Kod ships selected".
            // Both are true; only one was being heard.
            ("", "Off (default)", "Kod sets nothing — claude uses its own effort"),
        ];
        let mut effort = div().flex().flex_col().gap(px(5.));
        for (val, label, note) in opts {
            let selected = cur == val;
            let v = val.to_string();
            effort = effort.child(setting_radio_row(
                format!("eff-{}", if val.is_empty() { "off" } else { val }),
                selected,
                label,
                note,
                cx.listener(move |this, _: &ClickEvent, _, cx| this.set_claude_effort(&v, cx)),
            ));
        }

        // (secs, label, note); 0 = permanent, the default when nothing is stored.
        let cur_toast = self.toast_secs;
        let toast_opts: [(u64, &str, &str); 4] = [
            (6, "6 seconds", ""),
            (15, "15 seconds", ""),
            (30, "30 seconds", ""),
            (
                0,
                "Until dismissed",
                "stays up until you ✕ or open it",
            ),
        ];
        let mut toast_picker = div().flex().flex_col().gap(px(5.));
        for (secs, label, note) in toast_opts {
            let selected = cur_toast == secs;
            toast_picker = toast_picker.child(setting_radio_row(
                format!("toast-{secs}"),
                selected,
                label,
                note,
                cx.listener(move |this, _: &ClickEvent, _, cx| this.set_toast_secs(secs, cx)),
            ));
        }

        settings_body()
            .child(settings_section(
                "Default claude effort",
                "Preset on every claude session this app starts or resumes — no more manual /effort. Sub-agents keep their own effort settings.",
                effort,
            ))
            .child(settings_section(
                "Needs-you toast duration",
                "How long a needs-you toast stays on screen before it auto-clears. \"Until dismissed\" keeps it up until you ✕ or open it.",
                toast_picker,
            ))
    }

    /// Automation — the two unattended-resume switches (docs/019 / task #31).
    /// They are one setting in two halves: the master arms it, `fire_on_reset`
    /// is what actually makes it fire, so they belong on the same page.
    fn render_settings_automation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let auto_on = self.auto_continue;
        let auto_continue = setting_toggle_row(
            "auto-continue-toggle",
            auto_on,
            if auto_on { "On" } else { "Off" },
            "",
            cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.set_auto_continue(!this.auto_continue, cx)
            }),
        );
        let fire_on = self.ac_fire_on_reset;
        let fire_on_reset = setting_toggle_row(
            "ac-fire-on-reset-toggle",
            fire_on,
            if fire_on { "On" } else { "Off" },
            "",
            cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.set_ac_fire_on_reset(!this.ac_fire_on_reset, cx)
            }),
        );

        settings_body()
            .child(settings_section(
                "Auto-continue on limit reset",
                "Unattended: when a session is blocked on a usage limit, resume it automatically the moment its window resets — even with the app closed. Requires the background daemon (the default run mode); it has no effect in in-process mode. Off by default.",
                auto_continue,
            ))
            .child(settings_section(
                "Fire on the reset clock",
                "How auto-continue decides the block is over. Off (default): only resume once the limit banner actually disappears — the safest signal, but an idle session never repaints its grid, so auto-continue effectively won't fire on its own. On: resume when the estimated reset time arrives and the session is quiet. Turn this on to make auto-continue actually resume unattended.",
                fire_on_reset,
            ))
    }

    /// Background AI (#57) — renamed from "In-app LLM", which named a thing the
    /// user has no other word for and never said what it DID. Everything here
    /// governs prompts Kod issues on its own behalf: session summaries, memory
    /// extraction, Recover previews, map proposals, breakdowns, re-ground. It
    /// never touches the model a session of yours runs.
    fn render_settings_bg_ai(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut provider_picker = div().flex().flex_col().gap(px(5.));
        for provider in extract::PromptProvider::ALL {
            let selected = self.prompt_provider == provider;
            provider_picker = provider_picker.child(setting_radio_row(
                format!("prompt-provider-{}", provider.key()),
                selected,
                provider.label(),
                provider.note(),
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.set_prompt_provider(provider, cx)
                }),
            ));
        }

        // (setter stays inline — it writes the same store key + mirror + notify.)
        let sum_on = self.summaries_on;
        let summaries = setting_toggle_row(
            "sum-toggle",
            sum_on,
            if sum_on { "On" } else { "Off" },
            "",
            cx.listener(|this, _: &ClickEvent, _, cx| {
                this.summaries_on = !this.summaries_on;
                if let Ok(store) = this.store.lock() {
                    let _ = store
                        .set_setting("summaries_on", if this.summaries_on { "1" } else { "0" });
                }
                cx.notify();
            }),
        );

        settings_body()
            .child(settings_section(
                "Which account runs them",
                "Background work goes through this CLI's existing login — no API key, no extra bill. Env overrides: ORCH_PROMPT_PROVIDER, ORCH_PROMPT_PLUMBING_MODEL, ORCH_PROMPT_STRUCTURAL_MODEL.",
                provider_picker,
            ))
            .child(settings_section(
                "Session summaries",
                "When a session goes idle, one background call (≤20/hr, backs off on rate limits) writes the one-line summary + next step you read on the Standup.",
                summaries,
            ))
            .child(settings_group_header("PROMPT MODELS"))
            .child(settings_section(
                "Plumbing model",
                "The cheap, fast, high-volume calls: session summaries and Recover previews. Pick the smallest model that still reads a transcript correctly — this one runs the most.",
                self.render_model_setting("prompt_plumbing_model", "plumbing model", cx),
            ))
            .child(settings_section(
                "Structural model",
                "The heavier reasoning calls: map proposals, breakdowns, memory extraction. These run rarely and their output is edited into your map, so accuracy beats cost here.",
                self.render_model_setting("prompt_structural_model", "structural model", cx),
            ))
    }

    /// One background-prompt model as a PRESET picker plus a "Custom…" escape
    /// hatch (#57). It used to be a bare free-text row: you had to already know a
    /// valid model id, and nothing in the app ever listed one. The presets are the
    /// ids the SELECTED provider's CLI accepts (they are passed straight through
    /// as `claude --model <v>` / `codex -m <v>`), so switching provider re-lists
    /// them; "" still means "the account's own default".
    fn render_model_setting(
        &self,
        key: &'static str,
        slot: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cur = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.get_setting(key))
            .unwrap_or_default()
            .trim()
            .to_string();
        let presets = model_presets(self.prompt_provider);
        // an id we didn't ship a preset for is CUSTOM — hand-typed, or from a
        // preset list that has since moved on. Showing it as Custom keeps the
        // stored value VISIBLE; folding it into "account default" would hide a
        // value that is still what the background calls actually use. (A preset
        // id belonging to the OTHER provider can't reach this state — the
        // provider switch itself clears it, see set_prompt_provider.)
        let is_custom = !cur.is_empty() && !presets.iter().any(|(v, _)| *v == cur);
        let editing = self.setting_edit.as_ref().is_some_and(|e| e.key == key);

        let mut col = div().flex().flex_col().gap(px(5.));
        for (val, note) in presets {
            let selected = !is_custom && !editing && cur == *val;
            let label = if val.is_empty() {
                "(account default)"
            } else {
                *val
            };
            let v = val.to_string();
            col = col.child(setting_radio_row(
                format!("{key}-preset-{}", if val.is_empty() { "default" } else { val }),
                selected,
                label,
                *note,
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.set_model_setting(key, &v, cx)
                }),
            ));
        }
        col = col.child(setting_radio_row(
            format!("{key}-preset-custom"),
            is_custom || editing,
            "Custom…",
            "any model id the CLI accepts",
            cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_setting_edit(key, slot, window, cx)
            }),
        ));
        // the custom VALUE (and its inline editor) only when it's the live choice —
        // an "account default" row rendered under Custom… would read as a second,
        // contradictory answer to the same question.
        if is_custom || editing {
            col = col.child(self.render_text_setting_row(key, slot, cx));
        }
        col.into_any_element()
    }

    /// Persist + apply one background-prompt model preset (#57). Same tail as
    /// `commit_setting_text` — the live prompt config is rebuilt so the change
    /// takes effect on the next background call, not after a restart.
    fn set_model_setting(&mut self, key: &'static str, val: &str, cx: &mut Context<Self>) {
        if let Ok(store) = self.store.lock() {
            let _ = store.set_setting(key, val);
        }
        // picking a preset ANSWERS the question the Custom… editor was asking.
        if self.setting_edit.as_ref().is_some_and(|e| e.key == key) {
            self.setting_edit = None;
        }
        self.reload_prompt_config();
        cx.notify();
    }

    /// Open the shared inline editor on one string setting, prefilled with the
    /// stored value. Factored out because both the "Edit" affordance and the
    /// "Custom…" preset row enter the same state.
    fn open_setting_edit(
        &mut self,
        key: &'static str,
        slot: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let buf = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.get_setting(key))
            .unwrap_or_default();
        self.setting_edit = Some(SettingEdit { key, slot, buf });
        self.seed_inline_caret(InlineTarget::SettingText);
        // never two inline editors at once (mirrors how the profile Edit/Add
        // listeners clear setting_edit).
        if let Some(d) = self.profile_draft.as_mut() {
            d.editing = None;
        }
        // the Settings window's own router owns the keystream from here.
        self.focus_settings_input(window);
        cx.notify();
    }

    /// One editable string-setting row (Phase 4): shows the stored value (or
    /// "account default" when empty) with an Edit affordance, OR — while this
    /// key's faux-input is active — the inline_input itself. Reusable: `key` is
    /// the store key, `slot` the inline-input's slot label.
    fn render_text_setting_row(
        &self,
        key: &'static str,
        slot: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // this key's faux-input is live → render the shared inline editor with
        // the slot label carried on the edit itself.
        if let Some(edit) = self.setting_edit.as_ref().filter(|e| e.key == key) {
            return outlinepane::inline_input(edit.slot, &edit.buf, self.inline_caret).into_any_element();
        }
        let cur = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.get_setting(key))
            .unwrap_or_default();
        let is_default = cur.trim().is_empty();
        let shown = if is_default {
            "account default".to_string()
        } else {
            cur
        };
        // NOT a `flex_1().min_w_0().truncate()` row with Edit beside it. The value
        // here is a MODEL ID — the only place the stored id is ever shown, and the
        // part an ellipsis eats is the tail, which is the whole identity of a
        // hand-typed one. Same principle as the projects-root row, and the same
        // shape: the value wraps on its own line, the affordance sits under it
        // (which is also what the account-folder field already does).
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(if is_default { MUTED2 } else { TEXT_STRONG }))
                    .child(SharedString::from(shown)),
            )
            .child(
                div()
                    .id(SharedString::from(format!("edit-{key}")))
                    .px(px(11.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(0x2C5246))
                    .text_size(px(12.5))
                    .text_color(rgb(ACCENT))
                    .hover(|h| h.bg(rgb(0x16231D)))
                    .child("Edit")
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.open_setting_edit(key, slot, window, cx)
                    })),
            )
            .into_any_element()
    }

    /// Persist + apply the currently-edited string setting (Phase 4): write the
    /// store key, re-apply the live prompt config so the model change takes
    /// effect immediately, then close the editor. ⏎ path from route_inline_key.
    pub(crate) fn commit_setting_text(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.setting_edit.take() else {
            return;
        };
        let val = edit.buf.trim().to_string();
        if let Ok(store) = self.store.lock() {
            let _ = store.set_setting(edit.key, &val);
        }
        self.reload_prompt_config();
        cx.notify();
    }

    /// Rebuild the live background-prompt config from the stored provider + both
    /// model keys and push it to `extract` — so a plumbing/structural model edit
    /// takes effect without a restart (mirrors set_prompt_provider's tail).
    fn reload_prompt_config(&mut self) {
        let mut plumbing = None;
        let mut structural = None;
        if let Ok(store) = self.store.lock() {
            plumbing = store.get_setting("prompt_plumbing_model");
            structural = store.get_setting("prompt_structural_model");
        }
        let config = extract::PromptConfig::from_settings(
            Some(self.prompt_provider.key()),
            plumbing.as_deref(),
            structural.as_deref(),
        );
        self.prompt_provider = config.provider;
        extract::set_prompt_config(config);
    }

    /// Profiles — which account NEW sessions use per CLI (#56), then CRUD over
    /// the accounts themselves. The picker comes first deliberately: the default
    /// is the setting a user actually has an opinion about; the card list below
    /// is where those options come from.
    fn render_settings_profiles(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let profiles = self
            .store
            .lock()
            .map(|s| s.profiles())
            .unwrap_or_default();

        // ONE question — "which account do new sessions use?" — asked once, with a
        // row group per CLI. This was two stacked section cards, two headers and
        // two paragraphs, and the second paragraph opened with "The same, for
        // ⇧⌘T": the page said one concept twice and the copy admitted it. The
        // per-CLI split is real (a claude default must never reach a codex spawn)
        // but it is a row LABEL, not a second setting.
        let mut defaults = div().flex().flex_col().gap(px(12.));
        for (kind, label) in DEFAULT_PROFILE_CLIS {
            defaults = defaults.child(labeled_field(
                label,
                self.render_default_profile_picker(kind, &profiles, cx),
            ));
        }

        let mut col = settings_body()
            .child(settings_section(
                "Default account for new sessions",
                "Which account a new session starts under. The spawn menu still overrides it one session at a time; delete a profile and new sessions fall back to your ambient login.",
                defaults,
            ))
            // Add profile — opens an empty draft with the label field focused.
            .child(
                div()
                    .id("profile-add")
                    .px(px(13.))
                    .py(px(7.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(0x2C5246))
                    .text_size(px(13.))
                    .text_color(rgb(ACCENT))
                    .hover(|h| h.bg(rgb(0x16231D)))
                    .child("＋ Add profile")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.profile_draft = Some(ProfileDraft {
                            editing_id: None,
                            label: String::new(),
                            cli_kind: CliKind::Claude,
                            config_dir: None,
                            model: String::new(),
                            extra_args: String::new(),
                            env: String::new(),
                            editing: Some(DraftSlot::Label),
                        });
                        // never two inline editors at once.
                        this.setting_edit = None;
                        this.focus_settings_input(window);
                        cx.notify();
                    })),
            );

        // the draft editor sits above the list while open.
        if self.profile_draft.is_some() {
            col = col.child(self.render_profile_draft(cx));
        }

        if profiles.is_empty() {
            col = col.child(
                div()
                    .p(px(15.))
                    .rounded(px(12.))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(HAIR))
                    .text_size(px(12.5))
                    .text_color(rgb(MUTED))
                    .child("No profiles yet — Add one to run a second claude/codex account."),
            );
        } else {
            for p in &profiles {
                col = col.child(self.render_profile_card(p, cx));
            }
        }
        col
    }

    /// The per-CLI "which account do new sessions use" picker (#56). Writes the
    /// exact setting key `spawn::resolve_spawn_profile` reads, so a pick takes
    /// effect on the very next ⌘T with no restart. A stored id whose profile was
    /// deleted — or edited to the OTHER CLI — resolves to no match here and to
    /// ambient at spawn (`resolve_spawn_profile` filters the default lane by
    /// kind too): the same answer from both ends. Both write paths clear the key
    /// anyway, so the agreement is a backstop, not the mechanism.
    fn render_default_profile_picker(
        &self,
        kind: CliKind,
        profiles: &[ProfileRow],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mine: Vec<&ProfileRow> = profiles
            .iter()
            .filter(|p| p.cli_kind == kind.label())
            .collect();
        let cur = crate::spawn::default_profile_key(kind)
            .and_then(|k| self.store.lock().ok().and_then(|s| s.get_setting(k)))
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|id| mine.iter().any(|p| p.id == *id));

        let mut col = div()
            .flex()
            .flex_col()
            .gap(px(5.))
            .child(setting_radio_row(
                format!("default-profile-{}-ambient", kind.label()),
                cur.is_none(),
                "(ambient account)",
                "whatever the CLI is already logged into",
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.set_default_profile(kind, None, cx)
                }),
            ));
        for p in &mine {
            let id = p.id;
            let note = p
                .config_dir
                .clone()
                .filter(|d| !d.is_empty())
                .unwrap_or_default();
            col = col.child(setting_radio_row(
                format!("default-profile-{}-{id}", kind.label()),
                cur == Some(id),
                SharedString::from(p.label.clone()),
                SharedString::from(note),
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.set_default_profile(kind, Some(id), cx)
                }),
            ));
        }
        if mine.is_empty() {
            col = col.child(
                div()
                    .text_size(px(11.5))
                    .text_color(rgb(MUTED2))
                    // terse on purpose: both groups now live in ONE card, so the
                    // long version of this line printed twice, directly above an
                    // empty-state card that says it a third time.
                    .child(SharedString::from(format!(
                        "No {} profiles yet — add one below.",
                        kind.label()
                    ))),
            );
        }
        col.into_any_element()
    }

    /// Persist the per-CLI default profile (#56). `None` writes the EMPTY string
    /// rather than deleting the row: "" is the documented ambient value, and an
    /// explicit "" is the difference between "I chose ambient" and "never asked".
    fn set_default_profile(&mut self, kind: CliKind, id: Option<i64>, cx: &mut Context<Self>) {
        let Some(key) = crate::spawn::default_profile_key(kind) else {
            return;
        };
        if let Ok(store) = self.store.lock() {
            let _ = store.set_setting(key, &id.map(|i| i.to_string()).unwrap_or_default());
        }
        cx.notify();
    }

    /// One profile as a card (Phase 5): color chip · label · "<cli> · <dir>",
    /// with Edit (opens a prefilled draft) and Delete (✕ → delete_profile).
    fn render_profile_card(&self, p: &ProfileRow, cx: &mut Context<Self>) -> impl IntoElement {
        let id = p.id;
        let chip = p
            .color
            .as_deref()
            .filter(|c| !c.is_empty())
            .and_then(|c| u32::from_str_radix(c.trim_start_matches('#'), 16).ok());
        let dir_shown = p
            .config_dir
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(|d| d.to_string())
            .unwrap_or_else(|| "(default account)".to_string());
        let meta = format!("{} · {}", p.cli_kind, dir_shown);
        // captured-by-move clones for the Edit listener (prefill the draft).
        let e_label = p.label.clone();
        let e_kind = cli_kind_from_str(&p.cli_kind);
        let e_dir = p.config_dir.clone();
        // "" is the draft's own spelling of "the account's own default", which is
        // what a NULL model column means.
        let e_model = p.model.clone().unwrap_or_default();
        // argv comes back OUT as the string a human can edit, re-quoted so a
        // stored `["--append-system-prompt", "be terse"]` doesn't reopen as three
        // separate arguments the moment the user saves again.
        let e_extra = fmt_extra_args(&p.extra_args);
        let e_env = fmt_profile_env(&p.env);

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .p(px(15.))
            .rounded(px(12.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(HAIR))
            .when_some(chip, |d, c| {
                d.child(
                    div()
                        .flex_none()
                        .w(px(12.))
                        .h(px(12.))
                        .rounded(px(6.))
                        .bg(rgb(c)),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    // the card's TITLE — a name the user typed, in a list of
                    // cards — so it truncates rather than reflowing the row.
                    // No min_w_0: same column-collapse trap as setting_radio_row's
                    // label (see the comment there) — it would render as a bare "…".
                    .child(
                        div()
                            .truncate()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT_STRONG))
                            .child(SharedString::from(p.label.clone())),
                    )
                    // "<cli> · <account folder>" — a PATH, and the only place this
                    // profile's config dir is ever shown. It wraps: clipping the
                    // tail of a path hides exactly the segment that identifies it.
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child(SharedString::from(meta)),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("profile-edit-{id}")))
                    .flex_none()
                    .px(px(11.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(0x2C5246))
                    .text_size(px(12.5))
                    .text_color(rgb(ACCENT))
                    .hover(|h| h.bg(rgb(0x16231D)))
                    .child("Edit")
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.profile_draft = Some(ProfileDraft {
                            editing_id: Some(id),
                            label: e_label.clone(),
                            cli_kind: e_kind,
                            config_dir: e_dir.clone(),
                            model: e_model.clone(),
                            extra_args: e_extra.clone(),
                            env: e_env.clone(),
                            editing: None,
                        });
                        this.setting_edit = None;
                        this.focus_settings_input(window);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id(SharedString::from(format!("profile-del-{id}")))
                    .flex_none()
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(HAIR))
                    .text_size(px(12.5))
                    .text_color(rgb(MUTED))
                    .hover(|h| h.border_color(rgb(0xE68A8A)).text_color(rgb(0xE68A8A)))
                    .child("✕")
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Ok(store) = this.store.lock() {
                            let _ = store.delete_profile(id);
                            // a default pointing at the row we just deleted is a
                            // lie the picker would keep reading. Spawn already
                            // folds a dangling id to ambient, but persisted state
                            // shouldn't need that safety net to be honest (#56).
                            crate::spawn::reconcile_default_profile_keys(&store, id, None);
                        }
                        // a draft that was editing this very row is now stale.
                        if this.profile_draft.as_ref().and_then(|d| d.editing_id) == Some(id) {
                            this.profile_draft = None;
                        }
                        cx.notify();
                    })),
            )
    }

    /// The add/edit draft editor (Phase 5) — rendered when `profile_draft` is
    /// Some. label (free text, faux-input) · cli_kind (radio) · config_dir
    /// (folder picker) · model (per-kind preset radio + Custom…, #62) · extra
    /// args (free text, split into argv on Save, #61) · Save/Cancel.
    fn render_profile_draft(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // safe: only called when profile_draft.is_some().
        let draft = self.profile_draft.as_ref().unwrap();
        let editing = draft.editing_id.is_some();

        // ── label — FREE TEXT (faux-input when active, else value + Edit) ──
        let label_body =
            self.render_draft_text_row(DraftSlot::Label, &draft.label, "(unnamed)", cx);

        // ── cli_kind — claude / codex radio ──
        let mut kinds = div().flex().flex_col().gap(px(5.));
        for (k, lbl) in [(CliKind::Claude, "claude"), (CliKind::Codex, "codex")] {
            let selected = draft.cli_kind == k;
            kinds = kinds.child(setting_radio_row(
                format!("profile-draft-kind-{lbl}"),
                selected,
                lbl,
                "",
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if let Some(d) = this.profile_draft.as_mut() {
                        // model ids are per-CLI — a kind change invalidates any
                        // picked OR hand-typed model, so fall back to the account
                        // default and close an editor that is now asking about
                        // the wrong CLI's ids.
                        if d.cli_kind != k {
                            d.model.clear();
                            if d.editing == Some(DraftSlot::Model) {
                                d.editing = None;
                            }
                        }
                        d.cli_kind = k;
                    }
                    cx.notify();
                }),
            ));
        }

        // ── config_dir — folder picker + chosen path (or default account) ──
        let dir_shown = draft
            .config_dir
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(|d| d.to_string())
            .unwrap_or_else(|| "(default account)".to_string());
        let config_dir = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            // the chosen CLAUDE_CONFIG_DIR / CODEX_HOME, on its own wrapping line:
            // it is the whole point of the field, and it is what a user checks
            // before Save. A truncated path checks nothing.
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(rgb(MUTED))
                    .child(SharedString::from(dir_shown)),
            )
            .child(
                div()
                    .id("profile-draft-dir")
                    .px(px(11.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(0x2C5246))
                    .text_size(px(12.5))
                    .text_color(rgb(ACCENT))
                    .hover(|h| h.bg(rgb(0x16231D)))
                    .child("Choose account folder…")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.choose_profile_config_dir(cx)
                    })),
            );

        // ── model — a per-kind preset radio plus the SAME Custom… escape hatch
        // the Background AI picker uses (#62). Same ids as those presets, and for
        // the same reason: they go straight to the CLI as `--model` / `-m`, so a
        // stale list here is a profile that fails to spawn. Without the hatch, a
        // stored id we ship no preset for — `claude-opus-4-8`, dropped when the
        // list was last refreshed — rendered with NOTHING selected while still
        // riding every spawn under this profile: invisible, unreadable, and
        // silently clobbered by the first click on any other row.
        let presets: &[&str] = match draft.cli_kind {
            CliKind::Claude => CLAUDE_MODEL_IDS,
            CliKind::Codex => CODEX_MODEL_IDS,
            _ => &[],
        };
        let editing_model = draft.editing == Some(DraftSlot::Model);
        let sel = selected_model_row(&draft.model, presets, editing_model);
        let mut models = div().flex().flex_col().gap(px(5.)).child(setting_radio_row(
            "profile-draft-model-default",
            sel == ModelRow::AccountDefault,
            "(account default)",
            "",
            cx.listener(move |this, _: &ClickEvent, _, cx| this.set_draft_model("", cx)),
        ));
        for (i, preset) in presets.iter().enumerate() {
            let val = (*preset).to_string();
            models = models.child(setting_radio_row(
                format!("profile-draft-model-{preset}"),
                sel == ModelRow::Preset(i),
                *preset,
                "",
                cx.listener(move |this, _: &ClickEvent, _, cx| this.set_draft_model(&val, cx)),
            ));
        }
        models = models.child(setting_radio_row(
            "profile-draft-model-custom",
            sel == ModelRow::Custom,
            "Custom…",
            "any model id this CLI accepts",
            cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_draft_edit(DraftSlot::Model, window, cx)
            }),
        ));
        // the custom VALUE (and its editor) only while it IS the live choice — an
        // "(account default)" line under a selected Custom… row would read as a
        // second, contradictory answer to the same question.
        if sel == ModelRow::Custom {
            models = models.child(self.render_draft_text_row(
                DraftSlot::Model,
                &draft.model,
                "(account default)",
                cx,
            ));
        }

        // ── env — free text, parsed into KEY=value pairs on Save ──
        //
        // The store and the spawn path have carried profile.env since the
        // beginning (env_json -> spec.env, spawn.rs); the editor simply never
        // showed it, and Save preserved whatever was already there. It is a real
        // field now, so Save writes what you typed.
        let env_pairs = parse_profile_env(&draft.env);
        let env_preview = if env_pairs.is_empty() {
            "nothing exported — sessions under this profile inherit Kod's environment only"
                .to_string()
        } else {
            let mut keys: Vec<&str> = env_pairs.keys().map(|k| k.as_str()).collect();
            keys.sort();
            format!(
                "exports {} variable{}: {}",
                keys.len(),
                if keys.len() == 1 { "" } else { "s" },
                keys.join(" · ")
            )
        };
        let env_row = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(self.render_draft_text_row(DraftSlot::Env, &draft.env, "(none)", cx))
            // KEYS only, never the values: a profile's env is where an API key
            // ends up, and echoing secrets into a settings pane that anyone can
            // screen-share is a bad trade for confirmation you already have from
            // the count. Whether a pair PARSED is the thing you can get wrong,
            // and the key list shows that.
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(rgb(MUTED2))
                    .child(SharedString::from(env_preview)),
            );

        // ── extra args — free text, split into argv on Save (#61) ──
        let parsed = parse_extra_args(&draft.extra_args);
        let preview = if parsed.is_empty() {
            "nothing appended — sessions under this profile get Kod's own flags only".to_string()
        } else {
            format!(
                "appends {} argument{}: {}",
                parsed.len(),
                if parsed.len() == 1 { "" } else { "s" },
                parsed.join(" · ")
            )
        };
        let extra_args = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(self.render_draft_text_row(
                DraftSlot::ExtraArgs,
                &draft.extra_args,
                "(none)",
                cx,
            ))
            // show the SPLIT, not an echo of what was typed: whether `be terse`
            // survived as one argument or became two is the only thing about this
            // field a user can get wrong, and `·` between arguments is the only
            // way to see which happened before saving.
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(rgb(MUTED2))
                    .child(SharedString::from(preview)),
            );

        // ── Save / Cancel ──
        let actions = div()
            .flex()
            .flex_row()
            .gap(px(8.))
            .child(
                div()
                    .id("profile-draft-save")
                    .px(px(13.))
                    .py(px(7.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(0x346B54))
                    .bg(rgb(CARD2))
                    .text_size(px(13.))
                    .text_color(rgb(ACCENT))
                    .hover(|h| h.bg(rgb(0x16231D)))
                    .child("Save")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_profile_draft(cx))),
            )
            .child(
                div()
                    .id("profile-draft-cancel")
                    .px(px(13.))
                    .py(px(7.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(HAIR))
                    .text_size(px(13.))
                    .text_color(rgb(MUTED))
                    .hover(|h| h.border_color(rgb(0x2C5246)))
                    .child("Cancel")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.profile_draft = None;
                        cx.notify();
                    })),
            );

        let body = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(labeled_field("Label", label_body))
            .child(labeled_field("CLI", kinds.into_any_element()))
            .child(labeled_field("Account folder", config_dir.into_any_element()))
            .child(labeled_field("Model", models.into_any_element()))
            .child(labeled_field("Extra args", extra_args.into_any_element()))
            .child(labeled_field("Env", env_row.into_any_element()))
            .child(actions);

        settings_section(
            if editing { "Edit profile" } else { "New profile" },
            "A named account: an isolated config home (CLAUDE_CONFIG_DIR / CODEX_HOME), a default model, and extra CLI flags every spawn under it adopts. Env overrides stay advanced — left untouched here.",
            body,
        )
    }

    /// Save the open draft (Phase 5): create a new profile or update the edited
    /// one. `env`/`color` are still NOT surfaced in this editor — a NEW profile
    /// gets empty defaults, an EDIT preserves the row's existing values (never
    /// silently wiped). An empty label re-opens the label editor instead.
    fn save_profile_draft(&mut self, cx: &mut Context<Self>) {
        // an unnamed profile is not saveable — put the cursor back in the label.
        if self
            .profile_draft
            .as_ref()
            .is_some_and(|d| d.label.trim().is_empty())
        {
            if let Some(d) = self.profile_draft.as_mut() {
                d.editing = Some(DraftSlot::Label);
            }
            self.seed_inline_caret(InlineTarget::ProfileField);
            cx.notify();
            return;
        }
        let Some(draft) = self.profile_draft.take() else {
            return;
        };
        // `color` is the only advanced field this editor still doesn't show, so
        // it is preserved on an EDIT; a NEW profile folds to None. `env` USED to
        // be preserved the same way — it is edited directly now, so preserving
        // it here would silently discard whatever was just typed.
        let color = draft
            .editing_id
            .and_then(|id| self.store.lock().ok().and_then(|s| s.profile(id)))
            .and_then(|r| r.color);
        let label = draft.label.trim();
        let cli_kind = draft.cli_kind.label();
        let config_dir = draft.config_dir.as_deref().filter(|s| !s.is_empty());
        let model = draft.model.trim();
        let model = (!model.is_empty()).then_some(model);
        // An EMPTY field is NO extra args, never a one-element `[""]`: an empty
        // argv entry reaches the CLI as a bare "" — which codex reads as the
        // prompt, so a profile nobody typed flags into would start every session
        // with a blank turn.
        let extra = parse_extra_args(&draft.extra_args);
        let env = parse_profile_env(&draft.env);
        if let Ok(store) = self.store.lock() {
            match draft.editing_id {
                Some(id) => {
                    let _ = store.update_profile(
                        id,
                        label,
                        cli_kind,
                        config_dir,
                        model,
                        &extra,
                        &env,
                        color.as_deref(),
                    );
                    // A kind flip re-labels an existing account folder, and the
                    // default keys name a profile by ID — so `default_profile_claude`
                    // could still name this row after it became a codex profile,
                    // and ⌘T would spawn claude against a codex config home while
                    // the picker (which filters by kind) showed "(ambient
                    // account)". Same clean-up the delete path does.
                    crate::spawn::reconcile_default_profile_keys(&store, id, Some(cli_kind));
                }
                None => {
                    let _ = store.create_profile(
                        label,
                        cli_kind,
                        config_dir,
                        model,
                        &extra,
                        &env,
                        color.as_deref(),
                    );
                }
            }
        }
        cx.notify();
    }

    /// One free-text row of the profile draft: the shared inline faux-input while
    /// this slot owns the keystream, otherwise the current value plus an Edit
    /// affordance. Exactly `render_text_setting_row`'s shape, against the open
    /// DRAFT instead of a store key — the label, the Custom… model id and the
    /// extra args are the same control three times, and nothing here is
    /// persisted until Save.
    fn render_draft_text_row(
        &self,
        slot: DraftSlot,
        value: &str,
        when_empty: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self
            .profile_draft
            .as_ref()
            .is_some_and(|d| d.editing == Some(slot))
        {
            return outlinepane::inline_input(slot.slot_label(), value, self.inline_caret).into_any_element();
        }
        let empty = value.trim().is_empty();
        let shown = if empty {
            when_empty.to_string()
        } else {
            value.to_string()
        };
        // same column shape as `render_text_setting_row`, for the same reason: two
        // of the three slots are values that only mean anything read WHOLE — the
        // Custom… model id, and the extra-args line whose tail flags are exactly
        // what the preview under it exists to let you check before Save.
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(if empty { MUTED2 } else { TEXT_STRONG }))
                    .child(SharedString::from(shown)),
            )
            .child(
                div()
                    .id(SharedString::from(format!(
                        "profile-draft-{}-edit",
                        slot.key()
                    )))
                    .px(px(11.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(0x2C5246))
                    .text_size(px(12.5))
                    .text_color(rgb(ACCENT))
                    .hover(|h| h.bg(rgb(0x16231D)))
                    .child("Edit")
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.open_draft_edit(slot, window, cx)
                    })),
            )
            .into_any_element()
    }

    /// Hand the Settings window's keystream to one of the draft's free-text
    /// fields. Mirrors `open_setting_edit`: never two inline editors at once, and
    /// the window's OWN sink takes focus or the faux-input receives nothing.
    fn open_draft_edit(&mut self, slot: DraftSlot, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = self.profile_draft.as_mut() {
            d.editing = Some(slot);
        }
        self.seed_inline_caret(InlineTarget::ProfileField);
        self.setting_edit = None;
        self.focus_settings_input(window);
        cx.notify();
    }

    /// Pick one of the draft's model rows; "" is the account's own default.
    /// Picking one ANSWERS the question the Custom… editor was asking, so it
    /// closes that editor — otherwise the selected radio and the faux-input under
    /// it would disagree about which id is live (mirrors `set_model_setting`).
    fn set_draft_model(&mut self, val: &str, cx: &mut Context<Self>) {
        if let Some(d) = self.profile_draft.as_mut() {
            d.model = val.to_string();
            if d.editing == Some(DraftSlot::Model) {
                d.editing = None;
            }
        }
        cx.notify();
    }

    /// Ask macOS for a folder and adopt it as the open draft's account config
    /// home (Phase 5). Same picker as choose_projects_root; a path is picked, not
    /// typed (this app has no real text field).
    fn choose_profile_config_dir(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(dir) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(d) = this.profile_draft.as_mut() {
                            d.config_dir = Some(dir.display().to_string());
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

}

impl DraftSlot {
    /// The faux-input's slot label — what the user reads while typing into it.
    fn slot_label(self) -> &'static str {
        match self {
            Self::Label => "label",
            Self::Model => "model id",
            Self::ExtraArgs => "extra args",
            Self::Env => "env",
        }
    }

    /// Stable element-id fragment for this slot's Edit affordance. Separate from
    /// `slot_label` because an id may not carry a space.
    fn key(self) -> &'static str {
        match self {
            Self::Label => "label",
            Self::Model => "model",
            Self::ExtraArgs => "extra-args",
            Self::Env => "env",
        }
    }
}

/// Which row of the per-profile model picker draws as selected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ModelRow {
    /// "" — whatever this account's own login already uses.
    AccountDefault,
    /// index into the preset list for this draft's CLI.
    Preset(usize),
    /// an id we ship no preset for: hand-typed, or from a preset list that has
    /// since moved on.
    Custom,
}

/// Pure, and factored out because it IS the bug #62.2 came to fix: a stored id
/// outside the preset list must land on Custom…, never on "no row at all" —
/// which is what the per-profile picker used to render for `claude-opus-4-8`
/// after that id was dropped, while the value went on riding every spawn under
/// the profile. Same rule the Background AI picker applies to its own presets.
fn selected_model_row(model: &str, presets: &[&str], editing: bool) -> ModelRow {
    // an OPEN Custom… editor owns the selection before a character is typed:
    // otherwise clicking Custom… would leave "(account default)" lit and the
    // user would be typing into a row nothing points at.
    if editing {
        return ModelRow::Custom;
    }
    let model = model.trim();
    if model.is_empty() {
        return ModelRow::AccountDefault;
    }
    match presets.iter().position(|p| *p == model) {
        Some(i) => ModelRow::Preset(i),
        None => ModelRow::Custom,
    }
}

/// Split the profile draft's free-text extra-args field into real argv (#61).
///
/// Handles the only two things that matter for a line of CLI flags: whitespace
/// separates arguments, and a quoted run stays one argument
/// (`--append-system-prompt "be terse"`). No escapes, no globbing, no `$VAR`,
/// no `|` — these strings are handed to `Command::arg`, never to a shell, so
/// anything fancier would be a promise the spawn path cannot keep.
///
/// An empty (or all-whitespace) field yields NO arguments rather than one empty
/// one: `[""]` would reach the CLI as a bare `""`, which codex reads as the
/// prompt. An explicitly quoted `""` is still honoured — that one the user asked
/// for, out loud.
/// `KEY=value KEY2="a b"` -> the env map a spawn exports.
///
/// Deliberately built ON TOP of `parse_extra_args` rather than beside it: the
/// quoting rules are already implemented and tested there, and a second parser
/// would drift. That function already yields `--foo=a b` from `--foo="a b"`,
/// which is exactly the shape an env pair needs.
///
/// A token with no `=`, or an illegal variable name, is DROPPED. Env names are
/// `[A-Za-z_][A-Za-z0-9_]*` — anything else is not something a process can
/// receive, so keeping it would only produce a setting that silently never
/// applies.
fn parse_profile_env(s: &str) -> std::collections::HashMap<String, String> {
    let legal = |k: &str| {
        !k.is_empty()
            && !k.starts_with(|c: char| c.is_ascii_digit())
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    parse_extra_args(s)
        .into_iter()
        .filter_map(|tok| {
            let (k, v) = tok.split_once('=')?;
            legal(k).then(|| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// The inverse, so opening a profile and pressing Save without touching
/// anything is a no-op.
///
/// SORTED BY KEY, which is not cosmetic: a HashMap iterates in a random order,
/// so without this the field would reshuffle itself every time the editor
/// opened and a Save would look like an edit.
fn fmt_profile_env(env: &std::collections::HashMap<String, String>) -> String {
    let mut pairs: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    fmt_extra_args(&pairs)
}

fn parse_extra_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    // "has an argument been started" — distinct from `!cur.is_empty()`, which
    // cannot tell a quoted empty argument from no argument at all.
    let mut started = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// Render stored argv back into the field the user edits (#61) — the inverse of
/// `parse_extra_args`, so opening a saved profile and pressing Save without
/// touching anything is a no-op rather than a silent re-split. An argument that
/// needs quoting gets the quote character it does NOT itself contain.
///
/// An argument containing BOTH kinds is where the naive version broke: falling
/// back to `"…"` around a value that already held a `"` re-parsed as three
/// pieces and the double quotes vanished from the argv, silently rewriting what
/// every spawn under that profile passes to the CLI. There are no escapes here
/// (deliberately — this is argv, not a shell), but the parser CONCATENATES
/// adjacent quoted runs the way a shell does, so `'"'` renders a literal double
/// quote and the round trip closes without inventing an escape syntax.
fn fmt_extra_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if !a.is_empty() && !a.chars().any(|c| c.is_whitespace() || c == '\'' || c == '"') {
                a.clone()
            } else if a.contains('"') && !a.contains('\'') {
                format!("'{a}'")
            } else if a.contains('"') {
                a.split('"')
                    .map(|run| format!("\"{run}\""))
                    .collect::<Vec<_>>()
                    .join("'\"'")
            } else {
                format!("\"{a}\"")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The model ids Kod offers for claude, newest-first. These strings are passed
/// through verbatim as `claude --model <id>`, so they must be ids the CLI
/// accepts — this list is a contract with the CLI, not decoration. Shared by the
/// Background AI presets (#57) and the per-profile model picker, which drifted
/// apart once already.
const CLAUDE_MODEL_IDS: &[&str] = &["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"];

/// The same for codex (`codex -m <id>`).
const CODEX_MODEL_IDS: &[&str] = &["gpt-5-codex"];

/// The CLIs the "Default account for new sessions" card offers a row group for,
/// each labelled with the shortcut that spawns it — exactly the kinds
/// `spawn::default_profile_key` stores a default for (a shell has no account
/// concept, so it has neither). A LIST rather than one hand-written section per
/// CLI: they are one setting with a row group each, and a third CLI must earn
/// its group by landing here, not by growing the page a third near-identical
/// header and paragraph — which is exactly how this section became confusing.
const DEFAULT_PROFILE_CLIS: [(CliKind, &str); 2] = [
    (CliKind::Claude, "claude · ⌘T"),
    (CliKind::Codex, "codex · ⇧⌘T"),
];

/// Is `model` an id we ship for a provider OTHER than `provider` — i.e. one
/// that provably belongs to the wrong CLI? Deliberately narrow: an unknown id is
/// a Custom… value the user typed for a CLI we don't have a list for, and
/// guessing at those would silently discard a working setting.
fn belongs_to_other_provider(model: &str, provider: extract::PromptProvider) -> bool {
    if model.is_empty() {
        return false;
    }
    let known = |p: extract::PromptProvider| model_presets(p).iter().any(|(v, _)| *v == model);
    let other = match provider {
        extract::PromptProvider::Claude => extract::PromptProvider::Codex,
        extract::PromptProvider::Codex => extract::PromptProvider::Claude,
    };
    known(other) && !known(provider)
}

/// (id, note) rows for a background-prompt model picker, for the provider that
/// is actually selected — "" first, meaning the account's own default. The notes
/// are what make plumbing-vs-structural a decidable choice rather than a list of
/// opaque ids (#57).
fn model_presets(provider: extract::PromptProvider) -> &'static [(&'static str, &'static str)] {
    match provider {
        extract::PromptProvider::Claude => &[
            ("", "whatever your claude login already uses"),
            ("claude-opus-5", "most capable, slowest, priciest"),
            ("claude-sonnet-5", "the balanced middle"),
            ("claude-haiku-4-5", "cheapest and fastest"),
        ],
        extract::PromptProvider::Codex => &[
            ("", "whatever your codex login already uses"),
            ("gpt-5-codex", "codex's coding model"),
        ],
    }
}

/// The shared body column every section returns: full width up to a 640px
/// reading measure, centered by the window's scroller. 198 rail + 26+26 gutters
/// + 640 body = 890, so the 900×640 default opens with the column exactly
/// comfortable and the 720 minimum still shows the rail plus a readable card.
fn settings_body() -> Div {
    div()
        .w_full()
        .max_w(px(640.))
        .flex()
        .flex_col()
        .gap(px(10.))
}

/// One rail row (#54). A rail row is a PLACE, so it reads as a selected list
/// item — a filled slab with a left accent bar — not as the pill-shaped button
/// the old tab bar used, which read as "press me" and made five of them look
/// like a toolbar.
fn settings_rail_row(
    section: SettingsSection,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(SharedString::from(format!("rail-{}", section.key())))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(9.))
        .px(px(9.))
        .py(px(7.))
        .rounded(px(8.))
        .cursor_pointer()
        .bg(rgb(if selected { CARD2 } else { PANEL }))
        .hover(|h| h.bg(rgb(CARD)))
        .on_click(on_click)
        // the accent bar is always laid out, only ever recolored — otherwise
        // every label shifts 12px sideways as the selection moves.
        .child(
            div()
                .flex_none()
                .w(px(3.))
                .h(px(15.))
                .rounded(px(2.))
                .when(selected, |d| d.bg(rgb(ACCENT))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(13.))
                .text_color(rgb(if selected { TEXT_STRONG } else { MUTED }))
                .when(selected, |d| d.font_weight(FontWeight::SEMIBOLD))
                .child(section.label()),
        )
}

/// One labeled field: a small caps label above its control, so a vertical stack
/// reads as named fields. Two users, the same shape — the profile draft's five
/// rows, and the per-CLI groups inside the default-account card, where the label
/// is all that is left of what used to be a whole second section.
fn labeled_field(label: &'static str, control: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(5.))
        .child(
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(MUTED2))
                .child(label),
        )
        .child(control)
}

/// Parse a stored `cli_kind` string back into the enum (unknown ⇒ Claude — the
/// safe default; a profile only ever stores "claude"/"codex" today).
fn cli_kind_from_str(s: &str) -> CliKind {
    match s {
        "codex" => CliKind::Codex,
        "shell" => CliKind::Shell,
        _ => CliKind::Claude,
    }
}

/// One selectable settings row — the ● / ○ indicator, a bold `label`, and an
/// optional muted `note`, on the shared CARD/CARD2 background with the
/// `0x346B54` selected-border treatment. Factored from the four ~30-line row
/// blocks the modal duplicated (effort / provider / summaries).
///
/// The `label` truncates and the `note` does not, deliberately — see each.
fn setting_radio_row(
    id: impl Into<SharedString>,
    selected: bool,
    label: impl Into<SharedString>,
    note: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let note = note.into();
    let has_note = !note.is_empty();
    div()
        .id(id.into())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .px(px(11.))
        .py(px(8.))
        .rounded(px(9.))
        .cursor_pointer()
        .bg(rgb(if selected { CARD2 } else { CARD }))
        .border_1()
        .border_color(rgb(if selected { 0x346B54 } else { HAIR }))
        .hover(|h| h.border_color(rgb(0x2C5246)))
        .on_click(on_click)
        .child(
            div()
                .w(px(12.))
                .flex_none()
                .text_size(px(12.))
                .text_color(rgb(if selected { ACCENT } else { MUTED2 }))
                .child(if selected { "●" } else { "○" }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                // an OPTION NAME, so it truncates: these rows are a list you scan
                // down, and one long name must not make its row two lines tall.
                //
                // NO `min_w_0()` here, and that is the whole bug this row once had:
                // min-width:0 is what lets a flex-ROW child shrink below its content.
                // This is a COLUMN child, which already gets its width from the
                // parent by stretch — so min_w_0 only removed the floor and let the
                // box collapse to zero, at which point `truncate()` had nothing to
                // show but the ellipsis. Every option in this window rendered as a
                // bare "…" while its note underneath read fine.
                .child(
                    div()
                        .truncate()
                        .text_size(px(13.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_STRONG))
                        .child(label.into()),
                )
                // the note WRAPS. In the default-account picker it carries the
                // profile's config dir — the one string that tells two claude
                // accounts apart — and an ellipsis ate it from the END, which is
                // where the difference lives (`~/.claude-work` vs `…-personal`).
                // Nothing else on screen shows it, so a clipped note is a value
                // the user cannot read anywhere. Same call the projects-root row
                // makes. The shipped notes are all short and never wrap.
                .when(has_note, |c| {
                    c.child(
                        div()
                            .text_size(px(11.5))
                            .text_color(rgb(MUTED2))
                            .child(note),
                    )
                }),
        )
}

/// An on/off settings row (summaries, auto-continue).
///
/// This used to delegate to `setting_radio_row`, which was wrong: a radio row
/// draws ONE option with a ● / ○ dot, so an on/off setting rendered as a single
/// permanently-selected entry with nothing to switch *to* — it read as "this is
/// locked on", not "click to turn off". Toggles now draw a real track+knob
/// switch, which is unmistakably flippable. Each caller wraps this in a
/// `settings_section` that names the setting, so `label` carries the state word.
fn setting_toggle_row(
    id: impl Into<SharedString>,
    on: bool,
    label: impl Into<SharedString>,
    note: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let note = note.into();
    let has_note = !note.is_empty();
    // The knob sits at one end of the track; `justify_end` when on slides it right.
    let knob = div()
        .w(px(14.))
        .h(px(14.))
        .flex_none()
        .rounded(px(7.))
        .bg(rgb(if on { TEXT_STRONG } else { MUTED2 }));
    let track = div()
        .w(px(32.))
        .h(px(18.))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .when(on, |t| t.justify_end())
        .px(px(2.))
        .rounded(px(9.))
        .bg(rgb(if on { 0x346B54 } else { CARD2 }))
        .border_1()
        .border_color(rgb(if on { 0x346B54 } else { HAIR }))
        .child(knob);
    div()
        .id(id.into())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .px(px(11.))
        .py(px(8.))
        .rounded(px(9.))
        .cursor_pointer()
        .bg(rgb(if on { CARD2 } else { CARD }))
        .border_1()
        .border_color(rgb(if on { 0x346B54 } else { HAIR }))
        .hover(|h| h.border_color(rgb(0x2C5246)))
        .on_click(on_click)
        .child(track)
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(13.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(if on { TEXT_STRONG } else { MUTED }))
                        .child(label.into()),
                )
                .when(has_note, |c| {
                    c.child(
                        div()
                            .text_size(px(11.5))
                            .text_color(rgb(MUTED2))
                            .child(note),
                    )
                }),
        )
}

/// A grouped settings section card: a bold `title`, a muted `desc`, then `body`
/// (the control rows), on a PANEL card so related settings read as one group.
fn settings_section(
    title: impl Into<SharedString>,
    desc: impl Into<SharedString>,
    body: impl IntoElement,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(9.))
        .p(px(15.))
        .rounded(px(12.))
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(HAIR))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_STRONG))
                        .child(title.into()),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child(desc.into()),
                ),
        )
        .child(body)
}

/// The small uppercase label that heads a group of section cards. The rail row
/// and the content header now say what each SECTION is, so this survives as
/// exactly one sub-divider: the PROMPT MODELS pair inside Background AI, which
/// really is a group within a page rather than a page of its own.
fn settings_group_header(label: impl Into<SharedString>) -> impl IntoElement {
    div()
        .pt(px(6.))
        .pb(px(1.))
        .px(px(2.))
        .text_size(px(11.))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(MUTED2))
        .child(label.into())
}

#[cfg(test)]
mod tests {
    use super::{
        belongs_to_other_provider, cli_kind_from_str, fmt_extra_args, fmt_profile_env,
        model_presets, parse_profile_env,
        parse_extra_args, selected_model_row, ModelRow, SettingsSection, CLAUDE_MODEL_IDS,
        CODEX_MODEL_IDS, DEFAULT_PROFILE_CLIS,
    };
    use crate::extract::PromptProvider;
    use crate::spawn::{default_profile_key, reconcile_default_profile_keys};
    use crate::DraftSlot;
    use orchestrator_host::session::CliKind;
    use orchestrator_store::Store;
    use std::collections::HashMap;

    /// The bug #57 came to fix, frozen. The Background AI presets and the
    /// per-profile model picker are two lists of the SAME ids, and they had
    /// already drifted once (the profile list still offered `claude-opus-4-8`
    /// long after the Background AI copy moved on). Both now read the shared
    /// constants; this asserts they still do, id for id and in order.
    #[test]
    fn model_pickers_offer_exactly_the_shared_ids() {
        let non_default = |p: PromptProvider| -> Vec<&'static str> {
            model_presets(p)
                .iter()
                .map(|(v, _)| *v)
                .filter(|v| !v.is_empty())
                .collect()
        };
        assert_eq!(non_default(PromptProvider::Claude), CLAUDE_MODEL_IDS);
        assert_eq!(non_default(PromptProvider::Codex), CODEX_MODEL_IDS);
    }

    /// "" is the ONLY value that means "the account's own default", and it must
    /// be offered first — a picker whose first row is a concrete model would make
    /// "I never chose" unreachable once the user has chosen anything.
    #[test]
    fn every_model_picker_leads_with_the_account_default() {
        for p in PromptProvider::ALL {
            let rows = model_presets(p);
            assert_eq!(rows[0].0, "", "{} must lead with \"\"", p.key());
            assert!(!rows[0].1.is_empty(), "the default row still needs a note");
            // "" exactly once, or two rows would both claim to be selected.
            assert_eq!(rows.iter().filter(|(v, _)| v.is_empty()).count(), 1);
        }
    }

    /// A stored model id is passed STRAIGHT to the selected provider's CLI, so a
    /// claude id left armed after switching to codex becomes `codex exec -m
    /// claude-haiku-4-5` — every background call fails, silently. The provider
    /// switch clears exactly the ids that provably belong to the other CLI.
    #[test]
    fn cross_provider_model_ids_are_the_only_ones_a_switch_clears() {
        for id in CLAUDE_MODEL_IDS {
            assert!(belongs_to_other_provider(id, PromptProvider::Codex), "{id}");
            assert!(!belongs_to_other_provider(id, PromptProvider::Claude));
        }
        for id in CODEX_MODEL_IDS {
            assert!(belongs_to_other_provider(id, PromptProvider::Claude), "{id}");
            assert!(!belongs_to_other_provider(id, PromptProvider::Codex));
        }
        // "" is "account default" and a hand-typed Custom… id is the user's own
        // business — neither may be discarded by a provider switch.
        for p in PromptProvider::ALL {
            assert!(!belongs_to_other_provider("", p));
            assert!(!belongs_to_other_provider("some-private-model-id", p));
        }
    }

    /// Element ids and — because gpui keys scroll offsets by element id — the
    /// per-section scroll position are derived from `key()`. A duplicate would
    /// silently make two sections share one scroll offset.
    #[test]
    fn rail_section_keys_and_labels_are_unique() {
        let mut keys: Vec<&str> = SettingsSection::ALL.iter().map(|s| s.key()).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n, "duplicate rail section key");
        let mut labels: Vec<&str> = SettingsSection::ALL.iter().map(|s| s.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), n, "duplicate rail section label");
        // the blurb is the ONLY line that says what a section is for (#57's
        // complaint about "In-app LLM" was exactly this line being missing).
        assert!(SettingsSection::ALL.iter().all(|s| !s.blurb().is_empty()));
    }

    /// The default-profile picker filters rows with `p.cli_kind == kind.label()`,
    /// and `save_profile_draft` WRITES `draft.cli_kind.label()`. If those two ever
    /// disagree the picker silently lists nothing to choose from.
    #[test]
    fn stored_cli_kind_round_trips_through_label() {
        for kind in [CliKind::Claude, CliKind::Codex, CliKind::Shell] {
            assert_eq!(cli_kind_from_str(kind.label()), kind);
        }
        // anything else folds to claude rather than panicking — a hand-edited
        // row must not take the Profiles section down with it.
        assert_eq!(cli_kind_from_str("gemini"), CliKind::Claude);
        assert_eq!(cli_kind_from_str(""), CliKind::Claude);
    }

    /// #62.2, frozen. A profile whose stored model is not in the current preset
    /// list used to render with NOTHING selected while that id went on riding
    /// every spawn — you could clobber it, but you could never SEE it. It must
    /// land on Custom…, which is what puts the value back on screen.
    #[test]
    fn a_model_id_the_presets_dropped_lands_on_custom_not_on_nothing() {
        // the real one: shipped as a preset, then dropped when the list moved on.
        assert_eq!(
            selected_model_row("claude-opus-4-8", CLAUDE_MODEL_IDS, false),
            ModelRow::Custom
        );
        // ...and so does anything else the CLI would accept but we don't list.
        assert_eq!(
            selected_model_row("some-private-model-id", CLAUDE_MODEL_IDS, false),
            ModelRow::Custom
        );
        // a claude id under a CODEX draft is just as unknown — the picker lists
        // per-CLI ids, so a kind flip must not leave the old id looking chosen.
        assert_eq!(
            selected_model_row("claude-opus-5", CODEX_MODEL_IDS, false),
            ModelRow::Custom
        );
    }

    /// Exactly one row of the picker is ever lit, and the ones we DO ship still
    /// select themselves — the Custom… hatch must not swallow the presets.
    #[test]
    fn every_preset_selects_its_own_row_and_empty_means_account_default() {
        for presets in [CLAUDE_MODEL_IDS, CODEX_MODEL_IDS] {
            for (i, id) in presets.iter().enumerate() {
                assert_eq!(selected_model_row(id, presets, false), ModelRow::Preset(i));
                // whitespace is what a faux-input collects on a stray space bar.
                assert_eq!(
                    selected_model_row(&format!("  {id} "), presets, false),
                    ModelRow::Preset(i)
                );
            }
            assert_eq!(
                selected_model_row("", presets, false),
                ModelRow::AccountDefault
            );
            assert_eq!(
                selected_model_row("   ", presets, false),
                ModelRow::AccountDefault
            );
            // an OPEN editor owns the selection even while still empty, or the
            // user types into a row nothing points at.
            assert_eq!(selected_model_row("", presets, true), ModelRow::Custom);
            assert_eq!(
                selected_model_row(presets[0], presets, true),
                ModelRow::Custom
            );
        }
    }

    /// #61: the field is free text, the column is a JSON array. An empty field
    /// must yield NO arguments — a one-element `[""]` reaches the CLI as a bare
    /// `""`, which codex reads as the prompt, so every session under an
    /// untouched profile would open with a blank turn.
    fn env_of(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn env_pairs_parse_and_quoted_values_keep_their_spaces() {
        assert_eq!(
            parse_profile_env("FOO=bar BAZ=qux"),
            env_of(&[("FOO", "bar"), ("BAZ", "qux")])
        );
        // the case a naive space-split gets wrong
        assert_eq!(
            parse_profile_env(r#"GREETING="hello world""#),
            env_of(&[("GREETING", "hello world")])
        );
        // a value may contain '=' — only the FIRST one separates
        assert_eq!(
            parse_profile_env("URL=https://x/y?a=b"),
            env_of(&[("URL", "https://x/y?a=b")])
        );
        // and an empty value is a legal, meaningful setting
        assert_eq!(parse_profile_env("QUIET="), env_of(&[("QUIET", "")]));
    }

    #[test]
    fn an_empty_env_field_exports_nothing() {
        for blank in ["", "   ", "\t"] {
            assert!(parse_profile_env(blank).is_empty(), "{blank:?}");
        }
    }

    #[test]
    fn tokens_that_could_never_reach_a_process_are_dropped() {
        // No '=' at all, an empty name, and names a process cannot receive.
        // Keeping these would produce a setting that silently never applies.
        assert!(parse_profile_env("JUSTAWORD").is_empty());
        assert!(parse_profile_env("=novalue").is_empty());
        assert!(parse_profile_env("2FOO=bar").is_empty(), "cannot start with a digit");
        assert!(parse_profile_env("has-dash=bar").is_empty(), "dashes are illegal");
        assert!(parse_profile_env(r#""has space=bar""#).is_empty());
        // ...while the legal ones alongside them still survive
        assert_eq!(
            parse_profile_env("JUSTAWORD OK_1=yes 2BAD=no"),
            env_of(&[("OK_1", "yes")])
        );
    }

    #[test]
    fn env_round_trips_so_opening_and_saving_changes_nothing() {
        for pairs in [
            vec![("FOO", "bar")],
            vec![("A", "1"), ("B", "2"), ("C", "3")],
            vec![("GREETING", "hello world")],
            vec![("EMPTY", "")],
            vec![("QUOTED", "it's")],
            vec![("BOTH", "it's \"quoted\"")],
        ] {
            let map = env_of(&pairs);
            assert_eq!(
                parse_profile_env(&fmt_profile_env(&map)),
                map,
                "round trip broke for {pairs:?}"
            );
        }
    }

    #[test]
    fn the_env_field_is_stable_across_renders() {
        // A HashMap iterates in a RANDOM order. Without sorting, the field would
        // reshuffle every time the editor opened and an untouched Save would look
        // like an edit.
        let map = env_of(&[("Z", "1"), ("A", "2"), ("M", "3")]);
        let first = fmt_profile_env(&map);
        for _ in 0..20 {
            assert_eq!(fmt_profile_env(&map), first);
        }
        assert!(first.starts_with("A="), "sorted by key: {first}");
    }

    #[test]
        fn an_empty_extra_args_field_is_no_arguments_at_all() {
        for blank in ["", "   ", "\t", "\n  \t "] {
            assert!(parse_extra_args(blank).is_empty(), "{blank:?}");
        }
        // ...but an explicitly quoted empty argument is one the user asked for.
        assert_eq!(parse_extra_args("--flag \"\""), vec!["--flag", ""]);
    }

    #[test]
    fn extra_args_split_on_whitespace_and_quotes_hold_an_argument_together() {
        assert_eq!(
            parse_extra_args("--dangerously-skip-permissions"),
            vec!["--dangerously-skip-permissions"]
        );
        assert_eq!(
            parse_extra_args("  --verbose   --debug  "),
            vec!["--verbose", "--debug"]
        );
        // the case the preview line exists for: quoted, so it stays ONE argument.
        assert_eq!(
            parse_extra_args("--append-system-prompt \"be terse\""),
            vec!["--append-system-prompt", "be terse"]
        );
        assert_eq!(
            parse_extra_args("--append-system-prompt 'be terse'"),
            vec!["--append-system-prompt", "be terse"]
        );
        // unquoted, it is TWO — which is exactly what the preview shows, so the
        // user finds out before Save rather than at the next failed spawn.
        assert_eq!(
            parse_extra_args("--append-system-prompt be terse"),
            vec!["--append-system-prompt", "be", "terse"]
        );
        // a quote can also open mid-argument (`--foo="a b"`), and an unclosed
        // one takes the rest of the line rather than dropping it.
        assert_eq!(parse_extra_args("--foo=\"a b\""), vec!["--foo=a b"]);
        assert_eq!(parse_extra_args("--foo \"unclosed"), vec!["--foo", "unclosed"]);
    }

    /// Opening a saved profile and pressing Save without touching anything must
    /// be a no-op. It isn't unless the field re-renders stored argv with its
    /// quoting intact — otherwise `["--append-system-prompt", "be terse"]` comes
    /// back as three arguments the second time around.
    #[test]
    fn stored_extra_args_round_trip_through_the_field() {
        let cases: Vec<Vec<String>> = vec![
            vec![],
            vec!["--verbose".into()],
            vec!["--append-system-prompt".into(), "be terse".into()],
            vec!["--flag".into(), "".into()],
            vec!["--msg".into(), "it's fine".into()],
            vec!["--msg".into(), "say \"hi\"".into()],
            vec!["--a".into(), "--b".into(), "c d e".into()],
            // BOTH quote kinds in one argument — the case that used to drop the
            // double quotes on reopen, so pressing Save rewrote the stored argv.
            vec![
                "--append-system-prompt".into(),
                "use \"strict\" mode; don't guess".into(),
            ],
            vec!["--msg".into(), "'\"".into()],
            vec!["--msg".into(), "\"'".into()],
            vec!["--msg".into(), "a\"b'c\"d".into()],
        ];
        for args in cases {
            assert_eq!(
                parse_extra_args(&fmt_extra_args(&args)),
                args,
                "round trip: {args:?}"
            );
        }
    }

    /// #62.4 — the ✕ on a profile card deletes the row and then runs the shared
    /// stale-default sweep. Both ends of this test go through
    /// `default_profile_key` ON PURPOSE: a sweep that went back to hardcoding
    /// its own key strings would still pass a test that hardcoded the same ones,
    /// which is exactly how the existing key-pinning test could stay green while
    /// the sweep quietly stopped clearing anything.
    #[test]
    fn the_delete_sweep_clears_the_very_key_the_settings_picker_reads() {
        let s = Store::open_in_memory().expect("in-memory store");
        for kind in [CliKind::Claude, CliKind::Codex] {
            let key = default_profile_key(kind).expect("claude/codex have a default key");
            let id = s
                .create_profile("work", kind.label(), None, None, &[], &HashMap::new(), None)
                .expect("create");
            s.set_setting(key, &id.to_string()).expect("set default");
            let _ = s.delete_profile(id);
            reconcile_default_profile_keys(&s, id, None);
            assert_eq!(
                s.get_setting(key).as_deref(),
                Some(""),
                "{key} still names a deleted profile"
            );
        }
        // a shell has no account concept, so there is no key to go stale.
        assert!(default_profile_key(CliKind::Shell).is_none());
    }

    /// #68: the Profiles section asks ONE question — "which account do new
    /// sessions use?" — as one card with a row group per CLI, where it used to
    /// ask it twice in two near-identical section cards. The groups are DATA now,
    /// so what can rot is that list drifting from the CLIs that actually STORE a
    /// default: a CLI with a key and no group is a setting with no UI, a group
    /// with no key is a picker whose writes go nowhere. Checked in both
    /// directions, through `default_profile_key` rather than its literal strings.
    #[test]
    fn every_cli_with_a_default_profile_key_gets_exactly_one_row_group() {
        for kind in [CliKind::Claude, CliKind::Codex, CliKind::Shell] {
            let groups = DEFAULT_PROFILE_CLIS
                .iter()
                .filter(|(k, _)| *k == kind)
                .count();
            assert_eq!(
                groups,
                usize::from(default_profile_key(kind).is_some()),
                "{} row group(s) for {}",
                groups,
                kind.label()
            );
        }
        // the group label is ALL that survives of the per-CLI distinction after
        // the collapse, so it must name its CLI and be unique — two unlabelled
        // (or identically labelled) pickers stacked in one card is the confusion
        // this task came to fix, one layer down.
        assert!(
            DEFAULT_PROFILE_CLIS
                .iter()
                .all(|(k, l)| l.starts_with(k.label())),
            "a row group's label must name its CLI"
        );
        let mut labels: Vec<&str> = DEFAULT_PROFILE_CLIS.iter().map(|(_, l)| *l).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), DEFAULT_PROFILE_CLIS.len(), "duplicate label");
    }

    /// gpui keys click targets by element id, and the three draft fields build
    /// theirs from `key()`. A duplicate would put two Edit buttons on one id and
    /// route both clicks to the same field.
    #[test]
    fn draft_slot_ids_and_labels_are_unique() {
        let slots = [
            DraftSlot::Label,
            DraftSlot::Model,
            DraftSlot::ExtraArgs,
            DraftSlot::Env,
        ];
        let mut keys: Vec<&str> = slots.iter().map(|s| s.key()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), slots.len(), "duplicate draft slot id");
        let mut labels: Vec<&str> = slots.iter().map(|s| s.slot_label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), slots.len(), "duplicate faux-input slot label");
        // an id may not carry a space — gpui element ids are used verbatim.
        assert!(slots.iter().all(|s| !s.key().contains(' ')));
    }
}
