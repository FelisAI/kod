//! Pure view-math for the terminal interaction layer (#9 slice 3b): URL
//! detection, run→link mapping, selection text, and scrollbar-drag math. These
//! live in the host crate (not the gpui bin crate) ONLY because `#[test]` can't
//! compile there — they have no gpui dependency. The GUI's mouse handlers are
//! thin wrappers over these.

use crate::emulator::{GridSnapshot, StyleRun};

const SCHEMES: [&str; 4] = ["https://", "http://", "file://", "ssh://"];

/// Find URL spans in a line of plain text. Returns `(char_start, char_end, url)`
/// in CHAR indices (so they align with `StyleRun.text.chars()`). Scheme-anchored;
/// trailing sentence punctuation is trimmed so "see https://x.com." links "…x.com".
pub fn detect_urls(text: &str) -> Vec<(usize, usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    'scan: while i < n {
        for scheme in SCHEMES {
            let s: Vec<char> = scheme.chars().collect();
            if i + s.len() <= n && chars[i..i + s.len()] == s[..] {
                let mut j = i + s.len();
                while j < n {
                    let c = chars[j];
                    if c.is_whitespace() || matches!(c, ')' | ']' | '}' | '>' | '<' | '"' | '\'' | '`' | '|') {
                        break;
                    }
                    j += 1;
                }
                let mut end = j;
                while end > i + s.len() && matches!(chars[end - 1], '.' | ',' | ';' | ':' | '!' | '?') {
                    end -= 1;
                }
                out.push((i, end, chars[i..end].iter().collect()));
                i = j;
                continue 'scan;
            }
        }
        i += 1;
    }
    out
}

/// One link per run: the URL a run belongs to, or `None`. (OSC-8 `StyleRun.uri`
/// will take precedence here once slice-3b step 4 adds it; for now it's
/// regex-only.) Output length == `runs.len()` so the renderer indexes by run idx.
pub fn row_links(runs: &[StyleRun], plain: &str) -> Vec<Option<String>> {
    // fast path (most rows): no scheme in the text AND no OSC-8 uri → nothing to
    // link, so skip the full char scan. (Keeps per-frame cost off the common row.)
    if !plain.contains("://") && runs.iter().all(|r| r.uri.is_none()) {
        return vec![None; runs.len()];
    }
    let urls = detect_urls(plain);
    let mut out = Vec::with_capacity(runs.len());
    let mut col = 0usize;
    for run in runs {
        let len = run.text.chars().count();
        let (start, end) = (col, col + len);
        // an OSC-8 hyperlink wins; else the first regex-detected URL whose char-span
        // overlaps this run (a URL split across runs makes every covered run link).
        let link = run.uri.clone().or_else(|| urls.iter().find(|(s, e, _)| *s < end && *e > start).map(|(_, _, u)| u.clone()));
        out.push(link);
        col = end;
    }
    out
}

/// The URL at grid cell `(row, col)`, or `None`. Mirrors `row_links` /
/// `render_row`'s run→link walk EXACTLY (same plain-text source, same
/// per-run char-count columns), so a click resolves the identical link the
/// renderer underlines. Used by the plain-click-to-open path (which lives in
/// the window mouse handler and only knows a cell, not a run).
pub fn link_at(snap: &GridSnapshot, row: usize, col: usize) -> Option<String> {
    let runs = snap.rows.get(row)?;
    let plains = snap.plain_lines();
    let plain = plains.get(row)?;
    let links = row_links(runs, plain);
    let mut c = 0usize;
    for (i, run) in runs.iter().enumerate() {
        let len = run.text.chars().count();
        if col >= c && col < c + len {
            return links.get(i).cloned().flatten();
        }
        c += len;
    }
    None
}

/// Normalize a selection pair so `lo ≤ hi` lexicographically (by row, then col).
fn order(a: (usize, usize), b: (usize, usize)) -> ((usize, usize), (usize, usize)) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The highlighted CHAR range `[start, end)` within `row` for a char-granular
/// selection — full row for middle rows, partial for the first/last, `None` when
/// the row is outside the selection. Clamped to `row_len`.
pub fn row_highlight(sel: Option<((usize, usize), (usize, usize))>, row: usize, row_len: usize) -> Option<(usize, usize)> {
    let s = sel?;
    let (lo, hi) = order(s.0, s.1);
    if row < lo.0 || row > hi.0 {
        return None;
    }
    let start = if row == lo.0 { lo.1 } else { 0 };
    let end = if row == hi.0 { hi.1 } else { row_len };
    Some((start.min(row_len), end.min(row_len)))
}

/// The selected sub-range WITHIN a run spanning chars `[col, run_end)`, returned
/// RELATIVE to the run start, or None if the run is outside the row highlight.
/// Lazy on purpose: when the run is past the selection, `e` can be < `col`, so
/// `e - col` must NOT be computed unless `s < e` (else usize underflow → abort).
pub fn run_highlight(hl: Option<(usize, usize)>, col: usize, run_end: usize) -> Option<(usize, usize)> {
    let (a, b) = hl?;
    let (s, e) = (a.max(col), b.min(run_end));
    (s < e).then(|| (s - col, e - col))
}

/// The plain text of a char-granular selection, newline-joined (⌘C copy). First/
/// last rows sliced by CHAR index (multibyte-safe ✦◆); middle rows whole. Out-of-
/// range indices clamp without panicking.
pub fn selection_text(snap: &GridSnapshot, sel: ((usize, usize), (usize, usize))) -> String {
    let (lo, hi) = order(sel.0, sel.1);
    let lines = snap.plain_lines();
    if lines.is_empty() {
        return String::new();
    }
    let slice = |s: &str, a: usize, b: usize| -> String { s.chars().skip(a).take(b.saturating_sub(a)).collect() };
    let lo_row = lo.0.min(lines.len() - 1);
    let hi_row = hi.0.min(lines.len() - 1);
    if lo_row == hi_row {
        return slice(&lines[lo_row], lo.1, hi.1);
    }
    let mut out = vec![slice(&lines[lo_row], lo.1, usize::MAX)];
    out.extend(lines[lo_row + 1..hi_row].iter().cloned());
    out.push(slice(&lines[hi_row], 0, hi.1));
    out.join("\n")
}

/// Pixel → grid cell `(absolute_row, col)`. Pure so the canvas mouse closure is a
/// thin wrapper (mirrors `scrollbar_delta`). `start` = first rendered row index
/// (rows scrolled above the viewport), `n` = rows rendered. Col is NOT clamped to
/// row length — a drag past EOL selects to line end; the slicers clamp.
#[allow(clippy::too_many_arguments)]
pub fn cell_at(px_x: f32, px_y: f32, origin_x: f32, origin_y: f32, pad_x: f32, pad_y: f32, char_w: f32, line_h: f32, start: usize, n: usize) -> (usize, usize) {
    let lx = (px_x - origin_x - pad_x).max(0.0);
    let ly = (px_y - origin_y - pad_y).max(0.0);
    let vis_row = if line_h > 0.0 { (ly / line_h).floor() as usize } else { 0 };
    let col = if char_w > 0.0 { (lx / char_w).floor() as usize } else { 0 };
    (start + vis_row.min(n.saturating_sub(1)), col)
}

/// Scrollbar thumb-drag → a `host.scroll` delta. Dragging DOWN (`cur_y > grab_y`)
/// moves toward the live bottom (display_offset → 0); UP moves into history.
/// `off0` = offset when the grab started; `current_off` = offset right now (so the
/// delta is correct even if output shifted the view mid-drag). Pure → unit-tested.
pub fn scrollbar_delta(grab_y: f32, cur_y: f32, off0: u32, current_off: u32, history: u32, visible: u32, container_h: f32) -> i32 {
    if history == 0 || container_h <= 0.0 {
        return 0;
    }
    let rows_per_px = (history + visible) as f32 / container_h;
    let dy = cur_y - grab_y;
    let target = (off0 as f32 - dy * rows_per_px).clamp(0.0, history as f32);
    target.round() as i32 - current_off as i32
}


/// Partition a run spanning chars `[col, run_end)` by highlight `marks`
/// (row-relative char ranges + a bg color, non-overlapping, sorted). Returns
/// run-RELATIVE (start, end, Option<bg>) pieces covering the whole run in
/// order — the one splitter for selection AND search marks (DRY: replaces the
/// single-range run_highlight path in render_row).
pub fn run_marks(marks: &[(usize, usize, u32)], col: usize, run_end: usize) -> Vec<(usize, usize, Option<u32>)> {
    let mut out = Vec::new();
    let mut pos = col;
    for &(s, e, bg) in marks {
        let (cs, ce) = (s.max(col), e.min(run_end));
        if cs >= ce || ce <= pos {
            continue;
        }
        let cs = cs.max(pos);
        if cs > pos {
            out.push((pos - col, cs - col, None));
        }
        out.push((cs - col, ce - col, Some(bg)));
        pos = ce;
    }
    if pos < run_end {
        out.push((pos - col, run_end - col, None));
    }
    out
}

/// Merge selection + search ranges for ONE row into sorted, non-overlapping
/// (start, end, bg) marks. Priority on overlap: current match > selection >
/// other matches (the thing you're navigating to must stay visible even
/// mid-selection).
pub fn row_marks(
    selection: Option<(usize, usize)>,
    sel_bg: u32,
    matches: &[(usize, usize)],
    match_bg: u32,
    current: Option<usize>,
    current_bg: u32,
) -> Vec<(usize, usize, u32)> {
    // candidate intervals with priority; a boundary sweep then picks the
    // highest-priority cover per segment and merges same-color neighbors.
    let mut iv: Vec<(usize, usize, u8, u32)> = Vec::new();
    if let Some((s, e)) = selection {
        if s < e {
            iv.push((s, e, 1, sel_bg));
        }
    }
    for (i, &(s, e)) in matches.iter().enumerate() {
        if s < e {
            let (p, bg) = if current == Some(i) { (2, current_bg) } else { (0, match_bg) };
            iv.push((s, e, p, bg));
        }
    }
    if iv.is_empty() {
        return Vec::new();
    }
    let mut bounds: Vec<usize> = iv.iter().flat_map(|&(s, e, _, _)| [s, e]).collect();
    bounds.sort_unstable();
    bounds.dedup();
    let mut out: Vec<(usize, usize, u32)> = Vec::new();
    for w in bounds.windows(2) {
        let (a, b) = (w[0], w[1]);
        if let Some(&(_, _, _, bg)) = iv.iter().filter(|&&(s, e, _, _)| s <= a && b <= e).max_by_key(|&&(_, _, p, _)| p) {
            if let Some(last) = out.last_mut() {
                if last.1 == a && last.2 == bg {
                    last.1 = b;
                    continue;
                }
            }
            out.push((a, b, bg));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> StyleRun {
        StyleRun { text: text.into(), fg: 0, bg: 0, bold: false, italic: false, underline: false, uri: None }
    }
    fn run_uri(text: &str, uri: &str) -> StyleRun {
        StyleRun { uri: Some(uri.into()), ..run(text) }
    }

    #[test]
    fn detect_urls_finds_schemes_and_trims_punctuation() {
        assert_eq!(detect_urls("no link here"), vec![]);
        assert_eq!(detect_urls("see https://x.com."), vec![(4, 17, "https://x.com".into())]);
        let two = detect_urls("a http://h.io and ssh://b/c)");
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].2, "http://h.io");
        assert_eq!(two[1].2, "ssh://b/c"); // trailing ) excluded
        // char (not byte) offsets survive a multibyte prefix.
        let emoji = detect_urls("✦ https://a.b");
        assert_eq!(emoji, vec![(2, 13, "https://a.b".into())]);
    }

    #[test]
    fn row_links_maps_runs_to_overlapping_url() {
        // runs concatenate to "go https://x.io now"
        let runs = [run("go "), run("https://x.io"), run(" now")];
        let links = row_links(&runs, "go https://x.io now");
        assert_eq!(links, vec![None, Some("https://x.io".into()), None]);
        // a URL split across two runs → both link.
        let split = [run("https://"), run("x.io")];
        let l2 = row_links(&split, "https://x.io");
        assert_eq!(l2, vec![Some("https://x.io".into()), Some("https://x.io".into())]);
        assert_eq!(row_links(&[run("plain")], "plain"), vec![None]);
        // OSC-8 uri wins over (and without) any regex match.
        assert_eq!(row_links(&[run_uri("click", "https://osc8")], "click"), vec![Some("https://osc8".into())]);
        assert_eq!(row_links(&[run_uri("https://regex", "https://osc8")], "https://regex"), vec![Some("https://osc8".into())]);
    }

    #[test]
    fn link_at_resolves_cell_to_url() {
        // runs concatenate to "go https://x.io now" — cols: "go "=0..3,
        // "https://x.io"=3..15, " now"=15..19.
        let snap = GridSnapshot {
            seq: 0,
            rows: vec![vec![run("go "), run("https://x.io"), run(" now")]],
            cursor: (0, 0),
            cursor_visible: false,
            bracketed_paste: false,
            kitty_keys: false,
            display_offset: 0,
            alt_screen: false,
            history_size: 0,
        };
        assert_eq!(link_at(&snap, 0, 0), None); // 'g' in "go "
        assert_eq!(link_at(&snap, 0, 3), Some("https://x.io".into())); // URL start
        assert_eq!(link_at(&snap, 0, 14), Some("https://x.io".into())); // URL last char
        assert_eq!(link_at(&snap, 0, 15), None); // ' ' in " now"
        assert_eq!(link_at(&snap, 0, 99), None); // past EOL
        assert_eq!(link_at(&snap, 5, 0), None); // out-of-range row
    }

    #[test]
    fn row_highlight_partials_first_last_full_middle() {
        // single-line selection (2,3)→(2,7) on a 20-char row.
        assert_eq!(row_highlight(Some(((2, 3), (2, 7))), 2, 20), Some((3, 7)));
        assert_eq!(row_highlight(Some(((2, 7), (2, 3))), 2, 20), Some((3, 7))); // reversed normalizes
        // multi-line (1,4)→(3,2): first row from col 4 to EOL, middle full, last to col 2.
        assert_eq!(row_highlight(Some(((1, 4), (3, 2))), 1, 10), Some((4, 10)));
        assert_eq!(row_highlight(Some(((1, 4), (3, 2))), 2, 10), Some((0, 10)));
        assert_eq!(row_highlight(Some(((1, 4), (3, 2))), 3, 8), Some((0, 2)));
        assert_eq!(row_highlight(Some(((1, 4), (3, 2))), 0, 10), None); // outside
        assert_eq!(row_highlight(Some(((1, 4), (3, 2))), 4, 10), None);
        assert_eq!(row_highlight(Some(((2, 3), (2, 99))), 2, 5), Some((3, 5))); // col clamps to len
        assert_eq!(row_highlight(None, 2, 10), None);
    }

    #[test]
    fn run_highlight_lazy_no_underflow_past_selection() {
        assert_eq!(run_highlight(Some((2, 7)), 0, 5), Some((2, 5))); // run overlaps selection start
        assert_eq!(run_highlight(Some((2, 7)), 5, 10), Some((0, 2))); // run holds selection tail
        // run entirely PAST the selection: e (3) < col (5). The crash was here —
        // e - col underflowed because then_some computed it eagerly. Must be None.
        assert_eq!(run_highlight(Some((0, 3)), 5, 10), None);
        assert_eq!(run_highlight(Some((8, 9)), 0, 5), None); // run entirely before selection
        assert_eq!(run_highlight(None, 0, 5), None);
    }

    #[test]
    fn selection_text_slices_chars_first_last() {
        let snap = GridSnapshot {
            seq: 0,
            rows: vec![vec![run("aaaa")], vec![run("bbbb")], vec![run("✦ cc")]],
            cursor: (0, 0),
            cursor_visible: false,
            bracketed_paste: false,
            kitty_keys: false,
            display_offset: 0,
            alt_screen: false,
            history_size: 0,
        };
        assert_eq!(selection_text(&snap, ((0, 1), (0, 3))), "aa"); // single-row char slice
        assert_eq!(selection_text(&snap, ((0, 2), (2, 1))), "aa\nbbbb\n✦"); // first sliced, mid whole, last char-sliced (multibyte!)
        assert_eq!(selection_text(&snap, ((2, 1), (0, 2))), "aa\nbbbb\n✦"); // reversed
        assert_eq!(selection_text(&snap, ((1, 99), (1, 99))), ""); // out-of-range, no panic
    }

    #[test]
    fn cell_at_maps_pixels_to_cells_and_clamps() {
        // origin (10,20), pad (12,8), char 7.7, line 15, start 0, n 40.
        assert_eq!(cell_at(22.0, 28.0, 10.0, 20.0, 12.0, 8.0, 7.7, 15.0, 0, 40), (0, 0)); // exactly origin+pad
        assert_eq!(cell_at(22.0 + 2.5 * 7.7, 28.0 + 1.5 * 15.0, 10.0, 20.0, 12.0, 8.0, 7.7, 15.0, 0, 40), (1, 2)); // floor
        assert_eq!(cell_at(0.0, 0.0, 10.0, 20.0, 12.0, 8.0, 7.7, 15.0, 0, 40), (0, 0)); // left/above → clamp 0
        assert_eq!(cell_at(22.0, 9999.0, 10.0, 20.0, 12.0, 8.0, 7.7, 15.0, 5, 10), (14, 0)); // below grid → start+n-1
    }

    #[test]
    fn scrollbar_delta_tracks_drag_direction_and_clamps() {
        // container 100px tall, 100 lines history, 40 visible → 1.4 rows/px.
        // drag DOWN 50px from offset 80 → toward live: 80 - 50*1.4 = 10 → delta 10-80=-70.
        assert_eq!(scrollbar_delta(0.0, 50.0, 80, 80, 100, 40, 100.0), -70);
        // drag UP 50px from offset 0 → into history: 0 + 70 = 70 → delta 70.
        assert_eq!(scrollbar_delta(50.0, 0.0, 0, 0, 100, 40, 100.0), 70);
        // clamps at history top.
        assert_eq!(scrollbar_delta(0.0, -1000.0, 50, 50, 100, 40, 100.0), 50); // target 100, delta 100-50
        // no history → no-op.
        assert_eq!(scrollbar_delta(0.0, 50.0, 0, 0, 0, 40, 100.0), 0);
    }
}

#[cfg(test)]
mod mark_tests {
    use super::*;
    const SEL: u32 = 1;
    const HIT: u32 = 2;
    const CUR: u32 = 3;

    #[test]
    fn run_marks_partitions_a_run() {
        // run covers chars [10, 20); one mark [12, 15)
        let p = run_marks(&[(12, 15, HIT)], 10, 20);
        assert_eq!(p, vec![(0, 2, None), (2, 5, Some(HIT)), (5, 10, None)]);
        // no marks → one unmarked piece
        assert_eq!(run_marks(&[], 10, 20), vec![(0, 10, None)]);
        // mark spanning the whole run
        assert_eq!(run_marks(&[(0, 99, HIT)], 10, 20), vec![(0, 10, Some(HIT))]);
        // mark entirely outside → unmarked
        assert_eq!(run_marks(&[(0, 5, HIT)], 10, 20), vec![(0, 10, None)]);
    }

    #[test]
    fn row_marks_priority_current_over_selection_over_match() {
        // selection [0,10) covering match [2,5) (current) and match [7,9)
        let m = row_marks(Some((0, 10)), SEL, &[(2, 5), (7, 9)], HIT, Some(0), CUR);
        // current match survives inside the selection; the other match yields to selection
        assert_eq!(m, vec![(0, 2, SEL), (2, 5, CUR), (5, 10, SEL)]);
        // no selection: both matches, current colored differently
        let m2 = row_marks(None, SEL, &[(2, 5), (7, 9)], HIT, Some(1), CUR);
        assert_eq!(m2, vec![(2, 5, HIT), (7, 9, CUR)]);
    }
}
