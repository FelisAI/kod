//! cmdbar — the "talk to evolve" command bar's PURE brain (docs/019 COMMAND
//! BAR + ruling 9). The bottom strip takes a typed line and routes it down one
//! of two lanes:
//!
//! - **Imperative** ("rename auth to Identity", "move billing under Growth",
//!   "delete tech") → a deterministic parse to a concrete op, applied INSTANTLY
//!   on the HUMAN lane (dictation is human intent through a machine hand); and
//! - **Design intent** ("flesh out the sync area", "reorganize by product
//!   area") → the MACHINE lane as a fenced changeset the user reviews.
//!
//! The deterministic parser IS the classifier: a line that parses to the small
//! imperative grammar is human-lane; everything else is machine-lane (docs/019:
//! "a small deterministic imperative parser for the common verbs; everything
//! else → machine lane"). Everything here is pure over strings + an
//! (id, name) index — the integrator (main.rs) owns the tree, focus, keystream,
//! and turns the parse into real `DiffOp`s / an agentic dispatch. Unit-tested;
//! no I/O, no gpui.

use orchestrator_store::PartId;

/// Which lane a typed command routes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lane {
    /// A concrete imperative → a `DiffOp` on the human lane (instant, undoable).
    Imperative(Imperative),
    /// Design intent → the machine lane (a fenced changeset).
    Intent,
}

/// Where a `move` sends its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveDest {
    /// "under X" / "into X" / "to X" — a named new parent.
    Named(String),
    /// "to root" / "to the top" / "to top level" — promote to a root.
    Root,
}

/// A parsed imperative (names only — resolution to ids is the integrator's,
/// which owns the tree). The common verbs docs/019 names: rename / move / delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Imperative {
    Rename { target: String, to: String },
    Move { target: String, dest: MoveDest },
    Delete { target: String },
}

/// Scope decision for a machine-lane intent (docs/019 ruling 9 + schema
/// "scope inference"): the fence defaults to the selection's subtree, but a
/// cross-subtree restructure asks one keystroke before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeDecision {
    /// Fence to the current selection's subtree (scope_part_id = selection).
    Subtree,
    /// Whole map (scope_part_id NULL) — no selection to fence to.
    WholeMap,
    /// The intent implies cross-subtree restructuring — ask "this subtree /
    /// whole map?" before dispatch (the marquee sentence must not dead-end).
    Ask,
}

/// Resolution of a typed node name against the live tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolve {
    /// Exactly one node matched.
    One(PartId),
    /// No node matched the name.
    None,
    /// More than one node matched (the user must disambiguate).
    Ambiguous,
}

/// Classify a typed line into its lane. The deterministic imperative parse is
/// the whole gate: parses → the human lane with that op; doesn't → the machine
/// lane (design intent). An empty line is intent (a no-op the integrator drops).
pub fn classify(query: &str) -> Lane {
    match parse_imperative(query) {
        Some(imp) => Lane::Imperative(imp),
        None => Lane::Intent,
    }
}

/// Parse one of the common imperatives, or None (→ the line is design intent).
/// Case-insensitive on the VERB and connectives; node names keep their case
/// (they're matched case-insensitively at resolution). Every branch requires
/// non-empty operands, so a partial phrase ("rename foo", "reorganize by area")
/// falls through to the machine lane rather than making a broken op.
pub fn parse_imperative(query: &str) -> Option<Imperative> {
    let q = query.trim();
    let lower = q.to_lowercase();
    // verb is the first whitespace-delimited token.
    let (verb, rest) = split_first_word(q);
    let vlow = verb.to_lowercase();
    match vlow.as_str() {
        "rename" => {
            let (target, to) = split_on_any(rest, &[" to ", " -> ", " → ", " into "])?;
            let (target, to) = (target.trim(), to.trim());
            (!target.is_empty() && !to.is_empty()).then(|| Imperative::Rename {
                target: strip_quotes(target).to_string(),
                to: strip_quotes(to).to_string(),
            })
        }
        "move" => {
            let (target, dest_raw) = split_on_any(rest, &[" under ", " into ", " to ", " below "])?;
            let (target, dest_raw) = (target.trim(), dest_raw.trim());
            if target.is_empty() || dest_raw.is_empty() {
                return None;
            }
            let dest = parse_move_dest(dest_raw);
            Some(Imperative::Move {
                target: strip_quotes(target).to_string(),
                dest,
            })
        }
        "delete" | "remove" => {
            // "delete X" / "remove X" — the rest is the target. A trailing
            // "and …" makes it prose, not a clean op → machine lane.
            let target = strip_quotes(rest.trim());
            if target.is_empty() || lower.contains(" and ") {
                return None;
            }
            Some(Imperative::Delete {
                target: target.to_string(),
            })
        }
        _ => None,
    }
}

/// "root" / "top" / "top level" / "the top" → Root; anything else is a named
/// destination (a leading "the " is dropped so "the Growth area" resolves).
fn parse_move_dest(raw: &str) -> MoveDest {
    let low = raw.to_lowercase();
    let low = low.strip_prefix("the ").unwrap_or(&low).trim();
    if matches!(
        low,
        "root" | "top" | "top level" | "the top level" | "toplevel"
    ) {
        MoveDest::Root
    } else {
        MoveDest::Named(strip_quotes(raw.strip_prefix("the ").unwrap_or(raw).trim()).to_string())
    }
}

/// Infer the machine-lane fence (docs/019 ruling 9). No selection → whole map;
/// a cross-subtree restructure → ask; else fence to the selection's subtree.
pub fn infer_scope(query: &str, has_selection: bool) -> ScopeDecision {
    if !has_selection {
        return ScopeDecision::WholeMap;
    }
    if implies_cross_subtree(query) {
        ScopeDecision::Ask
    } else {
        ScopeDecision::Subtree
    }
}

/// Does the intent imply restructuring that crosses the selected subtree
/// (new roots, moves out, a whole-map regroup)? Keyword heuristic (v1): the
/// re-organize / re-structure / group-by / whole-map family. Pure + tested so
/// the marquee's scope keystroke fires on exactly the sentences that need it.
pub fn implies_cross_subtree(query: &str) -> bool {
    let q = query.to_lowercase();
    const MARKERS: [&str; 12] = [
        "reorganize",
        "reorganise",
        "restructure",
        "regroup",
        "group by",
        "organize by",
        "organise by",
        "by product area",
        "whole map",
        "everything",
        "new root",
        "top level",
    ];
    MARKERS.iter().any(|m| q.contains(m))
}

/// Resolve a typed node name against `(id, name)` pairs (the live tree). Exact
/// (case-insensitive) match wins outright; else a unique case-insensitive
/// substring match; more than one substring hit is Ambiguous (the user
/// disambiguates). Empty name → None. Pure so name resolution is unit-tested.
pub fn resolve(name: &str, index: &[(PartId, String)]) -> Resolve {
    let n = name.trim().to_lowercase();
    if n.is_empty() {
        return Resolve::None;
    }
    // exact (case-insensitive) — if exactly one, take it even when it is also a
    // substring of others ("auth" exact beats "auth flow" partial).
    let exact: Vec<PartId> = index
        .iter()
        .filter(|(_, nm)| nm.to_lowercase() == n)
        .map(|(id, _)| *id)
        .collect();
    if exact.len() == 1 {
        return Resolve::One(exact[0]);
    }
    if exact.len() > 1 {
        return Resolve::Ambiguous;
    }
    let subs: Vec<PartId> = index
        .iter()
        .filter(|(_, nm)| nm.to_lowercase().contains(&n))
        .map(|(id, _)| *id)
        .collect();
    match subs.len() {
        0 => Resolve::None,
        1 => Resolve::One(subs[0]),
        _ => Resolve::Ambiguous,
    }
}

// ---- small string helpers (pure) ----

fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

/// Split `s` at the FIRST occurrence (case-insensitive) of any separator in
/// `seps`, returning (before, after). None when no separator is present.
fn split_on_any<'a>(s: &'a str, seps: &[&str]) -> Option<(&'a str, &'a str)> {
    // Separators are ASCII, so match them against the ORIGINAL bytes with a
    // length-preserving case-insensitive compare (review: `to_lowercase()`
    // changes byte lengths for non-ASCII input — e.g. 'İ' → 2 bytes — shifting
    // every offset, so slicing the original mis-split or panicked mid-char).
    let sb = s.as_bytes();
    let mut best: Option<(usize, usize)> = None; // (start byte, sep len)
    for sep in seps {
        let sepb = sep.as_bytes();
        let n = sepb.len();
        if n == 0 || n > sb.len() {
            continue;
        }
        for i in 0..=sb.len() - n {
            if sb[i..i + n].eq_ignore_ascii_case(sepb) {
                if best.map(|(b, _)| i < b).unwrap_or(true) {
                    best = Some((i, n));
                }
                break; // earliest occurrence of THIS separator
            }
        }
    }
    // pos and pos+len land on ASCII bytes (the separator is ASCII), which are
    // always char boundaries, so slicing the original never panics.
    best.map(|(pos, len)| (&s[..pos], &s[pos + len..]))
}

/// Strip a single pair of matching wrapping quotes (straight or curly) so
/// `rename "auth" to 'Identity'` resolves cleanly.
fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    let pairs = [('"', '"'), ('\'', '\''), ('“', '”'), ('‘', '’'), ('`', '`')];
    for (o, c) in pairs {
        if let Some(inner) = s.strip_prefix(o).and_then(|x| x.strip_suffix(c)) {
            if !inner.is_empty() {
                return inner.trim();
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rename_with_every_connective_and_strips_quotes() {
        assert_eq!(
            parse_imperative("rename auth to Identity"),
            Some(Imperative::Rename {
                target: "auth".into(),
                to: "Identity".into()
            })
        );
        assert_eq!(
            parse_imperative("Rename \"auth flow\" -> 'Identity'"),
            Some(Imperative::Rename {
                target: "auth flow".into(),
                to: "Identity".into()
            })
        );
        assert_eq!(
            parse_imperative("rename billing → Growth"),
            Some(Imperative::Rename {
                target: "billing".into(),
                to: "Growth".into()
            })
        );
        // a rename with no target-or-name is NOT a clean op → machine lane.
        assert_eq!(parse_imperative("rename auth"), None);
        assert_eq!(parse_imperative("rename to Identity"), None);
    }

    #[test]
    fn parses_move_under_into_to_and_root() {
        assert_eq!(
            parse_imperative("move billing under Growth"),
            Some(Imperative::Move {
                target: "billing".into(),
                dest: MoveDest::Named("Growth".into())
            })
        );
        assert_eq!(
            parse_imperative("move Store into Engine"),
            Some(Imperative::Move {
                target: "Store".into(),
                dest: MoveDest::Named("Engine".into())
            })
        );
        // "to the Growth area" drops the leading article.
        assert_eq!(
            parse_imperative("move auth to the Growth area"),
            Some(Imperative::Move {
                target: "auth".into(),
                dest: MoveDest::Named("Growth area".into())
            })
        );
        // promote to root in its several spellings.
        for phrase in [
            "move Store to root",
            "move Store to top",
            "move Store to top level",
            "move Store to the top level",
        ] {
            assert_eq!(
                parse_imperative(phrase),
                Some(Imperative::Move {
                    target: "Store".into(),
                    dest: MoveDest::Root
                }),
                "{phrase}"
            );
        }
        assert_eq!(
            parse_imperative("move billing"),
            None,
            "no destination → machine lane"
        );
    }

    #[test]
    fn parses_delete_and_remove_but_not_prose() {
        assert_eq!(
            parse_imperative("delete tech"),
            Some(Imperative::Delete {
                target: "tech".into()
            })
        );
        assert_eq!(
            parse_imperative("remove “old area”"),
            Some(Imperative::Delete {
                target: "old area".into()
            })
        );
        // prose that merely starts with delete is intent, not a clean op.
        assert_eq!(parse_imperative("delete tech and regroup the rest"), None);
        assert_eq!(parse_imperative("delete"), None);
    }

    #[test]
    fn classify_is_the_parser_intent_is_the_fallthrough() {
        assert!(matches!(
            classify("rename auth to Identity"),
            Lane::Imperative(_)
        ));
        assert!(matches!(
            classify("move billing under Growth"),
            Lane::Imperative(_)
        ));
        // the docs/019 design-intent examples all fall to the machine lane.
        assert_eq!(classify("flesh out the sync area"), Lane::Intent);
        assert_eq!(classify("reorganize by product area"), Lane::Intent);
        assert_eq!(
            classify("this Tech section is wrong, group by product area"),
            Lane::Intent
        );
        assert_eq!(classify(""), Lane::Intent);
    }

    #[test]
    fn scope_inference_defaults_to_subtree_and_asks_on_cross_subtree() {
        // no selection → whole map, whatever the words.
        assert_eq!(
            infer_scope("reorganize everything", false),
            ScopeDecision::WholeMap
        );
        // a contained "flesh out" fences to the selection.
        assert_eq!(
            infer_scope("flesh out the sync area", true),
            ScopeDecision::Subtree
        );
        // a cross-subtree restructure asks the one keystroke (the marquee path).
        assert_eq!(
            infer_scope("reorganize by product area", true),
            ScopeDecision::Ask
        );
        assert_eq!(
            infer_scope("regroup these under new roots", true),
            ScopeDecision::Ask
        );
        assert_eq!(
            infer_scope("break the whole map apart", true),
            ScopeDecision::Ask
        );
    }

    #[test]
    fn implies_cross_subtree_covers_the_restructure_family() {
        for q in [
            "reorganize by X",
            "restructure this",
            "group by product area",
            "move to top level",
            "organize by area",
        ] {
            assert!(implies_cross_subtree(q), "{q}");
        }
        for q in [
            "flesh out sync",
            "add a caching child",
            "describe the store",
        ] {
            assert!(!implies_cross_subtree(q), "{q}");
        }
    }

    #[test]
    fn resolve_prefers_exact_then_unique_substring_then_ambiguous() {
        let idx: Vec<(PartId, String)> = vec![
            (1, "auth".into()),
            (2, "auth flow".into()),
            (3, "Billing".into()),
        ];
        // exact wins even though "auth" is a substring of "auth flow".
        assert_eq!(resolve("auth", &idx), Resolve::One(1));
        assert_eq!(
            resolve("AUTH", &idx),
            Resolve::One(1),
            "case-insensitive exact"
        );
        // unique substring.
        assert_eq!(resolve("bill", &idx), Resolve::One(3));
        // ambiguous substring (both auth rows contain "au").
        assert_eq!(resolve("au", &idx), Resolve::Ambiguous);
        assert_eq!(resolve("nope", &idx), Resolve::None);
        assert_eq!(resolve("  ", &idx), Resolve::None);
        // two exact matches (a duplicate name) → ambiguous.
        let dup: Vec<(PartId, String)> = vec![(1, "Store".into()), (2, "store".into())];
        assert_eq!(resolve("store", &dup), Resolve::Ambiguous);
    }

    #[test]
    fn imperative_split_is_correct_across_multibyte_targets() {
        // review: 'İ' (U+0130) lowercases to 2 code units, so a lowercase-based
        // split shifted the offset and mangled/panicked. The name must survive.
        assert_eq!(
            parse_imperative("rename İzmir to Coast"),
            Some(Imperative::Rename {
                target: "İzmir".into(),
                to: "Coast".into()
            })
        );
        // a multibyte separator-adjacent target on the NEW-name side too.
        assert_eq!(
            parse_imperative("rename auth to Ídentity"),
            Some(Imperative::Rename {
                target: "auth".into(),
                to: "Ídentity".into()
            })
        );
        // classify must not panic on multibyte input (runs every keystroke).
        let _ = classify("rename Ｆｕｌｌwidth to x");
    }
}
