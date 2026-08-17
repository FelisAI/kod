//! Focused Dark: the palette every surface paints with, plus the handful of
//! atoms built directly on it (a status dot, a project badge, a section header,
//! the accent button, the centered empty-state).
//!
//! Deliberately dependency-free apart from gpui — nothing here reads app state,
//! which is what lets ~15 render modules share it without a cycle. Anything that
//! needs `&Orchestrator` belongs in the render module that owns the surface.

use gpui::prelude::FluentBuilder;
use gpui::*;
use orchestrator_host::Phase;

// ---- Focused Dark tokens ----
pub(crate) const APP_BG: u32 = 0x0F1218;
pub(crate) const PANEL: u32 = 0x141820;
pub(crate) const CARD: u32 = 0x191D27;
pub(crate) const CARD2: u32 = 0x1D222C;
pub(crate) const HAIR: u32 = 0x2B303B;
pub(crate) const HAIR_SOFT: u32 = 0x23272F;
pub(crate) const TEXT: u32 = 0xD3D8E1;
pub(crate) const TEXT_STRONG: u32 = 0xF2F5FA;
pub(crate) const MUTED: u32 = 0x9AA3B1;
pub(crate) const MUTED2: u32 = 0x757E8A;
pub(crate) const ACCENT: u32 = 0x7EE2C0;
pub(crate) const AMBER: u32 = 0xE6C07A;
/// deep amber fill / hairline for the needs-you + summons affordances (matches
/// mapview's needs-you tag — the cockpit's "map wants you" vocabulary).
pub(crate) const AMBER_INK: u32 = 0x2A2417;
pub(crate) const AMBER_HAIR: u32 = 0x4A4636;
pub(crate) const GREEN: u32 = 0x5BB99B;
/// working/busy — deliberately warm-but-quiet: in the user's workflow a WORKING
/// agent needs nothing from him; GREEN means "idle, come drive me".
pub(crate) const ORANGE: u32 = 0xE08A4E;

pub(crate) fn dot(color: u32) -> impl IntoElement {
    div().w(px(7.)).h(px(7.)).rounded(px(4.)).bg(rgb(color))
}

/// (bg, border, text) triples for the project badge — theme-consistent hues,
/// picked by a stable hash of the slug so a project always looks the same.
const BADGE_HUES: [(u32, u32, u32); 7] = [
    (0x7EE2C022, 0x7EE2C055, 0x8FE8CA), // teal
    (0xE6C07A22, 0xE6C07A55, 0xECC98A), // amber
    (0x8AB4F822, 0x8AB4F855, 0xA9C3FB), // blue
    (0xC896E622, 0xC896E655, 0xD0A6EA), // purple
    (0xF0969620, 0xF0969650, 0xEDA3A3), // red
    (0x8FD2AE20, 0x8FD2AE50, 0xA3D9BD), // green
    (0x8FBEDC20, 0x8FBEDC50, 0xA6CBE4), // cyan
];
fn slug_hue(slug: &str) -> usize {
    let mut h: u32 = 2166136261;
    for b in slug.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    h as usize % BADGE_HUES.len()
}
/// A colored initial badge — the per-project visual anchor (kills the "texty"
/// rail). Initials come from the project NAME (so "beta" → "be", not the slug's
/// "gi"); the hue is a stable hash of the slug.
pub(crate) fn project_badge(name: &str, slug: &str, size: f32) -> impl IntoElement {
    let (bg, border, text) = BADGE_HUES[slug_hue(slug)];
    let initials: String = name
        .chars()
        .filter(|c| c.is_alphanumeric())
        .take(2)
        .collect::<String>()
        .to_lowercase();
    div()
        .w(px(size))
        .h(px(size))
        .flex_none()
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(bg))
        .border_1()
        .border_color(rgba(border))
        .text_size(px(size * 0.48))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(text))
        .child(SharedString::from(initials))
}
/// A rail section header ("● ACTIVE 2" / "PROJECTS") with a trailing hairline.
pub(crate) fn section_header(label: &str, count: Option<usize>, color: u32) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(7.))
        .px(px(8.))
        .pt(px(10.))
        .pb(px(4.))
        .child(
            div()
                .text_size(px(10.5))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(color))
                .child(SharedString::from(label.to_string())),
        )
        .when_some(count, |c, n| {
            c.child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgb(MUTED2))
                    .child(SharedString::from(n.to_string())),
            )
        })
        .child(div().flex_1().h(px(1.)).bg(rgb(HAIR_SOFT)))
}

pub(crate) fn accent_btn(label: &str) -> impl IntoElement {
    div()
        .px(px(10.))
        .py(px(4.))
        .rounded(px(8.))
        .bg(rgb(ACCENT))
        .text_size(px(12.))
        .text_color(rgb(0x0C140F))
        .child(SharedString::from(label.to_string()))
}

/// A centered notice for a stage's empty / loading / error states.
pub(crate) fn centered_notice(title: &str, sub: &str) -> impl IntoElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(13.))
                .text_color(rgb(MUTED))
                .child(SharedString::from(title.to_string())),
        )
        .child(
            div()
                .text_size(px(11.5))
                .text_color(rgb(MUTED2))
                .child(SharedString::from(sub.to_string())),
        )
}

/// A session's status as a short word / color (the Sessions list + stream, #9).
pub(crate) fn phase_word(p: Phase) -> &'static str {
    match p {
        Phase::Idle => "idle",
        Phase::Busy => "working",
        Phase::AwaitingDecision => "needs you",
        Phase::Spawning => "starting",
        Phase::Dead => "ended",
    }
}
pub(crate) fn phase_color(p: Phase) -> u32 {
    match p {
        Phase::AwaitingDecision => AMBER,
        Phase::Busy => ORANGE,
        Phase::Idle => GREEN, // idle = actionable ("come drive me"), so it's loud
        Phase::Dead => 0xE68A8A,
        _ => MUTED2,
    }
}
