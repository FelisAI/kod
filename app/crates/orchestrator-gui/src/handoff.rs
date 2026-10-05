//! "Continue in…" (docs/028 §6): move a session's work into a NEW session under
//! another account or CLI — codex2 out of credits → claude, or → another codex
//! login. The packet (`orchestrator_host::handoff`) carries the conversation word
//! for word, where it stopped and the repo state; this side picks the target,
//! stops the source, spawns the target with the packet as its first message, and
//! records the lineage.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::*;
use orchestrator_core::CliHome;
use orchestrator_host::codex_account::{parse_account_limits, read_account_limits, ReadError};
use orchestrator_host::handoff::{self as packet, PacketRequest};

use crate::spawn::{apply_profile, apply_profile_argv};
use crate::*;

/// The setting behind "Ask me before it continues" (docs/028 §5). Absent = on:
/// the receiver restating what it understood is the cheap moment to catch a
/// handoff that didn't take.
pub(crate) const CONFIRM_KEY: &str = "handoff_confirm";

/// One account a session can continue in.
#[derive(Clone)]
pub(crate) struct Target {
    pub kind: CliKind,
    /// `None` = the CLI's own login, no profile.
    pub profile: Option<ProfileRow>,
    pub label: String,
    /// The account the source already runs on (a fresh context, same quota).
    pub same_account: bool,
    /// codex's config root, where the account's limits are read.
    pub codex_home: Option<PathBuf>,
}

/// What a codex account can still do, read when the picker opens.
#[derive(Clone)]
pub(crate) enum Headroom {
    Checking,
    Known { hint: String, blocked: bool },
    Unknown(String),
}

pub(crate) struct Picker {
    pub source: SessionId,
    /// "codex · codex2" — the source account as the user names it.
    pub source_label: String,
    /// The source is mid-turn: handing off stops it.
    pub working: bool,
    pub targets: Vec<Target>,
    pub headroom: HashMap<PathBuf, Headroom>,
    /// A handoff is in flight; the rows are inert meanwhile.
    pub running: bool,
    pub error: Option<String>,
}

/// Where a session came from, for its subhead: "↩ from codex · codex2".
#[derive(Clone)]
pub(crate) struct Lineage {
    pub from: String,
    pub packet_dir: String,
}

/// "codex · codex2", or "claude · default login" for the CLI's own account.
fn account_label(kind: CliKind, profile: Option<&ProfileRow>) -> String {
    format!("{} · {}", kind.label(), account_name(profile))
}

/// The account alone, as the packet's prompt names it to the receiver.
fn account_name(profile: Option<&ProfileRow>) -> String {
    profile.map(|p| p.label.clone()).unwrap_or_else(|| "default login".to_string())
}

/// Packets live beside the store, in the app's own data dir — never inside the
/// repo: they hold transcript text, and a file in the tree gets committed sooner
/// or later. The same directory `boot::open_store` resolves.
fn handoffs_root() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join("Library/Application Support/orchestrator"))
        .unwrap_or_else(|_| std::env::temp_dir().join("orchestrator"))
        .join("handoffs")
}

/// Read a codex account's limits (~0.6 s, no model quota) and say it in a few
/// words. Blocking: run it off the UI thread.
fn headroom_of(home: &Path) -> Headroom {
    let value = match read_account_limits("codex", home, Duration::from_secs(10)) {
        Ok(v) => v,
        Err(ReadError::Unsupported(_)) => return Headroom::Unknown("this codex can't report its limits".into()),
        Err(ReadError::Failed(_)) => return Headroom::Unknown("couldn't read its limits".into()),
    };
    let Some(l) = parse_account_limits(&value, crate::timefmt::now_ms()) else {
        return Headroom::Unknown("no limit data".into());
    };
    let blocked = l.is_blocked(false);
    // The mapping the ⛔ BLOCKED tier renders, so the two never word a limit apart.
    let u = l.to_usage_limit(blocked, orchestrator_host::host::local_off_secs());
    let at = u.reset_label();
    let used = u.percent.unwrap_or(0);
    let credits = l.reached.as_deref().is_some_and(|r| r.contains("credits"));
    let hint = match (blocked, at.is_empty()) {
        // Out of credits mid-window has no reset: you top up, you don't wait.
        (true, true) if credits => "out of credits".to_string(),
        (true, true) => "limit reached".to_string(),
        (true, false) => format!("limit reached · resets {at}"),
        (false, _) if l.windows.is_empty() => "available".to_string(),
        (false, true) => format!("{used}% used"),
        (false, false) => format!("{used}% used · resets {at}"),
    };
    Headroom::Known { hint, blocked }
}

impl Orchestrator {
    /// Open the picker for a live claude/codex session.
    pub(crate) fn open_handoff(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let Some(info) = self.host.infos().into_iter().find(|i| i.id == id) else {
            return;
        };
        if !matches!(info.kind, CliKind::Claude | CliKind::Codex) {
            return;
        }
        let (profiles, src_profile) = match self.store.lock() {
            Ok(s) => (s.profiles(), info.cli_session_id.as_deref().and_then(|c| s.session_profile_id(c))),
            Err(_) => return,
        };
        let src_row = src_profile.and_then(|pid| profiles.iter().find(|p| p.id == pid));
        let mut targets = Vec::new();
        for kind in [CliKind::Claude, CliKind::Codex] {
            let rows = std::iter::once(None).chain(profiles.iter().filter(|p| p.cli_kind == kind.label()).map(Some));
            for p in rows {
                let codex_home = (kind == CliKind::Codex)
                    .then(|| match p {
                        Some(p) => CliHome::for_config_dir(kind, p.config_dir.as_deref()),
                        None => CliHome::ambient(kind),
                    })
                    .flatten()
                    .map(|h| h.root().to_path_buf());
                targets.push(Target {
                    kind,
                    profile: p.cloned(),
                    label: account_label(kind, p),
                    same_account: kind == info.kind && p.map(|p| p.id) == src_profile,
                    codex_home,
                });
            }
        }
        let homes: HashSet<PathBuf> = targets.iter().filter_map(|t| t.codex_home.clone()).collect();
        self.handoff = Some(Picker {
            source: id,
            source_label: account_label(info.kind, src_row),
            working: matches!(info.phase, Phase::Busy),
            targets,
            headroom: homes.iter().map(|h| (h.clone(), Headroom::Checking)).collect(),
            running: false,
            error: None,
        });
        // One account read per codex home, off the UI thread.
        for home in homes {
            cx.spawn(async move |this, cx| {
                let read = {
                    let home = home.clone();
                    cx.background_executor().spawn(async move { headroom_of(&home) }).await
                };
                let _ = this.update(cx, |o, cx| {
                    if let Some(p) = o.handoff.as_mut() {
                        p.headroom.insert(home, read);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    pub(crate) fn close_handoff(&mut self, cx: &mut Context<Self>) {
        if self.handoff.as_ref().is_some_and(|p| !p.running) {
            self.handoff = None;
            cx.notify();
        }
    }

    fn handoff_failed(&mut self, msg: String, cx: &mut Context<Self>) {
        eprintln!("[orchestrator] handoff failed: {msg}");
        if let Some(p) = self.handoff.as_mut() {
            p.running = false;
            p.error = Some(msg);
        }
        cx.notify();
    }

    /// Stop a session for good: kill its process group and mark its row closed.
    /// `close` also takes it out of the map auto-continue walks, so nothing can
    /// type into it again (docs/028 §2, one writer).
    fn stop_session(&mut self, id: SessionId, cli: &str) {
        self.host.close(id);
        if let Ok(s) = self.store.lock() {
            let _ = s.close_session(cli);
        }
    }

    /// Hand the picker's source to target `ti`: build the packet off the UI
    /// thread, then spawn, record and focus on it.
    pub(crate) fn run_handoff(&mut self, ti: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((src_id, target, running)) = self.handoff.as_ref().map(|p| (p.source, p.targets.get(ti).cloned(), p.running)) else {
            return;
        };
        let Some(target) = target.filter(|_| !running) else {
            return;
        };
        let Some(info) = self.host.infos().into_iter().find(|i| i.id == src_id) else {
            return self.handoff_failed("that session has already ended".into(), cx);
        };
        let Some(cli) = info.cli_session_id.clone() else {
            return self.handoff_failed("this session hasn't written a transcript yet, so there's nothing to hand off".into(), cx);
        };
        // Where it runs and on which account: its crash-recovery row.
        let (row, earlier) = match self.store.lock() {
            Ok(s) => (s.hosted_session_of(&cli), s.handoff_into(&cli).map(|h| PathBuf::from(h.packet_dir))),
            Err(_) => (None, None),
        };
        let Some((_, _, cwd, src_profile_id)) = row else {
            return self.handoff_failed("Kod has no record of where this session runs".into(), cx);
        };
        let cwd = PathBuf::from(cwd);
        let src_profile = src_profile_id.and_then(|pid| self.store.lock().ok()?.profile(pid));
        // The transcript lives under the source's OWN account home.
        let home = match &src_profile {
            Some(p) => CliHome::for_config_dir(info.kind, p.config_dir.as_deref()),
            None => CliHome::ambient(info.kind),
        };
        let req = PacketRequest {
            source_kind: info.kind,
            transcript: home.and_then(|h| h.transcript_path(&cli)),
            cwd: cwd.clone(),
            source_label: account_name(src_profile.as_ref()),
            target_kind: target.kind,
            confirm_first: self.handoff_confirm,
            earlier_packet: earlier,
        };
        let dir = handoffs_root().join(packet::packet_dir_name(info.kind, &cli, crate::timefmt::now_ms()));
        let reason = if info.usage_limit.as_ref().is_some_and(|u| u.hit) { "usage_limit" } else { "manual" };
        // A source still mid-turn stops FIRST, so the packet sees where it really
        // ended. An idle or blocked one keeps its process until the target is up,
        // so a failed spawn costs nothing.
        let stop_first = matches!(info.phase, Phase::Busy | Phase::AwaitingDecision);
        if stop_first {
            self.stop_session(src_id, &cli);
        }
        if let Some(p) = self.handoff.as_mut() {
            p.running = true;
            p.error = None;
        }
        cx.notify();
        let job = Job { src_id, cli, src_kind: info.kind, src_profile_id, slug: info.project_slug.clone(), cwd, reason, stopped: stop_first, target };
        cx.spawn_in(window, async move |this, cx| {
            let built = cx.background_executor().spawn(async move { packet::write_packet(&req, &dir) }).await;
            let _ = this.update_in(cx, |o, window, cx| o.finish_handoff(built, job, window, cx));
        })
        .detach();
    }

    fn finish_handoff(&mut self, built: std::io::Result<packet::Packet>, job: Job, window: &mut Window, cx: &mut Context<Self>) {
        let pkt = match built {
            Ok(p) => p,
            Err(e) => return self.handoff_failed(format!("couldn't build the handoff: {e}"), cx),
        };
        let t = &job.target;
        let pid = t.profile.as_ref().map(|p| p.id);
        let mut spec = self.stage_spec(SpawnSpec::program(t.kind.label(), &job.cwd));
        if let Some(p) = &t.profile {
            apply_profile(&mut spec, t.kind, p);
            apply_profile_argv(&mut spec, t.kind, p);
        }
        let cwd_str = job.cwd.to_string_lossy().into_owned();
        let packet_dir = pkt.dir.to_string_lossy().into_owned();
        let spawned = match t.kind {
            CliKind::Claude => {
                spec.initial_prompt = pkt.prompt.clone();
                // The packet sits outside the repo: name it a working dir so the
                // first read needs no permission prompt. LAST among the caller
                // args — `--add-dir` is variadic, and the host's own
                // `--add-dir <cwd>` follows it, so nothing bare trails it.
                spec.args.push("--add-dir".into());
                spec.args.push(packet_dir.clone());
                self.host.spawn_claude(job.slug.clone(), spec).map(|(id, c)| (id, Some(c), None))
            }
            _ => {
                // codex takes its first prompt as a trailing positional; `--`
                // keeps it a prompt whatever its first word looks like.
                spec.args.push("--".into());
                spec.args.push(pkt.prompt.clone());
                // Discovery scans the account this session runs under, read off
                // the spec exactly as a plain spawn does (spawn.rs).
                let codex_home = CliHome::from_env(CliKind::Codex, &spec.env);
                let pre: HashSet<String> = codex_home
                    .as_ref()
                    .map(|h| orchestrator_core::scan::codex_ids_for_cwd(&job.cwd, h))
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                self.host.spawn(job.slug.clone(), CliKind::Codex, spec).map(|id| (id, None, Some((pre, codex_home))))
            }
        };
        let (new_id, new_cli, discovery) = match spawned {
            Ok(x) => x,
            Err(e) => {
                let msg = format!("couldn't start {}: {e}. The handoff is saved at {packet_dir}", t.label);
                return self.handoff_failed(msg, cx);
            }
        };
        let handoff_id = self.store.lock().ok().and_then(|s| {
            if let Some(c) = &new_cli {
                let _ = s.record_session(c, &job.slug, t.kind.label(), &cwd_str, pid);
            }
            s.record_handoff(
                &job.cli,
                job.src_kind.label(),
                job.src_profile_id,
                new_cli.as_deref(),
                t.kind.label(),
                pid,
                &job.slug,
                &cwd_str,
                &packet_dir,
                job.reason,
            )
            .ok()
        });
        if let Some((pre, codex_home)) = discovery {
            self.record_codex_fresh(new_id, job.slug.clone(), job.cwd.clone(), pre, pid, codex_home, handoff_id);
        }
        if !job.stopped {
            self.stop_session(job.src_id, &job.cli);
        }
        if let Some(c) = &new_cli {
            let from = self.handoff.as_ref().map(|p| p.source_label.clone()).unwrap_or_default();
            self.handoff_lineage.insert(c.clone(), Lineage { from, packet_dir });
        }
        self.handoff = None;
        self.focus_session(&job.slug, new_id, window, cx);
    }

    pub(crate) fn set_handoff_confirm(&mut self, on: bool, cx: &mut Context<Self>) {
        self.handoff_confirm = on;
        if let Ok(s) = self.store.lock() {
            let _ = s.set_setting(CONFIRM_KEY, if on { "1" } else { "0" });
        }
        cx.notify();
    }

    /// The picker, mounted on the root so it works over any screen.
    pub(crate) fn handoff_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.handoff.as_ref()?;
        let mut card = div()
            .id("handoff-card")
            .w(px(400.))
            .p(px(14.))
            .rounded(px(12.))
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(HAIR))
            .flex()
            .flex_col()
            .gap(px(8.))
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| app.stop_propagation())
            .on_click(|_: &ClickEvent, _, app| app.stop_propagation())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(div().flex_1().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT_STRONG)).child("Continue in…"))
                    .child(
                        div()
                            .id("handoff-close")
                            .cursor_pointer()
                            .child(icon("icons/close.svg", 12., MUTED2))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close_handoff(cx))),
                    ),
            )
            .child(
                div().text_size(px(12.)).text_color(rgb(MUTED)).child(SharedString::from(format!(
                    "{} → a new session in the same folder. It starts from a handoff built from this session's transcript and the repo, and this session closes.",
                    p.source_label
                ))),
            )
            .when(p.working, |c| {
                c.child(div().text_size(px(12.)).text_color(rgb(AMBER)).child("This session is still working. Handing off stops it."))
            });
        for kind in [CliKind::Claude, CliKind::Codex] {
            card = card.child(
                div().pt(px(4.)).text_size(px(10.)).text_color(rgb(MUTED2)).child(SharedString::from(kind.label().to_uppercase())),
            );
            for (i, t) in p.targets.iter().enumerate().filter(|(_, t)| t.kind == kind) {
                let (hint, dim) = match t.codex_home.as_ref().and_then(|h| p.headroom.get(h)) {
                    Some(Headroom::Checking) => ("checking limits…".to_string(), false),
                    Some(Headroom::Known { hint, blocked }) => (hint.clone(), *blocked),
                    Some(Headroom::Unknown(why)) => (why.clone(), false),
                    None => ("limits unknown until used".to_string(), false),
                };
                let hint = if t.same_account { format!("this account · {hint}") } else { hint };
                let glyph = if kind == CliKind::Codex { "◆" } else { "✦" };
                card = card.child(
                    div()
                        .id(SharedString::from(format!("handoff-target-{i}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(9.))
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(px(7.))
                        .when(!p.running, |d| d.cursor_pointer().hover(|h| h.bg(rgb(CARD2))))
                        .child(div().w(px(14.)).text_color(rgb(if dim { MUTED2 } else { ACCENT })).child(glyph))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(13.))
                                .text_color(rgb(if dim { MUTED } else { TEXT }))
                                .child(SharedString::from(t.label.clone())),
                        )
                        .child(div().flex_none().text_size(px(11.)).text_color(rgb(if dim { 0xE0A0A0 } else { MUTED2 })).child(SharedString::from(hint)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.run_handoff(i, window, cx))),
                );
            }
        }
        let confirm = self.handoff_confirm;
        card = card.child(
            div()
                .id("handoff-confirm")
                .mt(px(4.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(rgb(MUTED))
                .child(
                    div()
                        .w(px(13.))
                        .h(px(13.))
                        .rounded(px(3.))
                        .border_1()
                        .border_color(rgb(if confirm { ACCENT } else { HAIR }))
                        .when(confirm, |d| d.bg(rgb(ACCENT_INK)).child(icon("icons/check.svg", 10., ACCENT))),
                )
                .child("Ask me before it continues — it restates the work first")
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_handoff_confirm(!confirm, cx))),
        );
        if p.running {
            card = card.child(div().text_size(px(12.)).text_color(rgb(ACCENT)).child("Building the handoff…"));
        }
        if let Some(e) = &p.error {
            card = card.child(div().text_size(px(12.)).text_color(rgb(0xE68A8A)).child(SharedString::from(e.clone())));
        }
        Some(
            div()
                .id("handoff-backdrop")
                .absolute()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x0000_0099))
                .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| app.stop_propagation())
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close_handoff(cx)))
                .child(card)
                .into_any_element(),
        )
    }

    /// Open a handed-off session's packet in the default app for Markdown.
    pub(crate) fn open_handoff_packet(&self, packet_dir: &str) {
        let _ = std::process::Command::new("open").arg(Path::new(packet_dir).join("handoff.md")).spawn();
    }
}

/// What `finish_handoff` needs once the packet is written.
struct Job {
    src_id: SessionId,
    cli: String,
    src_kind: CliKind,
    src_profile_id: Option<i64>,
    slug: String,
    cwd: PathBuf,
    reason: &'static str,
    /// The source was stopped before the packet was built.
    stopped: bool,
    target: Target,
}

impl Orchestrator {
    /// ORCH_DEMO=handoff / handoff-run (boot.rs): a fresh claude session in the
    /// first project with a folder, the picker open over it, and with `run` its
    /// first codex row picked. claude rather than codex as the source because
    /// Kod mints claude's id at spawn, and a handoff needs the source's id.
    /// `false` until there is a project with a folder to run it in.
    pub(crate) fn demo_handoff(&mut self, run: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(i) = self.projects.iter().position(|p| p.path.is_some()) else {
            return false;
        };
        self.selected = i;
        self.screen = Screen::Workspace;
        self.mode = Mode::Agent;
        let slug = self.projects[i].slug.clone();
        let cwd = self.projects[i].path.clone().unwrap_or_else(std::env::temp_dir);
        let spec = self.stage_spec(SpawnSpec::program("claude", &cwd));
        let (id, cli) = match self.host.spawn_claude(slug.clone(), spec) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("[orchestrator] demo handoff: couldn't start claude: {e}");
                return true;
            }
        };
        eprintln!("[orchestrator] demo handoff: source claude {cli} in {}", cwd.display());
        if let Ok(s) = self.store.lock() {
            let _ = s.record_session(&cli, &slug, "claude", &cwd.to_string_lossy(), None);
        }
        self.active_session.insert(slug, id);
        // Behind a daemon the session reaches the host's list a beat after spawn.
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..50 {
                Timer::after(Duration::from_millis(200)).await;
                let done = this
                    .update_in(cx, |o, window, cx| {
                        o.open_handoff(id, cx);
                        let Some(p) = o.handoff.as_ref() else {
                            return false;
                        };
                        eprintln!("[orchestrator] demo handoff: picker open, {} targets", p.targets.len());
                        if run {
                            if let Some(t) = p.targets.iter().position(|t| t.kind == CliKind::Codex) {
                                o.run_handoff(t, window, cx);
                            }
                        }
                        true
                    })
                    .unwrap_or(true);
                if done {
                    break;
                }
            }
        })
        .detach();
        true
    }
}
