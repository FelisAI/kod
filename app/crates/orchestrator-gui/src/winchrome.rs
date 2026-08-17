//! Window chrome that OUTLIVES the process: the rail's width and the OS window's
//! own geometry (#52).
//!
//! Grouped because they share one discipline — a drag emits a sample per frame,
//! so both persist on SETTLE rather than on change (one gesture = one store
//! write), and both validate on the way back in, since a corrupt or stale value
//! restores a rail you can't see or a window you can't click.

use gpui::*;

use crate::*;

/// A live sidebar resize: the x where the press landed and the rail width at
/// that moment. New width is `w0 + (x - grab_x)`, so the rail edge tracks the
/// cursor exactly no matter where inside the gutter the grab happened.
#[derive(Clone, Copy)]
pub(crate) struct SidebarDragAnchor {
    pub grab_x: f32,
    pub w0: f32,
}

/// Sidebar rail width bounds. `DEFAULT` is the width the rail was hard-coded to
/// before it became resizable; `MIN` keeps project names legible and `MAX` stops
/// the rail from crowding out the stage.
pub(crate) const SIDEBAR_W_DEFAULT: f32 = 214.0;
pub(crate) const SIDEBAR_W_MIN: f32 = 168.0;
pub(crate) const SIDEBAR_W_MAX: f32 = 420.0;

/// Clamp a candidate rail width into the supported range, rejecting NaN.
pub(crate) fn clamp_sidebar_w(w: f32) -> f32 {
    if w.is_finite() {
        w.clamp(SIDEBAR_W_MIN, SIDEBAR_W_MAX)
    } else {
        SIDEBAR_W_DEFAULT
    }
}

/// Smallest window geometry worth restoring (#52) — PER WINDOW, as `(w, h)`.
/// Anything smaller is treated as corrupt and ignored, rather than restoring a
/// window too small to use. The two floors differ because the windows do: each
/// pair is that window's own `window_min_size` (boot.rs opens both from these
/// consts), and validating Settings against the main window's 480 floor would
/// reject a legitimately 460-tall Settings window and silently re-center it.
///
/// Both sides measure the same thing — a CONTENT size. `window_min_size` is a
/// content minimum, and `windowed_only` persists the content size, so a window
/// the OS lets the user drag to is a window the restore path accepts. A floor
/// installed on only one of the two windows was the bug here: the main window
/// had none, so it could be dragged below 720x480 and then silently re-centered
/// on every launch after.
pub(crate) const MAIN_WIN_MIN: (f32, f32) = (720.0, 480.0);
pub(crate) const SETTINGS_WIN_MIN: (f32, f32) = (720.0, 460.0);

/// The store keys the two windows' geometry lives under (#52, #62). Named here,
/// next to the parse/format pair, so the restore site and the persist site can
/// never drift onto different keys.
pub(crate) const MAIN_WIN_BOUNDS_KEY: &str = "win_bounds";
pub(crate) const SETTINGS_WIN_BOUNDS_KEY: &str = "settings_win_bounds";

/// Serialize window geometry as `x,y,w,h` in logical px (#52).
pub(crate) fn fmt_win_bounds(b: Bounds<Pixels>) -> String {
    format!(
        "{:.0},{:.0},{:.0},{:.0}",
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        f32::from(b.size.width),
        f32::from(b.size.height)
    )
}

/// Parse persisted window geometry, rejecting anything unusable: a malformed or
/// non-finite field, a size below `min` (that window's own floor), or an origin
/// whose titlebar lands on no attached display. That last check is the one that
/// matters in practice — it's the "saved on a monitor that's since been
/// unplugged" case, which would otherwise reopen the window somewhere the user
/// can't click it.
pub(crate) fn parse_win_bounds(
    v: &str,
    displays: &[Bounds<Pixels>],
    min: (f32, f32),
) -> Option<Bounds<Pixels>> {
    // Split FIRST and demand exactly four fields, then parse each in place.
    // The earlier version filtered unparseable fields out before counting, so a
    // corrupt string with one junk field and one extra field ("x,100,80,1240,820")
    // still yielded four numbers — silently shifted one slot left, restoring the
    // window at the wrong origin AND the wrong size. Positional data must never
    // be compacted.
    let parts: Vec<&str> = v.split(',').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut f = [0f32; 4];
    for (slot, raw) in f.iter_mut().zip(parts.iter()) {
        match raw.trim().parse::<f32>() {
            Ok(n) if n.is_finite() => *slot = n,
            _ => return None,
        }
    }
    let (x, y, w, h) = (f[0], f[1], f[2], f[3]);
    if w < min.0 || h < min.1 {
        return None;
    }
    // Require the titlebar strip to overlap a live display, so the window can
    // always be grabbed and dragged even if the rest hangs off the edge.
    let (tx0, tx1, ty0, ty1) = (x, x + w, y, y + 32.0);
    let reachable = displays.iter().any(|d| {
        let dx0 = f32::from(d.origin.x);
        let dy0 = f32::from(d.origin.y);
        let dx1 = dx0 + f32::from(d.size.width);
        let dy1 = dy0 + f32::from(d.size.height);
        tx0 < dx1 && tx1 > dx0 && ty0 < dy1 && ty1 > dy0
    });
    if !reachable {
        return None;
    }
    Some(Bounds {
        origin: point(px(x), px(y)),
        size: gpui::size(px(w), px(h)),
    })
}

/// One geometry sample, taken from a live OS window. Three fields because the
/// platform reports them SEPARATELY and only together say what to reopen at —
/// see `windowed_only`, which is the only thing that reads them. Taken in one
/// place so no sample site can grab two of the three and silently lose the
/// third.
#[derive(Clone, Copy)]
pub(crate) struct WinSample {
    pub(crate) bounds: WindowBounds,
    pub(crate) content: Size<Pixels>,
    pub(crate) maximized: bool,
}

impl WinSample {
    pub(crate) fn of(window: &Window) -> Self {
        Self {
            bounds: window.window_bounds(),
            content: window.viewport_size(),
            maximized: window.is_maximized(),
        }
    }
}

/// The persisted geometry is the frame's ORIGIN paired with the CONTENT size,
/// and both exclusions below are load-bearing. All three facts were MEASURED
/// against gpui 0.2.2 on macOS, not inferred:
///
/// * `open_window` takes `window_bounds` as a CONTENT rect
///   (`initWithContentRect_`) while `window_bounds()` reads back the FRAME —
///   opening at 900×640 reports 900×672, the extra 32px being the titlebar.
///   Persisting that frame size and restoring it as a content size grew every
///   window by exactly one titlebar per launch, forever. Origin + `content` is
///   a fixed point: it reopens at the size it was saved at.
/// * A ZOOMED macOS window reports `Windowed(<visible frame>)`, never
///   `Maximized` — the mac backend has no `Maximized` arm at all; the state is
///   exposed only through `is_maximized()`. Matching on the variant alone let a
///   zoom overwrite the user's real windowed geometry, which un-zooming could
///   never bring back. The `Maximized` arm stays for the platforms that do emit
///   it, but `maximized` is what actually guards macOS.
/// * Fullscreen reports the restore bounds, which are not what the user sized.
fn windowed_only(s: Option<WinSample>) -> Option<String> {
    let s = s?;
    if s.maximized {
        return None;
    }
    match s.bounds {
        WindowBounds::Windowed(b) => Some(fmt_win_bounds(Bounds {
            origin: b.origin,
            size: s.content,
        })),
        _ => None,
    }
}

/// One window's geometry settle-detector (#52, #62). One per WINDOW, not one per
/// app: the main window and Settings are resized independently, so a single
/// shared pending/saved pair would let a sample from one cancel the other's, and
/// a geometry that never settles is a geometry that never persists.
pub(crate) struct WinBoundsWatch {
    /// the store key this window's geometry lives under.
    key: &'static str,
    /// the last sample, and the last one actually written. Persisting only when
    /// those two agree is what turns a whole drag-resize gesture into one store
    /// write instead of one per frame.
    pending: Option<String>,
    saved: Option<String>,
}

impl WinBoundsWatch {
    pub(crate) const fn new(key: &'static str) -> Self {
        Self {
            key,
            pending: None,
            saved: None,
        }
    }

    /// One poll sample. `Some((key, value))` means "write this now" — the
    /// geometry has held STILL for a tick and differs from what is on disk.
    /// `None` while it is still moving, already saved, or not windowed at all.
    pub(crate) fn settled(&mut self, s: Option<WinSample>) -> Option<(&'static str, String)> {
        let cur = windowed_only(s)?;
        if self.pending.as_deref() != Some(cur.as_str()) {
            // still in motion — remember it and wait for it to hold still.
            self.pending = Some(cur);
            return None;
        }
        self.take_if_new(cur)
    }

    /// The window is CLOSING, so there is no next tick to settle into: take the
    /// last geometry as-is. Without this, resizing a window and then closing it
    /// — the exact gesture of "I'm done, remember this size" — loses the resize.
    pub(crate) fn closing(&mut self, s: Option<WinSample>) -> Option<(&'static str, String)> {
        let cur = windowed_only(s)?;
        self.pending = Some(cur.clone());
        self.take_if_new(cur)
    }

    fn take_if_new(&mut self, cur: String) -> Option<(&'static str, String)> {
        if self.saved.as_deref() == Some(cur.as_str()) {
            return None; // settled, and already on disk
        }
        self.saved = Some(cur.clone());
        Some((self.key, cur))
    }
}

impl Orchestrator {
    /// Persist the rail width (#52). Called when a drag ENDS, not on every move,
    /// so one resize is one store write rather than one per frame.
    pub(crate) fn persist_sidebar_w(&self) {
        if let Ok(store) = self.store.lock() {
            let _ = store.set_setting("sidebar_w", &format!("{:.0}", self.sidebar_w));
        }
    }

    /// The one store write behind every geometry watcher. ORCH_DEMO PINS both
    /// windows to a fixed origin so a region screenshot can crop to them, and
    /// the restore path already ignores saved geometry under it — persisting the
    /// pin would let a screenshot run overwrite the layout the user arranged.
    fn persist_win_bounds(&self, write: Option<(&'static str, String)>) {
        let Some((key, v)) = write else { return };
        if std::env::var("ORCH_DEMO").is_ok() {
            return;
        }
        if let Ok(store) = self.store.lock() {
            let _ = store.set_setting(key, &v);
        }
    }

    /// Persist the MAIN window's geometry once it settles (#52). Sampled from
    /// the same 500ms poll that drives needs-you, so there is no second timer.
    pub(crate) fn tick_window_bounds(&mut self, s: Option<WinSample>) {
        let write = self.win_bounds.settled(s);
        self.persist_win_bounds(write);
    }

    /// The same for the SETTINGS window (#62), which used to reopen centered at
    /// 900×640 every time and drop any resize on the floor. Its handle only
    /// exists while that window is open, so `wb` is None the rest of the time
    /// and this is a no-op — the two windows never share a settle state.
    pub(crate) fn tick_settings_window_bounds(&mut self, s: Option<WinSample>) {
        let write = self.settings_win_bounds.settled(s);
        self.persist_win_bounds(write);
    }

    /// Settings is closing (Esc / ⌘W / ⌘, / the red button): persist now rather
    /// than waiting for a settle tick that will never come.
    pub(crate) fn persist_settings_window_bounds(&mut self, s: Option<WinSample>) {
        let write = self.settings_win_bounds.closing(s);
        self.persist_win_bounds(write);
    }
}

/// Geometry persistence (#52): rail width clamping + window-bounds round-trip.
/// Pure functions only — no store, no window, nothing to spawn.
#[cfg(test)]
mod geometry_tests {
    // NOTE: no `use super::*` — gpui is glob-imported at the top of this file.
    use super::{
        clamp_sidebar_w, fmt_win_bounds, parse_win_bounds, WinBoundsWatch, WinSample,
        MAIN_WIN_BOUNDS_KEY, MAIN_WIN_MIN, SETTINGS_WIN_BOUNDS_KEY, SETTINGS_WIN_MIN,
        SIDEBAR_W_DEFAULT, SIDEBAR_W_MAX, SIDEBAR_W_MIN,
    };
    use gpui::{point, px, Bounds, Pixels, WindowBounds};

    /// macOS titlebar height for the style both windows open with (measured:
    /// a window opened at content 900×640 reports frame 900×672).
    const TITLEBAR: f32 = 32.0;

    fn display(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: gpui::size(px(w), px(h)),
        }
    }

    /// One 1920x1080 display at the origin.
    fn one_screen() -> Vec<Bounds<Pixels>> {
        vec![display(0.0, 0.0, 1920.0, 1080.0)]
    }

    #[test]
    fn sidebar_width_clamps_into_range() {
        assert_eq!(clamp_sidebar_w(10.0), SIDEBAR_W_MIN);
        assert_eq!(clamp_sidebar_w(9999.0), SIDEBAR_W_MAX);
        assert_eq!(clamp_sidebar_w(260.0), 260.0);
    }

    #[test]
    fn sidebar_width_rejects_non_finite() {
        // A corrupt stored value must not produce a NaN width, which would
        // silently collapse the rail (and the PTY column count with it).
        // Every non-finite input falls back to the default rather than to a
        // bound, so "inf" in the store reopens at a normal rail width.
        assert_eq!(clamp_sidebar_w(f32::NAN), SIDEBAR_W_DEFAULT);
        assert_eq!(clamp_sidebar_w(f32::INFINITY), SIDEBAR_W_DEFAULT);
        assert_eq!(clamp_sidebar_w(f32::NEG_INFINITY), SIDEBAR_W_DEFAULT);
    }

    #[test]
    fn window_bounds_round_trip() {
        let b = display(100.0, 80.0, 1240.0, 820.0);
        let parsed = parse_win_bounds(&fmt_win_bounds(b), &one_screen(), MAIN_WIN_MIN).expect("round-trips");
        assert_eq!(f32::from(parsed.origin.x), 100.0);
        assert_eq!(f32::from(parsed.origin.y), 80.0);
        assert_eq!(f32::from(parsed.size.width), 1240.0);
        assert_eq!(f32::from(parsed.size.height), 820.0);
    }

    #[test]
    fn window_bounds_rejects_malformed_or_tiny() {
        let s = one_screen();
        assert!(parse_win_bounds("", &s, MAIN_WIN_MIN).is_none());
        assert!(parse_win_bounds("1,2,3", &s, MAIN_WIN_MIN).is_none(), "too few fields");
        assert!(parse_win_bounds("a,b,c,d", &s, MAIN_WIN_MIN).is_none(), "non-numeric");
        assert!(parse_win_bounds("0,0,NaN,600", &s, MAIN_WIN_MIN).is_none(), "non-finite");
        assert!(
            parse_win_bounds("0,0,200,150", &s, MAIN_WIN_MIN).is_none(),
            "below the minimum usable size"
        );
    }

    #[test]
    fn a_junk_field_never_shifts_the_remaining_values_into_the_wrong_slots() {
        // Regression: parsing used to DROP unparseable fields and then count what
        // survived, so this five-field string with junk in slot 0 collapsed to a
        // valid-looking [100, 80, 1240, 820] and restored the window at the wrong
        // origin AND the wrong size. Positional data must be rejected, not compacted.
        let s = one_screen();
        assert!(parse_win_bounds("x,100,80,1240,820", &s, MAIN_WIN_MIN).is_none());
        assert!(parse_win_bounds("100,x,80,1240,820", &s, MAIN_WIN_MIN).is_none());
        // ...and a five-field string that is entirely numeric is still wrong.
        assert!(parse_win_bounds("0,100,80,1240,820", &s, MAIN_WIN_MIN).is_none(), "too many fields");
        // an empty slot is junk too, not a zero.
        assert!(parse_win_bounds("100,,1240,820", &s, MAIN_WIN_MIN).is_none());
    }

    #[test]
    fn window_bounds_rejects_geometry_on_a_detached_display() {
        // Saved while a second monitor was attached at x=1920; that monitor is
        // now gone, so restoring would put the window where it can't be clicked.
        let saved = fmt_win_bounds(display(2400.0, 300.0, 1240.0, 820.0));
        assert!(parse_win_bounds(&saved, &one_screen(), MAIN_WIN_MIN).is_none());
        // ...and it comes back once that display is present again.
        let both = vec![display(0.0, 0.0, 1920.0, 1080.0), display(1920.0, 0.0, 1920.0, 1080.0)];
        assert!(parse_win_bounds(&saved, &both, MAIN_WIN_MIN).is_some());
    }

    #[test]
    fn window_partly_offscreen_is_kept_while_its_titlebar_is_reachable() {
        // Hanging off the right edge is fine — the titlebar still overlaps, so
        // the window can be grabbed and dragged back.
        let saved = fmt_win_bounds(display(1700.0, 40.0, 1240.0, 820.0));
        assert!(parse_win_bounds(&saved, &one_screen(), MAIN_WIN_MIN).is_some());
    }

    /// Settings' `window_min_size` is 460 tall — SHORTER than the main window's
    /// 480 floor. Validating it against the main window's minimum would reject a
    /// window the user is allowed to make, and silently re-center Settings every
    /// time (which is the whole bug #62.1 exists to fix).
    #[test]
    fn each_window_is_validated_against_its_own_minimum() {
        let s = one_screen();
        let shortest_settings = fmt_win_bounds(display(
            80.0,
            60.0,
            SETTINGS_WIN_MIN.0,
            SETTINGS_WIN_MIN.1,
        ));
        assert!(parse_win_bounds(&shortest_settings, &s, SETTINGS_WIN_MIN).is_some());
        assert!(
            parse_win_bounds(&shortest_settings, &s, MAIN_WIN_MIN).is_none(),
            "the main window's floor is the taller one"
        );
        // ...and the two windows never write over each other's key.
        assert_ne!(MAIN_WIN_BOUNDS_KEY, SETTINGS_WIN_BOUNDS_KEY);
    }

    /// A sample shaped like the real platform's: `w`×`h` is the CONTENT size the
    /// window was opened at, and the frame the OS reports back is one titlebar
    /// taller. Anything that persists the frame size instead of the content size
    /// fails the tests below by exactly TITLEBAR.
    fn windowed(x: f32, y: f32, w: f32, h: f32) -> Option<WinSample> {
        Some(WinSample {
            bounds: WindowBounds::Windowed(display(x, y, w, h + TITLEBAR)),
            content: gpui::size(px(w), px(h)),
            maximized: false,
        })
    }

    #[test]
    fn a_drag_resize_persists_once_it_holds_still() {
        // Every frame of a live drag is a different sample; only the one that
        // repeats is written, so one gesture is one store write.
        let mut w = WinBoundsWatch::new(MAIN_WIN_BOUNDS_KEY);
        assert_eq!(w.settled(windowed(0.0, 0.0, 1240.0, 820.0)), None);
        assert_eq!(w.settled(windowed(0.0, 0.0, 1250.0, 820.0)), None);
        assert_eq!(w.settled(windowed(0.0, 0.0, 1260.0, 820.0)), None);
        assert_eq!(
            w.settled(windowed(0.0, 0.0, 1260.0, 820.0)),
            Some((MAIN_WIN_BOUNDS_KEY, "0,0,1260,820".to_string()))
        );
        // ...and a geometry already on disk is never rewritten, tick after tick.
        assert_eq!(w.settled(windowed(0.0, 0.0, 1260.0, 820.0)), None);
        assert_eq!(w.settled(windowed(0.0, 0.0, 1260.0, 820.0)), None);
    }

    #[test]
    fn maximized_and_fullscreen_are_never_saved_over_the_windowed_geometry() {
        // window_bounds() reports the SCREEN rect in those states — saving it
        // would clobber the restore size and the window would never come back.
        //
        // The ZOOMED sample is the one that matters here, and it is shaped the
        // way macOS actually reports it (measured, gpui 0.2.2): the variant is
        // `Windowed`, NOT `Maximized` — the mac backend never constructs
        // `Maximized` — and `is_maximized()` is the only thing that says so. A
        // guard that matched only on the variant let one double-click on the
        // titlebar overwrite the user's windowed geometry with the screen rect.
        let mut w = WinBoundsWatch::new(MAIN_WIN_BOUNDS_KEY);
        let screen = display(0.0, 30.0, 1920.0, 1050.0);
        let zoomed = Some(WinSample {
            bounds: WindowBounds::Windowed(screen),
            content: gpui::size(px(1920.0), px(1018.0)),
            maximized: true,
        });
        let other_platform_maximized = Some(WinSample {
            bounds: WindowBounds::Maximized(screen),
            content: gpui::size(px(1920.0), px(1018.0)),
            maximized: false,
        });
        let fullscreen = Some(WinSample {
            bounds: WindowBounds::Fullscreen(screen),
            content: gpui::size(px(1920.0), px(1080.0)),
            maximized: false,
        });
        for _ in 0..3 {
            assert_eq!(w.settled(zoomed), None, "a zoomed macOS window");
            assert_eq!(w.settled(other_platform_maximized), None);
            assert_eq!(w.settled(fullscreen), None);
            assert_eq!(w.settled(None), None);
        }
    }

    #[test]
    fn what_is_persisted_is_the_content_size_the_window_reopens_at() {
        // MEASURED (gpui 0.2.2, macOS): `open_window` takes window_bounds as a
        // CONTENT rect while `window_bounds()` reads back the FRAME, one 32px
        // titlebar taller. Persisting the frame and restoring it as content grew
        // the window by a titlebar on EVERY launch — 640 → 672 → 704 → …
        let mut w = WinBoundsWatch::new(SETTINGS_WIN_BOUNDS_KEY);
        w.settled(windowed(60.0, 60.0, 900.0, 640.0));
        assert_eq!(
            w.settled(windowed(60.0, 60.0, 900.0, 640.0)),
            Some((SETTINGS_WIN_BOUNDS_KEY, "60,60,900,640".to_string())),
            "the frame is 900x672; what reopens at 900x640 is the content size"
        );
        // ...and feeding that value back in is a fixed point: reopening at the
        // saved content size settles on the SAME string, so nothing is written
        // again and nothing drifts.
        let mut next = WinBoundsWatch::new(SETTINGS_WIN_BOUNDS_KEY);
        next.settled(windowed(60.0, 60.0, 900.0, 640.0));
        assert_eq!(
            next.settled(windowed(60.0, 60.0, 900.0, 640.0)),
            Some((SETTINGS_WIN_BOUNDS_KEY, "60,60,900,640".to_string()))
        );
    }

    #[test]
    fn closing_persists_a_resize_that_never_got_a_settle_tick() {
        // Resize, then close before the next poll: the whole point of resizing a
        // window you are done with, and the settle rule alone would drop it.
        let mut w = WinBoundsWatch::new(SETTINGS_WIN_BOUNDS_KEY);
        assert_eq!(w.settled(windowed(60.0, 60.0, 900.0, 640.0)), None);
        assert_eq!(
            w.closing(windowed(60.0, 60.0, 1000.0, 700.0)),
            Some((SETTINGS_WIN_BOUNDS_KEY, "60,60,1000,700".to_string()))
        );
        // a second close (reopen → close with nothing moved) writes nothing.
        assert_eq!(w.closing(windowed(60.0, 60.0, 1000.0, 700.0)), None);
    }

    #[test]
    fn the_two_windows_settle_independently() {
        // One shared pending/saved pair would let a Settings sample cancel the
        // main window's pending one, so neither geometry would ever settle.
        let mut main = WinBoundsWatch::new(MAIN_WIN_BOUNDS_KEY);
        let mut settings = WinBoundsWatch::new(SETTINGS_WIN_BOUNDS_KEY);
        assert_eq!(main.settled(windowed(0.0, 0.0, 1240.0, 820.0)), None);
        assert_eq!(settings.settled(windowed(60.0, 60.0, 900.0, 640.0)), None);
        assert_eq!(
            main.settled(windowed(0.0, 0.0, 1240.0, 820.0)),
            Some((MAIN_WIN_BOUNDS_KEY, "0,0,1240,820".to_string()))
        );
        assert_eq!(
            settings.settled(windowed(60.0, 60.0, 900.0, 640.0)),
            Some((SETTINGS_WIN_BOUNDS_KEY, "60,60,900,640".to_string()))
        );
    }

    #[test]
    fn what_settles_is_exactly_what_restores() {
        // The persist side and the restore side are one round trip: a geometry
        // the watcher emits must survive parse_win_bounds, or Settings saves a
        // size it then refuses to reopen at.
        let mut w = WinBoundsWatch::new(SETTINGS_WIN_BOUNDS_KEY);
        w.settled(windowed(120.0, 90.0, 980.0, 700.0));
        let (_, v) = w
            .settled(windowed(120.0, 90.0, 980.0, 700.0))
            .expect("settles on the second identical sample");
        let back = parse_win_bounds(&v, &one_screen(), SETTINGS_WIN_MIN).expect("round-trips");
        assert_eq!(f32::from(back.size.width), 980.0);
        assert_eq!(f32::from(back.size.height), 700.0);
        // ...and the floor both sides use is the same KIND of size: the value
        // that survives parse is what `window_bounds` reopens the window at,
        // which is a content rect, and `window_min_size` is a content minimum.
        let (_, at_floor) = WinBoundsWatch::new(SETTINGS_WIN_BOUNDS_KEY)
            .closing(windowed(0.0, 0.0, SETTINGS_WIN_MIN.0, SETTINGS_WIN_MIN.1))
            .expect("closing takes the sample as-is");
        assert!(
            parse_win_bounds(&at_floor, &one_screen(), SETTINGS_WIN_MIN).is_some(),
            "a window dragged to exactly its own minimum must reopen there"
        );
    }
}
