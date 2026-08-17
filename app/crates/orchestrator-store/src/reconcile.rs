//! The freshness reconciler (docs/016): a part you marked **done** goes STALE
//! (◌ unverified) when the code it anchors has changed since you asserted it —
//! so the map stops drifting to fiction without you hand-maintaining dots.
//!
//! Staleness LOWERS confidence; it never flips the lifecycle (that stays your
//! assertion until you change it). The decision is a pure function (`staleness`)
//! tested in isolation; the fs/glob walk (`newest_anchor_mtime`) is thin.

use crate::tree::{Lifecycle, Part, StatusSource};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Cheap pre-filter: is this part even worth stat-ing the filesystem for? Only
/// an asserted-`done` part with anchors can go stale — gate the fs walk on this
/// so a tree full of todos costs nothing.
pub fn is_candidate(part: &Part) -> bool {
    part.lifecycle == Lifecycle::Done
        && part.status_source != StatusSource::Seed
        && part.status_at_secs > 0
        && !part.anchors.is_empty()
}

/// Should this part be flagged stale, given the newest mtime among its anchored
/// files? `Some(stale)` for a part we reconcile, `None` to LEAVE AS-IS.
///
/// We only judge a human/agent assertion of **done** that has anchors which
/// matched real files. A seed default, an idea/todo/building, an un-asserted or
/// un-anchored part has no "done" claim to age — so we never touch it (avoids
/// false ◌ noise on active work).
pub fn staleness(part: &Part, newest_anchor_mtime: Option<u64>) -> Option<bool> {
    if part.lifecycle != Lifecycle::Done {
        return None;
    }
    if part.status_source == StatusSource::Seed {
        return None; // defensive: a seed is never `done`, but never stale either
    }
    if part.status_at_secs == 0 {
        return None; // no real assertion time to compare against
    }
    match newest_anchor_mtime {
        None => None, // no anchors matched any file — can't judge
        Some(mt) => Some(mt > part.status_at_secs),
    }
}

/// Newest modification time (unix secs) among files matching any of `anchors`
/// (globs, relative to `root`). `None` if nothing matched an existing file.
pub fn newest_anchor_mtime(root: &Path, anchors: &[String]) -> Option<u64> {
    let mut newest: Option<u64> = None;
    for a in anchors {
        let Some(pat) = root.join(a).to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(paths) = glob::glob(&pat) else {
            continue;
        };
        for path in paths.flatten() {
            if let Some(secs) = mtime_secs(&path) {
                newest = Some(newest.map_or(secs, |n| n.max(secs)));
            }
        }
    }
    newest
}

fn mtime_secs(p: &Path) -> Option<u64> {
    let modified = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(lifecycle: Lifecycle, source: StatusSource, at: u64) -> Part {
        Part {
            id: 1,
            name: "X".into(),
            lifecycle,
            status_source: source,
            status_at_secs: at,
            ..Part::default()
        }
    }

    #[test]
    fn only_asserted_done_with_changed_code_is_stale() {
        use Lifecycle::*;
        use StatusSource::*;
        // done, asserted at T=1000, code changed at 2000 → STALE
        assert_eq!(staleness(&part(Done, User, 1000), Some(2000)), Some(true));
        // done, asserted at 2000, code last changed 1000 (older) → FRESH
        assert_eq!(staleness(&part(Done, User, 2000), Some(1000)), Some(false));
        // done but no anchored file matched → can't judge, leave as-is
        assert_eq!(staleness(&part(Done, User, 1000), None), None);
        // not done → never stale (no "done" claim to age)
        assert_eq!(staleness(&part(Building, User, 1000), Some(2000)), None);
        assert_eq!(staleness(&part(Todo, User, 1000), Some(2000)), None);
        // seed / no assertion time → skip
        assert_eq!(staleness(&part(Done, Seed, 1000), Some(2000)), None);
        assert_eq!(staleness(&part(Done, User, 0), Some(2000)), None);
    }
}
