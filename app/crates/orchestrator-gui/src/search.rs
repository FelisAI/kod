//! ⌘F terminal search state (#⌘F). The GRID search runs host-side
//! (Emulator::search_grid via the fork's wrapline-aware RegexSearch); this
//! module owns the GUI state machine: query editing, match navigation with
//! wrap, and re-anchoring `cur` after a live-output recompute (indices shift
//! as new matches stream in at the bottom — anchor by LINE, never by index).

use orchestrator_host::emulator::SearchMatch;
use orchestrator_host::SessionId;

#[derive(Default)]
pub struct SearchState {
    pub open: bool,
    /// the session the matches belong to — highlights/nav apply ONLY when it
    /// matches the on-stage session (the selection_session lesson, critique).
    pub session: Option<SessionId>,
    pub query: String,
    pub matches: Vec<SearchMatch>,
    pub total: u32,
    pub cur: usize,
    /// dispatch generation — a worker result older than this is dropped.
    pub gen: u64,
    /// a dispatch was requested while one was in flight — re-kick on drain.
    pub requeue: bool,
    /// the session's dirty counter at the last dispatch (drift recompute gate).
    pub last_dirty: u64,
}

impl SearchState {
    pub fn close(&mut self) {
        self.open = false;
        self.matches.clear();
        self.total = 0;
        self.cur = 0;
        self.requeue = false;
    }

    /// Next match in document order, wrapping at the end.
    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.cur = (self.cur + 1) % self.matches.len();
        }
    }

    /// Previous match, wrapping at the start.
    pub fn prev(&mut self) {
        if !self.matches.is_empty() {
            self.cur = (self.cur + self.matches.len() - 1) % self.matches.len();
        }
    }

    /// Adopt a fresh result set, re-anchoring `cur` to the match CLOSEST to the
    /// previously-current line (prev_line = lines_above_bottom before the swap).
    pub fn adopt(&mut self, matches: Vec<SearchMatch>, total: u32) {
        let prev_line = self.matches.get(self.cur).map(|m| m.lines_above_bottom);
        self.matches = matches;
        self.total = total;
        self.cur = match prev_line {
            Some(line) if !self.matches.is_empty() => self
                .matches
                .iter()
                .enumerate()
                .min_by_key(|(_, m)| m.lines_above_bottom.abs_diff(line))
                .map(|(i, _)| i)
                .unwrap_or(0),
            _ => 0,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(line: u32) -> SearchMatch {
        SearchMatch {
            lines_above_bottom: line,
            start: 0,
            end: 3,
        }
    }

    #[test]
    fn nav_wraps_both_ways() {
        let mut s = SearchState {
            matches: vec![m(9), m(5), m(0)],
            ..Default::default()
        };
        assert_eq!(s.cur, 0);
        s.next();
        s.next();
        assert_eq!(s.cur, 2);
        s.next();
        assert_eq!(s.cur, 0, "next wraps to the first match");
        s.prev();
        assert_eq!(s.cur, 2, "prev wraps to the last match");
        // empty matches: nav is a no-op, never a panic
        let mut e = SearchState::default();
        e.next();
        e.prev();
        assert_eq!(e.cur, 0);
    }

    #[test]
    fn adopt_reanchors_to_nearest_line_not_index() {
        let mut s = SearchState {
            matches: vec![m(20), m(10), m(0)],
            cur: 1,
            ..Default::default()
        };
        // live output appended 3 lines: every anchor shifts by +3, and a new
        // match appeared at the bottom — nearest to old line 10 is now 13.
        s.adopt(vec![m(23), m(13), m(3), m(0)], 4);
        assert_eq!(
            s.cur, 1,
            "re-anchored to the nearest LINE (13), not old index"
        );
        // first adoption (no previous current) starts at 0
        let mut f = SearchState::default();
        f.adopt(vec![m(7), m(2)], 2);
        assert_eq!(f.cur, 0);
    }
}
