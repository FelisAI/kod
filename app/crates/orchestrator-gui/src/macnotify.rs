//! Native macOS notifications via `UNUserNotificationCenter` — the proper,
//! **"Kod"-attributed** system notification (correct app name + icon), used when
//! Kod runs from its signed `.app` bundle.
//!
//! WHY THE BUNDLE GATE IS LOAD-BEARING: `currentNotificationCenter()`
//! **hard-crashes** (`NSInternalInconsistencyException`, "bundleProxyForCurrent
//! process is nil") when there is no app bundle — i.e. a bare `cargo run`
//! (`target/debug/orchestrator`). It's not a catchable error; it kills the
//! process. So [`post_needs_you`] first checks we're inside a `*.app` bundle and
//! returns `false` (posting nothing) otherwise, and the caller falls back to the
//! `osascript` path. An UNSIGNED bundle doesn't crash — it just won't be granted
//! authorization, so nothing shows; that degrades quietly, no fallback needed.
//!
//! Authorization is requested lazily on first use. Every call here MUST be on the
//! MAIN THREAD — the sole caller (`summaries.rs`) runs in the GPUI update loop,
//! which is the main thread.
//!
//! This module also owns the app's other, QUIET channel — the Dock-tile badge
//! ([`set_dock_badge`]) — because it is the same AppKit/objc2 surface under the
//! same main-thread rule, and because the bundle gate above is exactly the
//! reasoning a reader needs to have in front of them to see why the badge does
//! *not* take one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

/// True iff the running executable lives inside a `*.app/Contents/MacOS/` bundle.
/// Cheap (no ObjC), and the gate that keeps the UN path from crashing a bare
/// `cargo run` binary. `Kod.app` (see `scripts/make-app.sh`) satisfies it.
pub fn in_app_bundle() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.contains(".app/Contents/MacOS/")))
        .unwrap_or(false)
}

static AUTH_ONCE: Once = Once::new();

/// Post a "needs you" notification via `UNUserNotificationCenter`. Returns `true`
/// if it went through the native path, `false` when not in a bundle (⇒ caller
/// uses the `osascript` fallback). `session` becomes the (bold) title, the ask
/// the body; macOS shows **"Kod"** as the source automatically — which is the
/// whole point vs. osascript's "Script Editor".
pub fn post_needs_you(session: &str, ask: &str) -> bool {
    if !in_app_bundle() {
        return false;
    }

    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNNotificationSound, UNUserNotificationCenter,
    };

    // NO `unsafe` BLOCK, deliberately: objc2 0.6 (pinned in Cargo.lock) exposes
    // every UN method used below as a SAFE binding, so wrapping this in `unsafe`
    // grants nothing and only earns an `unused_unsafe` warning on every build.
    // The preconditions are still real and still ours to uphold — they are just
    // not the compiler's business here: this must run on the MAIN THREAD, and the
    // `in_app_bundle()` gate above is what keeps `currentNotificationCenter()`
    // off the nil-bundle crash. If a future objc2 marks any of these `unsafe`
    // again, it returns as a hard compile error — exactly the signal we want.
    let center = UNUserNotificationCenter::currentNotificationCenter();

    // Request authorization exactly once. The completion handler ESCAPES (it
    // fires asynchronously on a background queue after this returns), so it
    // must be a heap RcBlock, not a stack block.
    AUTH_ONCE.call_once(|| {
        let handler = RcBlock::new(|_granted: Bool, _err: *mut NSError| {});
        let opts = UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound;
        center.requestAuthorizationWithOptions_completionHandler(opts, &handler);
    });

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(session));
    content.setSubtitle(&NSString::from_str("needs you"));
    let body: String = ask.chars().take(160).collect();
    content.setBody(&NSString::from_str(&body));
    content.setSound(Some(&UNNotificationSound::defaultSound()));

    // Unique id per post — reusing an identifier REPLACES the prior banner.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let id = NSString::from_str(&format!("kod-needs-you-{}", SEQ.fetch_add(1, Ordering::Relaxed)));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
    // A completion handler, NOT None. Passing None discards the delivery error,
    // which turns every failure into "nothing happened" — indistinguishable from
    // a banner the user simply missed, and the exact reason the first real test of
    // this path produced no evidence to work from. The handler escapes (it fires
    // on a background queue), so it must be a heap RcBlock.
    let done = RcBlock::new(|err: *mut NSError| {
        if !err.is_null() {
            // Safety: non-null NSError owned by the callback for its duration.
            let msg = unsafe { (*err).localizedDescription() };
            eprintln!("[kod] notification NOT delivered: {msg}");
        }
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&done));
    true
}

/// Whether the user has actually GRANTED notification authorization, logged once
/// at boot. `post_needs_you` cannot report this: authorization is asynchronous, so
/// the first post races the permission prompt and a `true` return only means "we
/// handed it to macOS", never "a banner appeared". Without this there is no way to
/// tell a denied app from a delivered-but-unnoticed one.
pub fn log_authorization_state() {
    if !in_app_bundle() {
        eprintln!("[kod] notifications: not in an .app bundle — osascript fallback in use");
        return;
    }
    use block2::RcBlock;
    use objc2_user_notifications::{UNAuthorizationStatus, UNUserNotificationCenter};
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let handler = RcBlock::new(|settings: core::ptr::NonNull<objc2_user_notifications::UNNotificationSettings>| {
        let status = unsafe { settings.as_ref().authorizationStatus() };
        let name = match status {
            UNAuthorizationStatus::NotDetermined => "NotDetermined (prompt not answered yet)",
            UNAuthorizationStatus::Denied => "DENIED — banners will never appear",
            UNAuthorizationStatus::Authorized => "Authorized",
            UNAuthorizationStatus::Provisional => "Provisional (quiet delivery only)",
            _ => "unknown",
        };
        eprintln!("[kod] notification authorization: {name}");
    });
    center.getNotificationSettingsWithCompletionHandler(&handler);
}

/// What the Dock tile must be told, if anything. The decision is a pure value so
/// that the *no-op* case — the whole reason this type exists — is provable in a
/// test instead of asserted in prose.
#[derive(Debug, PartialEq, Eq)]
pub enum BadgeUpdate {
    /// The count did not move: do not touch AppKit at all.
    Unchanged,
    /// Nothing is waiting ⇒ `setBadgeLabel(None)`. Never a "0" bubble — a badge
    /// reading zero is a badge asking to be dismissed, the exact opposite of an
    /// ambient signal.
    Clear,
    Set(String),
}

/// The `last` sentinel meaning "this process has never published a badge". It
/// forces the FIRST count through even when it is `0`, so the tile's state is
/// known rather than assumed-empty.
pub const BADGE_UNSET: u64 = u64::MAX;

/// The badge state machine: what (if anything) `n` requires, given the count
/// already on the tile.
pub fn badge_update(last: u64, n: usize) -> BadgeUpdate {
    if last == n as u64 {
        return BadgeUpdate::Unchanged;
    }
    if n == 0 {
        BadgeUpdate::Clear
    } else {
        BadgeUpdate::Set(n.to_string())
    }
}

/// The count currently ON the tile — see `set_dock_badge` for why this ledger is
/// the load-bearing part of the feature.
static LAST_BADGE: AtomicU64 = AtomicU64::new(BADGE_UNSET);

/// Publish `n` through `send`, maintaining the `last` ledger. Split out of
/// [`set_dock_badge`] so the ledger's two load-bearing rules are executed by
/// TESTS rather than only asserted in prose (they are what the whole feature
/// rests on, and an AppKit call cannot be made in a unit test):
///   1. record `n` only AFTER the label actually reached the tile, and
///   2. record NOTHING when the update was dropped — otherwise the ledger
///      claims a badge that was never drawn and every later tick at that same
///      count is skipped as `Unchanged`, freezing the tile on a stale number.
/// `send` reports whether the label truly landed; it is the ONLY thing allowed
/// to advance the ledger.
fn publish_badge(last: &AtomicU64, n: usize, send: impl FnOnce(Option<&str>) -> bool) {
    let label = match badge_update(last.load(Ordering::Relaxed), n) {
        BadgeUpdate::Unchanged => return,
        BadgeUpdate::Clear => None,
        BadgeUpdate::Set(s) => Some(s),
    };
    if send(label.as_deref()) {
        last.store(n as u64, Ordering::Relaxed);
    }
}

/// Show `n` on the Dock tile; `n == 0` clears it. The AMBIENT channel: it never
/// steals focus, never makes a sound, and never breaks a Focus mode (a focus mode
/// is a focus mode) — it is simply there whenever you glance at the Dock.
///
/// MAIN THREAD ONLY. `tick_needs` — the sole caller — runs in the GPUI update
/// loop, which is the main thread. That is not advisory: `sharedApplication`
/// demands a `MainThreadMarker`, which a worker thread cannot obtain, so a call
/// from one is DROPPED (below) and the badge silently stops tracking. Don't.
///
/// UNBUNDLED IS SAFE — and unlike [`post_needs_you`] this deliberately takes NO
/// [`in_app_bundle`] gate, which would otherwise blind the badge on the normal
/// `cargo run` dev path. MEASURED, not assumed: a bare unbundled binary running
/// this exact sequence (`sharedApplication` → `dockTile` → `setBadgeLabel`
/// Some/None) completed every call, read the label back unchanged, and rendered
/// the red bubble on its Dock tile. It cannot share the UN hazard by
/// construction: that crash is an IDENTITY failure — `currentNotificationCenter()`
/// has to resolve a bundle record to route a notification to a *registered* app —
/// while an `NSDockTile` is vended by the running `NSApplication`, which every
/// AppKit process has whether or not anything on disk describes it.
pub fn set_dock_badge(n: usize) {
    // THE POINT OF THE LEDGER: tick_needs fires every ~500ms and the count is
    // almost always the same one as last time. Re-setting an identical label
    // 120×/minute is pointless main-thread work that makes the Dock redraw for
    // no change, so an unchanged count must never reach AppKit at all.
    publish_badge(&LAST_BADGE, n, |label| {
        let Some(mtm) = objc2::MainThreadMarker::new() else {
            // Off the main thread. Drop the update and report FALSE, which is
            // what keeps `n` out of the ledger: the next on-main-thread tick
            // then still sees a change and repairs the badge instead of
            // inheriting a lie about what the tile shows.
            return false;
        };
        use objc2_app_kit::NSApplication;
        use objc2_foundation::NSString;
        // NO `unsafe`, for the same reason as post_needs_you: objc2 0.6 exposes
        // both of these as SAFE bindings — the main-thread precondition rides in
        // `mtm` rather than in an unsafe block — so wrapping them earns nothing
        // but an `unused_unsafe` warning. If a future objc2 marks either unsafe
        // again, it comes back as a hard compile error, the signal we want.
        let tile = NSApplication::sharedApplication(mtm).dockTile();
        let ns = label.map(NSString::from_str);
        tile.setBadgeLabel(ns.as_deref());
        true
    });
}

#[cfg(test)]
mod dock_badge_tests {
    use super::{badge_update, publish_badge, BadgeUpdate, BADGE_UNSET};
    use std::sync::atomic::AtomicU64;

    /// Replay tick counts through the SHIPPED publish path — the real ledger,
    /// the real early-out — with AppKit replaced by a recorder. Nothing here
    /// re-implements the ledger: `publish_badge` owns it, which is the point,
    /// so a future edit that advances the ledger somewhere other than "after
    /// the label landed" fails these tests instead of shipping a frozen badge.
    /// `delivers` decides, per call, whether the label reached the tile (false
    /// = the off-main-thread drop).
    fn replay(counts: &[usize], delivers: impl Fn(usize) -> bool) -> Vec<BadgeUpdate> {
        let last = AtomicU64::new(BADGE_UNSET);
        let mut calls = Vec::new();
        for (i, &n) in counts.iter().enumerate() {
            publish_badge(&last, n, |label| {
                calls.push(match label {
                    None => BadgeUpdate::Clear,
                    Some(s) => BadgeUpdate::Set(s.to_string()),
                });
                delivers(i)
            });
        }
        calls
    }

    /// The everything-lands case: what actually reached AppKit.
    fn appkit_calls(counts: &[usize]) -> Vec<BadgeUpdate> {
        replay(counts, |_| true)
    }

    #[test]
    fn zero_clears_and_never_draws_a_zero() {
        assert_eq!(badge_update(3, 0), BadgeUpdate::Clear);
        // the first publish of a 0 is still a real call: the sentinel means we
        // have never told the tile anything, so "empty" is an assumption.
        assert_eq!(appkit_calls(&[0]), vec![BadgeUpdate::Clear]);
        assert_eq!(
            appkit_calls(&[4, 0]),
            vec![BadgeUpdate::Set("4".into()), BadgeUpdate::Clear]
        );
    }

    #[test]
    fn a_waiting_count_renders_as_its_number() {
        assert_eq!(badge_update(0, 1), BadgeUpdate::Set("1".into()));
        assert_eq!(badge_update(BADGE_UNSET, 12), BadgeUpdate::Set("12".into()));
    }

    /// The perf requirement, stated as a test: two full minutes of ticks with a
    /// steady count must cost exactly ONE AppKit call, not 240.
    #[test]
    fn an_unchanged_count_is_a_no_op() {
        assert_eq!(badge_update(2, 2), BadgeUpdate::Unchanged);
        let steady = vec![2usize; 240];
        assert_eq!(appkit_calls(&steady), vec![BadgeUpdate::Set("2".into())]);
        // and a steady ZERO — the common case, an idle machine — settles to one
        // clear and then costs nothing forever.
        assert_eq!(appkit_calls(&[0; 240]), vec![BadgeUpdate::Clear]);
    }

    #[test]
    fn a_changed_count_updates() {
        assert_eq!(badge_update(1, 2), BadgeUpdate::Set("2".into()));
        assert_eq!(
            appkit_calls(&[0, 1, 1, 1, 2, 2, 0, 0, 3]),
            vec![
                BadgeUpdate::Clear,
                BadgeUpdate::Set("1".into()),
                BadgeUpdate::Set("2".into()),
                BadgeUpdate::Clear,
                BadgeUpdate::Set("3".into()),
            ]
        );
    }

    /// RULE 2, the one the badge's honesty rests on: an update that never
    /// reached the tile must not be recorded. Every call here is dropped (the
    /// off-main-thread case), so a count seen four times must be RE-attempted
    /// all four times — if the ledger advanced on a drop, only the first would
    /// appear and the tile would sit blank while the app insists it shows "2".
    #[test]
    fn a_dropped_update_is_never_recorded() {
        assert_eq!(
            replay(&[2, 2, 2, 2], |_| false),
            vec![
                BadgeUpdate::Set("2".into()),
                BadgeUpdate::Set("2".into()),
                BadgeUpdate::Set("2".into()),
                BadgeUpdate::Set("2".into()),
            ]
        );
    }

    /// RULE 1 and the repair it buys: the first tick is dropped, the second
    /// lands, and only THEN does the count stop being re-sent. The badge heals
    /// itself on the next main-thread tick rather than inheriting a lie.
    #[test]
    fn a_landed_update_is_recorded_and_repairs_a_dropped_one() {
        assert_eq!(
            replay(&[5, 5, 5], |i| i > 0),
            vec![
                BadgeUpdate::Set("5".into()), // dropped — not recorded
                BadgeUpdate::Set("5".into()), // lands — recorded here, and only here
            ]                                 // third tick is now a true no-op
        );
    }
}
