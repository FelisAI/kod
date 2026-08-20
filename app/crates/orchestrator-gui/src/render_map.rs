use gpui::*;
use crate::*;


impl Orchestrator {

    /// The live map (left) + outline (right) from the real tree.
    /// The living product map (#10): the canonical Map+Outline split — a
    /// spatial brain-map CANVAS (mapview) + the Outline drill pane
    /// (outlinepane: focus card, decision log, children, per-op proposals).
    /// Flow mode = the canvas full-width.
    pub(crate) fn render_map_outline(
        &self,
        parts: &[DesignPart],
        tree: &[TreeNode],
        pending: &[orchestrator_store::PendingDiff],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use std::collections::{HashMap, HashSet};
        let slug = self.project().slug.clone();
        let outline_open = self.outline_open(&slug);
        let (canvas_w, canvas_h) = if outline_open {
            (560.0f32, 620.0f32)
        } else {
            (980.0f32, 620.0f32)
        };

        // ---- drill root (docs/011 §B): the canvas shows children(map_root) ----
        // A stale root (node removed) falls back to the project root silently —
        // the setting self-heals on the next set_map_root.
        fn find_in<'a>(nodes: &'a [TreeNode], id: PartId) -> Option<&'a TreeNode> {
            for n in nodes {
                if n.part.id == id {
                    return Some(n);
                }
                if let Some(h) = find_in(&n.children, id) {
                    return Some(h);
                }
            }
            None
        }
        let map_root = self
            .map_root_of(&slug)
            .filter(|id| find_in(tree, *id).is_some());
        let view_tree: &[TreeNode] = match map_root {
            Some(id) => &find_in(tree, id).unwrap().children,
            None => tree,
        };
        // crumb path: project ▸ … ▸ current root (parent chain from the flat parts)
        let crumbs: Vec<(PartId, String)> = {
            let mut v = Vec::new();
            let by_id: HashMap<PartId, &DesignPart> = parts.iter().map(|p| (p.id, p)).collect();
            let mut cur = map_root;
            while let Some(id) = cur {
                if let Some(p) = by_id.get(&id) {
                    v.push((id, p.name.clone()));
                    cur = p.parent_id;
                } else {
                    break;
                }
            }
            v.reverse();
            v
        };

        // ---- layout: persisted positions win; a live drag previews on top ----
        let persisted: HashMap<PartId, (f64, f64)> = parts
            .iter()
            .filter_map(|p| p.map_x.zip(p.map_y).map(|xy| (p.id, xy)))
            .collect();
        let drag = self.map_drag;
        let lay = |ch: f32| {
            mapview::layout_nodes(
                view_tree,
                |id| match drag {
                    Some(d) if d.id == id => Some(d.cur),
                    _ => persisted.get(&id).copied(),
                },
                canvas_w,
                ch,
            )
        };
        // content-sized canvas: pack once, grow the frame to fit, re-pack so
        // the normalization frame matches what's rendered (drag/persist math).
        let mut positions = lay(canvas_h);
        let need = mapview::content_height(&positions);
        let canvas_h = if need > canvas_h { need } else { canvas_h };
        if need > 620.0 {
            positions = lay(canvas_h);
        }

        // ---- focus: whole-tree resolution (child rows focus children) ----
        fn find_node<'a>(nodes: &'a [TreeNode], id: PartId) -> Option<&'a TreeNode> {
            for n in nodes {
                if n.part.id == id {
                    return Some(n);
                }
                if let Some(h) = find_node(&n.children, id) {
                    return Some(h);
                }
            }
            None
        }
        let focused_id = self
            .focused_part
            .filter(|id| find_node(tree, *id).is_some())
            .or_else(|| tree.first().map(|n| n.part.id));
        let fp_node = focused_id.and_then(|id| find_node(tree, id));

        // ---- map handlers (module is main.rs-agnostic: Rc callbacks) ----
        let ent = cx.entity();
        let (cw, ch) = (canvas_w, canvas_h);
        let pos_by_id: HashMap<PartId, mapview::NodePos> =
            positions.iter().map(|p| (p.id, *p)).collect();
        // flat (id, parent) pairs — cheap ancestry math for the ⌥-drag deny
        // set without cloning Part rows into every handler.
        let pairs: Vec<(PartId, Option<PartId>)> =
            parts.iter().map(|p| (p.id, p.parent_id)).collect();
        let map_handlers = mapview::MapHandlers {
            node_down: {
                let ent = ent.clone();
                let pos_by_id = pos_by_id.clone();
                let pairs = pairs.clone();
                let slug = slug.clone();
                Rc::new(move |id, at, _w, app| {
                    let Some(np) = pos_by_id.get(&id).copied() else {
                        return;
                    };
                    ent.update(app, |this, cx| {
                        // blur-commit a live Detail edit before refocusing —
                        // and never leave an invisible editor eating keys
                        // (docs/019 save-on-blur; review 1b key-sink rule).
                        this.blur_outline_edit(cx);
                        this.focused_part = Some(id);
                        this.outline_link_open = false;
                        // selection opens the outline — DEFERRED past the
                        // double-click window, and for BOTH generations
                        // (satellites arm no drag, so a release-time open
                        // never fires for them).
                        if !this.outline_open(&slug) {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            this.outline_open_pending = Some((slug.clone(), now));
                        }
                        // satellites have no pin at THIS level (their map_x/map_y
                        // belongs to their parent's canvas — frame per level);
                        // arming a drag would persist a nonsense pin there.
                        // ⌥-drag reparent is likewise gen-0-only for now: the
                        // layout derives satellites from their parent, so a
                        // dragged satellite couldn't follow the cursor — the
                        // context menu + palette Move-to are their reparent
                        // path (docs/019 risks names them as the fallback).
                        if np.gen == 0 {
                            let orig = mapview::norm_of(&np, cw, ch);
                            this.map_drag = Some(mapview::MapDrag {
                                id,
                                grab: at,
                                orig,
                                node_w: np.fp_w,
                                node_h: np.fp_h,
                                cur: orig,
                                alt: false,
                                target: None,
                            });
                            this.map_drop_deny = mapview::subtree_ids(&pairs, id);
                        }
                        cx.notify();
                    });
                })
            },
            cycle_status: {
                let ent = ent.clone();
                Rc::new(move |id, lc, _w, app| {
                    ent.update(app, |this, cx| {
                        this.set_part_status(id, next_lifecycle(lc), cx)
                    });
                })
            },
            drag_move: {
                let ent = ent.clone();
                let positions = positions.clone();
                Rc::new(move |cursor, canvas_pt, alt, _w, app| {
                    ent.update(app, |this, cx| {
                        if let Some(mut d) = this.map_drag {
                            d.cur = mapview::drag_update(&d, cursor, cw, ch);
                            // ⌥ tracked LIVE (docs/019 CANVAS): pressing or
                            // releasing Option mid-drag converts the gesture
                            // between reposition-and-pin and reparent.
                            d.alt = alt;
                            d.target = if alt {
                                mapview::drop_target_at(&positions, &this.map_drop_deny, canvas_pt)
                            } else {
                                None
                            };
                            this.map_drag = Some(d);
                            cx.notify();
                        }
                    });
                })
            },
            drag_up: {
                let ent = ent.clone();
                let slug = slug.clone();
                Rc::new(move |_w, app| {
                    ent.update(app, |this, cx| {
                        if let Some(d) = this.map_drag.take() {
                            this.map_drop_deny.clear();
                            // dead-zone: a focus-click with 1px of mouse drift must
                            // not pin the card out of auto-layout forever (review
                            // 1b) — require real movement before persisting. The
                            // same zone keeps an ⌥-click from "reparenting" to
                            // wherever the cursor happened to rest.
                            let moved = ((d.cur.0 - d.orig.0).abs() * f64::from(cw))
                                .max((d.cur.1 - d.orig.1).abs() * f64::from(ch))
                                > 4.0;
                            if moved && d.alt {
                                // ⌥-drop = REPARENT (docs/019 CANVAS): onto a
                                // node → Move under it (appended last); onto
                                // empty canvas → Move to the current drill-frame
                                // root (a calm no-op when already there).
                                // map_root_of BEFORE the store lock — it locks
                                // the store itself on a cache miss.
                                let new_parent = d.target.or_else(|| this.map_root_of(&slug));
                                let mut store =
                                    this.store.lock().unwrap_or_else(|e| e.into_inner());
                                let parts = store.load_tree(&slug).unwrap_or_default();
                                if let Some(op) =
                                    orchestrator_store::reparent_op(&parts, d.id, new_parent)
                                {
                                    let _ = store.accept_diff_from(&slug, &[op], "user", None);
                                    // a reparent is a STRUCTURE gesture, not a
                                    // placement: the old pin belongs to the old
                                    // frame (frame-per-level, docs/011 §B) — clear
                                    // it so the node auto-lays-out in its new
                                    // home. Pins are spatial memory, unjournaled.
                                    let _ = store.clear_part_pos(d.id);
                                }
                                this.outline_open_pending = None;
                            } else if moved {
                                if let Ok(store) = this.store.lock() {
                                    let _ = store.set_part_pos(d.id, d.cur.0, d.cur.1);
                                }
                                // a real drag isn't a select-click — don't open.
                                this.outline_open_pending = None;
                            }
                            cx.notify();
                        }
                    });
                })
            },
            dispatch: {
                let ent = ent.clone();
                Rc::new(move |id, alt, w, app| {
                    ent.update(app, |this, cx| this.dispatch_to_part(id, alt, w, cx))
                })
            },
            open_agent: {
                let ent = ent.clone();
                Rc::new(move |cli, w, app| {
                    ent.update(app, |this, cx| {
                        if let Some((slug, sid)) = this.find_live_by_cli_id(&cli) {
                            this.focus_session(&slug, sid, w, cx);
                        }
                    })
                })
            },
            // drill-in (docs/011 §B): no-op until the integrator wires
            // map_root re-rooting + breadcrumbs here.
            drill: {
                let ent = ent.clone();
                let slug = slug.clone();
                Rc::new(move |id, _w, app| {
                    ent.update(app, |this, cx| {
                        // a badge-drill can land with an editor still live (its
                        // mousedown never reaches node_down) — blur-commit so
                        // the re-rooted canvas can't strand an invisible
                        // key-eating editor (the 1b key-sink rule).
                        this.blur_outline_edit(cx);
                        // click-2 of the double — the click-1 outline-open must
                        // NOT fire under the drilled canvas.
                        this.outline_open_pending = None;
                        this.set_map_root(&slug, Some(id));
                        this.focused_part = Some(id);
                        cx.notify();
                    })
                })
            },
            // right-click (docs/019 CANVAS): the context menu, on every node.
            context_menu: {
                let ent = ent.clone();
                Rc::new(move |id, at, _w, app| {
                    ent.update(app, |this, cx| {
                        this.blur_outline_edit(cx);
                        // a right-press mid-left-drag: the menu wins, the drag
                        // dies unpersisted (nothing moved by then anyway).
                        this.map_drag = None;
                        this.map_drop_deny.clear();
                        // one selection shared between canvas and outline
                        // (docs/019) — the menu's verbs act on what you see.
                        this.focused_part = Some(id);
                        this.outline_open_pending = None;
                        this.map_menu = Some(MapMenu {
                            id,
                            at,
                            pane: MenuPane::Root,
                        });
                        cx.notify();
                    });
                })
            },
            // dbl-click a gen-0 TITLE (docs/019 CANVAS): rename in situ.
            rename: {
                let ent = ent.clone();
                Rc::new(move |id, w, app| ent.update(app, |this, cx| this.menu_rename(id, w, cx)))
            },
            // dbl-click empty canvas (docs/019 CANVAS): create-at-point —
            // pin captured NOW (normalized against this frame), node created
            // at name-commit so Esc leaves no half-named ghost behind.
            bg_create: {
                let ent = ent.clone();
                Rc::new(move |pt, w, app| {
                    ent.update(app, |this, cx| {
                        this.blur_outline_edit(cx);
                        let (dw, dh) = mapview::DEFAULT_CARD;
                        // center the fresh card on the click point; px_to_norm
                        // clamps edge clicks back inside the canvas.
                        let nx = mapview::px_to_norm(pt.0 - dw / 2.0, cw, dw);
                        let ny = mapview::px_to_norm(pt.1 - dh / 2.0, ch, dh);
                        this.canvas_create_pin = Some((nx, ny));
                        this.begin_outline_edit(outlinepane::EditSlot::CreateCanvas, w, cx);
                    });
                })
            },
        };
        // ---- live layer (docs/011 §D): chips paint DECLARED linkage only ----
        // dispatch rows ⋈ live infos; never inferred attribution. Un-linked
        // live sessions get the muted honesty line, not guessed chips.
        let dmap = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let gen = store.write_gen();
            let mut memo = self.dispatch_memo.borrow_mut();
            if memo.0 != gen || memo.1 != slug {
                *memo = (gen, slug.clone(), store.session_dispatch_map(&slug));
            }
            memo.2.clone()
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut needs: HashSet<PartId> = HashSet::new();
        // docs/019 slice 4 (ONE SUMMONS): how long each node's oldest awaiting
        // session has waited — the summons picks the OLDEST needs-you first.
        let mut needs_ages: HashMap<PartId, u64> = HashMap::new();
        let mut agents: HashMap<PartId, mapview::AgentChip> = HashMap::new();
        let mut unlinked = 0usize;
        for i in self.cached_infos(&slug).iter().filter(|i| i.alive) {
            // shells (no cli id ever) and pre-discovery codex can't be linked —
            // counting them makes the honesty line unactionable (review).
            let Some(cli_ref) = i.cli_session_id.as_ref() else {
                continue;
            };
            let Some(pid) = dmap.get(cli_ref).copied() else {
                unlinked += 1;
                continue;
            };
            let cli = i.cli_session_id.clone().unwrap_or_default();
            use orchestrator_host::Phase;
            if i.phase == Phase::AwaitingDecision {
                needs.insert(pid);
                let age = now_ms.saturating_sub(i.phase_since_ms) / 1000;
                needs_ages
                    .entry(pid)
                    .and_modify(|a| *a = (*a).max(age))
                    .or_insert(age);
            }
            let busy = matches!(i.phase, Phase::Busy | Phase::Spawning);
            let mins = now_ms.saturating_sub(i.phase_since_ms) / 60_000;
            let label = if mins > 0 {
                format!("@{} · {}m", i.kind.label(), mins)
            } else {
                format!("@{}", i.kind.label())
            };
            match agents.entry(pid) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let c = e.get_mut();
                    c.extra += 1;
                    // show the actionable one: idle/awaiting beats working
                    if c.busy && !busy {
                        c.cli_id = cli;
                        c.label = label;
                        c.busy = false;
                    }
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(mapview::AgentChip::dispatch(cli, label, busy));
                }
            }
        }
        // ---- observed-touch + derived-building overlays (docs/019 slice 3) ----
        // ONE session_part snapshot: dispatch/declared recency (building) and
        // observed-touch mass (hollow chips) both read from it.
        let now_secs = now_ms / 1000;
        let sparts = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store.session_parts(&slug)
        };
        let alive_clis: HashSet<String> = self
            .cached_infos(&slug)
            .iter()
            .filter(|i| i.alive)
            .filter_map(|i| i.cli_session_id.clone())
            .collect();
        // DECLARED chip follows the session (review finding 8): a live session
        // that did `map here B` gets its chip on B too — building already lights
        // there, so the chip must not be left behind on the dispatch node. Only
        // when no dispatch chip already covers B (dispatch outranks declared).
        {
            let infos = self.cached_infos(&slug);
            for r in sparts
                .iter()
                .filter(|r| r.role == "declared" && alive_clis.contains(&r.cli_session_id))
            {
                if agents.contains_key(&r.part_id) {
                    continue;
                }
                if let Some(i) = infos.iter().find(|i| {
                    i.alive && i.cli_session_id.as_deref() == Some(r.cli_session_id.as_str())
                }) {
                    use orchestrator_host::Phase;
                    if i.phase == Phase::AwaitingDecision {
                        needs.insert(r.part_id);
                        let age = now_ms.saturating_sub(i.phase_since_ms) / 1000;
                        needs_ages
                            .entry(r.part_id)
                            .and_modify(|a| *a = (*a).max(age))
                            .or_insert(age);
                    }
                    let busy = matches!(i.phase, Phase::Busy | Phase::Spawning);
                    let mins = now_ms.saturating_sub(i.phase_since_ms) / 60_000;
                    let label = if mins > 0 {
                        format!("@{} · {}m", i.kind.label(), mins)
                    } else {
                        format!("@{}", i.kind.label())
                    };
                    agents.insert(
                        r.part_id,
                        mapview::AgentChip::dispatch(r.cli_session_id.clone(), label, busy),
                    );
                }
            }
        }
        // DERIVED BUILDING (commitment 2): a node is building iff a dispatch|
        // declared link is alive (stamped `now`) or its own last activity <48h.
        // NEVER from a touch row — observation can't light ◔.
        let mut building_links: HashMap<PartId, Vec<(returnchannel::LinkRole, u64)>> =
            HashMap::new();
        for r in &sparts {
            let role = returnchannel::LinkRole::from_str(&r.role);
            if role.is_declared_intent() {
                // alive → stamped now; else the DISPATCHED session's own last
                // activity (last_touch_secs on THIS declared-intent row is that
                // session's work, not a foreign touch — the slice-3 <48h leg),
                // falling back to the dispatch time. A pure touch-ROLE row is
                // excluded above, so observation can never light ◔.
                let last = if alive_clis.contains(&r.cli_session_id) {
                    now_secs
                } else {
                    r.last_touch_secs.unwrap_or(r.at_secs)
                };
                building_links
                    .entry(r.part_id)
                    .or_default()
                    .push((role, last));
            }
        }
        let building: HashMap<PartId, u64> = building_links
            .iter()
            .filter_map(|(pid, links)| {
                returnchannel::derived_building(links, now_secs).map(|b| (*pid, b.age_secs))
            })
            .collect();
        // part activity (touch + note recency) — feeds the warm band below AND
        // the Next-up / gap-finder cockpit strip (docs/019 slice 4).
        let activity: HashMap<PartId, u64> = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store.part_activity(&slug)
        };
        // OBSERVED HOLLOW chips (docs/019 chips): a LIVE session's observed-touch
        // mass on a node above threshold — where it holds NO dispatch chip — paints
        // a hollow-dotted chip (never a solid one; observation ≠ intent). The
        // per-link distinct-file / in-anchor-duration axes aren't tracked yet, so
        // the WEIGHT axis (≥3.0 ≈ sustained real work) carries promotion here.
        for r in &sparts {
            if !alive_clis.contains(&r.cli_session_id) {
                continue;
            }
            let role = returnchannel::LinkRole::from_str(&r.role);
            if returnchannel::chip_tier(role, r.weight, 0, 0)
                != returnchannel::ChipTier::ObservedHollow
            {
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(v) = agents.entry(r.part_id) {
                let info = self
                    .cached_infos(&slug)
                    .iter()
                    .find(|i| i.cli_session_id.as_deref() == Some(r.cli_session_id.as_str()));
                let busy = info
                    .map(|i| {
                        matches!(
                            i.phase,
                            orchestrator_host::Phase::Busy | orchestrator_host::Phase::Spawning
                        )
                    })
                    .unwrap_or(false);
                let label = format!(
                    "@{} touching",
                    info.map(|i| i.kind.label()).unwrap_or("agent")
                );
                let mut chip = mapview::AgentChip::dispatch(r.cli_session_id.clone(), label, busy);
                chip.hollow = true;
                v.insert(chip);
            }
        }
        // HEARTBEAT GATE (commitment 4): the observe tick stamps `last_beat_ms`;
        // if it has gone stale (daemon/loop lost), wash every chip grey rather
        // than paint a lying alive dot — an honest empty beats a lie.
        let link_lost = returnchannel::heartbeat_stale(
            self.last_beat_ms,
            now_ms,
            returnchannel::HEARTBEAT_GRACE_MS,
        );
        if link_lost {
            for c in agents.values_mut() {
                c.washed = true;
            }
        }
        // WARM BAND (docs/019 slice 4 resumption): a node touched within 7d that
        // is NOT currently live (no building glyph AND no chip of any kind)
        // renders a dashed "◌ seen Nd ago" marker so "where was I" survives the
        // 48h live window. Computed AFTER every chip lands so a node the user
        // is actively touching reads as live, never as a cold resumption trail.
        let warm: HashMap<PartId, u64> = parts
            .iter()
            .filter_map(|p| {
                let last = activity.get(&p.id).copied().unwrap_or(0);
                let live = building.contains_key(&p.id) || agents.contains_key(&p.id);
                cockpit::warm_band(last, now_secs, live).map(|w| (p.id, w.age_secs))
            })
            .collect();
        // ---- TRUTH METER (docs/019 commitment 4): upstream events vs derived
        // summaries — "evidence thru HH:MM" (green) or "map is blind since <t>"
        // (amber). NOT queue depth — the exact non-enqueue that killed v2 reads
        // blind. Rides the tap-health + dead-job + link-lost honesty signals.
        let (meter, dead_jobs) = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            // PER-SESSION fold: a project is blind if ANY session is behind, so
            // one session's fresh summary can't mask another's staleness (review
            // finding 5). Each session's events-behind is (its events past its
            // own last summary).
            let per: Vec<(Option<u64>, Option<u64>, u64)> = store
                .project_session_freshness(&slug)
                .into_iter()
                .map(|(ev, sum)| {
                    let behind = match sum {
                        Some(s) if ev > s => 1, // at least one event past the summary; exact count not needed for blindness
                        None => 1,
                        _ => 0,
                    };
                    (Some(ev), sum, behind)
                })
                .collect();
            let m = returnchannel::project_truth_meter(&per, returnchannel::TRUTH_GRACE_MS);
            // RECENT deaths only — the same 24h window the Standup's warning uses.
            // Counting every dead row ever made this badge un-clearable: the
            // user's 20 deaths from the Jul 10-11 outage would have pinned an
            // amber "8 summary jobs failed" on orchestrator forever, long after
            // the pipeline healed. A failure light that never goes out is one the
            // user learns to ignore — which is how the dead standup survived
            // two days unnoticed. The death stamp exists for exactly this.
            let day_ago = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
                .saturating_sub(returnchannel::FAILURE_WINDOW_SECS * 1000);
            let dead = store
                .dead_summary_jobs()
                .into_iter()
                .filter(|(_, _, pk, _, died_ms)| pk == &slug && *died_ms >= day_ago)
                .count();
            (m, dead)
        };
        let truth_current = matches!(meter, returnchannel::TruthMeter::Current { .. });
        let truth_text = returnchannel::truth_meter_line(&meter, |ms: u64| {
            let secs = (ms / 1000) as i64 + orchestrator_host::host::local_off_secs();
            format!(
                "{:02}:{:02}",
                secs.rem_euclid(86400) / 3600,
                (secs.rem_euclid(86400) % 3600) / 60
            )
        });
        // tap-blind is a conservative honesty signal (review finding 7: global
        // cumulative counters made early exploration / an anchorless project
        // trip a false alarm). Only alarm when the project HAS anchored parts
        // (touches SHOULD land) and hundreds of tool events produced zero rows —
        // a genuine parser break, never normal use.
        let has_anchors = parts.iter().any(|p| !p.anchors.is_empty());
        let tap_blind = has_anchors
            && matches!(
                returnchannel::tap_health(self.tap_events_seen, self.tap_rows_written, 200),
                returnchannel::TapHealth::Blind { .. }
            );
        let truth_line = {
            let dot = if truth_current { GREEN } else { AMBER };
            let mut row = div()
                .id("map-truth")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(7.))
                .mx(px(14.))
                .mt(px(8.))
                .child(
                    div()
                        .flex_none()
                        .w(px(6.))
                        .h(px(6.))
                        .rounded(px(3.))
                        .bg(rgb(dot)),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .font_family("Menlo")
                        .text_color(rgb(if truth_current { MUTED } else { AMBER }))
                        .child(SharedString::from(truth_text)),
                );
            if dead_jobs > 0 {
                // same threshold AND same words as the Standup's warning, so the
                // two surfaces can never disagree about whether it is broken now.
                row = row.child(div().text_size(px(11.)).text_color(rgb(AMBER)).child(
                    SharedString::from(format!(
                        "· {dead_jobs} summar{} failed in the last day",
                        if dead_jobs == 1 { "y" } else { "ies" }
                    )),
                ));
            }
            if tap_blind {
                row = row.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(AMBER))
                        .child("· touch tap silent — parser may be broken"),
                );
            }
            if link_lost {
                row = row.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(MUTED2))
                        .child("· live link lost"),
                );
            }
            row
        };
        let unlinked_line = (unlinked > 0).then(|| {
            let uslug = slug.clone();
            div()
                .id("map-unlinked")
                .mx(px(14.))
                .mt(px(8.))
                .text_size(px(11.5))
                .text_color(rgb(MUTED2))
                .cursor_pointer()
                .hover(|h| h.text_color(rgb(MUTED)))
                .child(SharedString::from(format!(
                    "{unlinked} live session{} not on the map — link from a node's outline",
                    if unlinked == 1 { " is" } else { "s are" }
                )))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.focus_hottest(&uslug, window, cx);
                    cx.notify();
                }))
        });
        // proposal-bearing nodes bubble through the rollup even when hidden
        // behind a drill/badge (docs/011 slice 2) — every singleton pending
        // op's target id counts. Changeset-linked rows (changeset_id set) are
        // the grouped review's job, not per-node badges (docs/019 slice 1c).
        let prop_targets: HashSet<PartId> = pending
            .iter()
            .filter(|pd| pd.kind != "seed" && pd.changeset_id.is_none())
            .flat_map(|pd| pd.ops.iter())
            .filter_map(|op| match op {
                DiffOp::SetStatus { id, .. }
                | DiffOp::Rename { id, .. }
                | DiffOp::Remove { id }
                | DiffOp::Move { id, .. } => Some(*id),
                DiffOp::Add {
                    parent: PartRef::Id(id),
                    ..
                } => Some(*id),
                _ => None,
            })
            .collect();
        // the live ⌥-drag's drop target — the accent "release to move under
        // me" ring (docs/019 CANVAS); None the instant Option is released.
        let drop_target = self.map_drag.filter(|d| d.alt).and_then(|d| d.target);
        // the ONE canvas-side inline editor (rename-in-situ at the node's
        // rect / create-at-point at the pending pin) — pure display in
        // mapview, buffer + keys in the root router like every inline edit.
        let canvas_input: Option<mapview::CanvasInput> = match self.outline_edit.active {
            Some(outlinepane::EditSlot::RenameCanvas(id)) => {
                pos_by_id.get(&id).map(|p| mapview::CanvasInput {
                    x: p.x,
                    y: p.y,
                    w: p.w,
                    buf: self.outline_edit.buf.clone(),
                    hint: "⏎ save · esc",
                })
            }
            Some(outlinepane::EditSlot::CreateCanvas) => self.canvas_create_pin.map(|(nx, ny)| {
                let (dw, dh) = mapview::DEFAULT_CARD;
                mapview::CanvasInput {
                    x: mapview::norm_to_px(nx, canvas_w, dw),
                    y: mapview::norm_to_px(ny, canvas_h, dh),
                    w: dw,
                    buf: self.outline_edit.buf.clone(),
                    hint: "⏎ create · esc",
                }
            }),
            _ => None,
        };
        let canvas = mapview::map_canvas(
            view_tree,
            &positions,
            focused_id,
            &needs,
            &agents,
            &building,
            &warm,
            &prop_targets,
            drop_target,
            canvas_input.as_ref(),
            canvas_w,
            canvas_h,
            &map_handlers,
        );

        // ---- proposals banner: sessions proposed updates — with receipts ----
        // singleton proposals only; changeset rows have their own review card.
        let prop_count: usize = pending
            .iter()
            .filter(|pd| pd.kind != "seed" && pd.changeset_id.is_none())
            .map(|pd| pd.ops.len())
            .sum();
        let prop_banner = (prop_count > 0).then(|| {
            let first_target: Option<PartId> = pending
                .iter()
                .filter(|pd| pd.kind != "seed" && pd.changeset_id.is_none())
                .flat_map(|pd| pd.ops.iter())
                .find_map(|op| match op {
                    DiffOp::SetStatus { id, .. }
                    | DiffOp::Rename { id, .. }
                    | DiffOp::Remove { id }
                    | DiffOp::Move { id, .. } => Some(*id),
                    DiffOp::Add {
                        parent: PartRef::Id(id),
                        ..
                    } => Some(*id),
                    _ => None,
                });
            div()
                .id("prop-banner")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .mx(px(14.))
                .mt(px(10.))
                .px(px(13.))
                .py(px(8.))
                .rounded(px(10.))
                .bg(rgb(0x1c1a12))
                .border_1()
                .border_color(rgb(0x4a4530))
                .cursor_pointer()
                .hover(|h| h.border_color(rgb(0x6b5a38)))
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(AMBER))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(SharedString::from(format!(
                            "▲ {prop_count} proposed map update{}",
                            if prop_count == 1 { "" } else { "s" }
                        ))),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(MUTED))
                        .child("each carries its evidence — review on the node ▸"),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if let Some(id) = first_target {
                        let slug = this.project().slug.clone();
                        this.focus_node_on_map(&slug, id, cx);
                    }
                    cx.notify();
                }))
        });

        // (docs/019: the one-time "Adopt product layout" banner is DEAD — it
        // was the bulk apply that built the unexplainable Tech blob. Structure
        // repair now flows through changesets, e.g. the canned Dissolve-Tech
        // card in slice 1c.)

        // ---- outline pane (collapsible state of the one Map mode, docs/011 §A) ----
        let live_sessions: Vec<(String, String)> = self
            .cached_infos(&slug)
            .iter()
            .filter(|i| i.alive)
            .filter_map(|i| {
                i.cli_session_id
                    .clone()
                    .map(|c| (c, termview::session_label(i)))
            })
            .collect();
        let outline: Option<AnyElement> = outline_open.then(|| fp_node).flatten().map(|node| {
            let now_secs = orchestrator_core::registry::now_secs();
            let notes = focused_id
                .and_then(|id| {
                    self.store
                        .lock()
                        .ok()
                        .and_then(|s| s.notes_for_part(id).ok())
                })
                .unwrap_or_default();
            let fp_id = node.part.id;
            let pending_for_node: Vec<(DiffOp, Option<String>)> = pending
                .iter()
                // singleton proposals only — changeset-linked rows surface
                // through the grouped review card, never per-op here.
                .filter(|pd| pd.kind != "seed" && pd.changeset_id.is_none())
                // Temp-parent ops only make sense applied with their WHOLE
                // diff (the temp map is per-accept) — a lone accept would
                // silently reparent to root (review 1b). evidence is
                // index-aligned with ops (store invariant), so zip is safe.
                .flat_map(|pd| {
                    pd.ops
                        .iter()
                        .zip(pd.evidence.iter())
                        .filter(|(op, _)| op_touches(op, fp_id) && !has_temp_ref(op))
                        .map(|(op, ev)| (op.clone(), ev.clone()))
                        .collect::<Vec<_>>()
                })
                .collect();
            let ops_snapshot: Vec<DiffOp> =
                pending_for_node.iter().map(|(op, _)| op.clone()).collect();
            let name_of = {
                let names: HashMap<PartId, String> =
                    parts.iter().map(|p| (p.id, p.name.clone())).collect();
                move |pid: PartId| {
                    names
                        .get(&pid)
                        .cloned()
                        .unwrap_or_else(|| format!("#{pid}"))
                }
            };
            // ancestry contextualizes a drilled/satellite focus: names of
            // ancestors, root-first, excluding the node itself.
            let ancestry: String = {
                let by_id: HashMap<PartId, &DesignPart> = parts.iter().map(|p| (p.id, p)).collect();
                let mut names = Vec::new();
                let mut cur = by_id.get(&fp_id).and_then(|p| p.parent_id);
                while let Some(id) = cur {
                    let Some(p) = by_id.get(&id) else { break };
                    names.push(p.name.clone());
                    cur = p.parent_id;
                }
                names.reverse();
                names.join(" ▸ ")
            };
            // the node's session trail (docs/011 slice 3): dispatch rows
            // joined against live infos; ended rows keep their last summary
            // headline; touch rows render as "also touched".
            let sessions: Vec<outlinepane::SessionRow> = {
                let rows = {
                    let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                    store.sessions_for_part(fp_id)
                };
                let infos = self.cached_infos(&slug);
                rows.iter()
                    .take(8)
                    .map(|(cli, role, _at)| {
                        let info = infos
                            .iter()
                            .find(|i| i.cli_session_id.as_deref() == Some(cli.as_str()) && i.alive);
                        let phase = info
                            .map(|i| match i.phase {
                                orchestrator_host::Phase::AwaitingDecision => "needs you",
                                orchestrator_host::Phase::Busy
                                | orchestrator_host::Phase::Spawning => "working",
                                _ => "your turn",
                            })
                            .unwrap_or("")
                            .to_string();
                        outlinepane::SessionRow {
                            cli_id: cli.clone(),
                            label: info
                                .map(termview::session_label)
                                .unwrap_or_else(|| outlinepane::id8(cli)),
                            live: info.is_some(),
                            phase,
                            headline: self
                                .sess_summaries
                                .get(cli)
                                .map(|r| r.headline.clone())
                                .unwrap_or_default(),
                            // whisper-grade rows (docs/019): observed touches AND demoted
                            // trail rows — a relinked-away session must not keep rendering
                            // as if still dispatched here. 'declared' stays intent-grade.
                            touched: role == "touch" || role == "trail",
                        }
                    })
                    .collect()
            };
            let breaking_down = self
                .breakdown_inflight
                .lock()
                .map(|g| g.contains(&fp_id))
                .unwrap_or(false);
            let e = cx.entity();
            let oh = outlinepane::OutlineHandlers {
                focus_child: Rc::new({
                    let e = e.clone();
                    move |id, _w, app| {
                        e.update(app, |this, cx| {
                            // blur-commit first: a live Detail edit must
                            // survive the refocus (docs/019 save-on-blur).
                            this.blur_outline_edit(cx);
                            this.focused_part = Some(id);
                            this.outline_link_open = false;
                            cx.notify();
                        })
                    }
                }),
                cycle_status: Rc::new({
                    let e = e.clone();
                    move |id, next, _w, app| {
                        e.update(app, |this, cx| this.set_part_status(id, next, cx))
                    }
                }),
                begin_edit: Rc::new({
                    let e = e.clone();
                    // one shared entry (docs/019 slice 1b): prefills
                    // rename/detail slots, opens the pane, hands the
                    // keystream to the root router.
                    move |slot, w, app| {
                        e.update(app, |this, cx| this.begin_outline_edit(slot, w, cx))
                    }
                }),
                set_kind: Rc::new({
                    let e = e.clone();
                    // the kind chip (docs/019): instant human-lane SetKind
                    // — the pane hands us the already-cycled next kind.
                    move |id, kind, _w, app| {
                        e.update(app, |this, cx| {
                            let slug = this.project().slug.clone();
                            let mut store = this.store.lock().unwrap_or_else(|er| er.into_inner());
                            let _ = store.accept_diff_from(
                                &slug,
                                &[DiffOp::SetKind { id, kind }],
                                "user",
                                None,
                            );
                            drop(store);
                            cx.notify();
                        })
                    }
                }),
                accept_op: Rc::new({
                    let e = e.clone();
                    let ops = ops_snapshot.clone();
                    // resolve by VALUE, not index — a stale frame's index
                    // lands on the wrong op after a prior accept (review 1b)
                    move |ix, _w, app| {
                        if let Some(op) = ops.get(ix).cloned() {
                            e.update(app, |this, cx| this.resolve_outline_op(&op, true, cx));
                        }
                    }
                }),
                dismiss_op: Rc::new({
                    let e = e.clone();
                    let ops = ops_snapshot.clone();
                    move |ix, _w, app| {
                        if let Some(op) = ops.get(ix).cloned() {
                            e.update(app, |this, cx| this.resolve_outline_op(&op, false, cx));
                        }
                    }
                }),
                open_full_log: Rc::new(move |_w, _app| {}),
                dispatch: Rc::new({
                    let e = e.clone();
                    move |id, alt, w, app| {
                        e.update(app, |this, cx| this.dispatch_to_part(id, alt, w, cx))
                    }
                }),
                // inert until propose_breakdown wires in (docs/011 slice 3)
                break_down: Rc::new({
                    let e = e.clone();
                    move |pid, _w, app| e.update(app, |this, cx| this.spawn_breakdown(pid, cx))
                }),
                open_session: Rc::new({
                    let e = e.clone();
                    move |cli, w, app| {
                        e.update(app, |this, cx| {
                            if let Some((slug, sid)) = this.find_live_by_cli_id(&cli) {
                                this.focus_session(&slug, sid, w, cx);
                            }
                        })
                    }
                }),
                link_session: Rc::new({
                    let e = e.clone();
                    let lslug = slug.clone();
                    // retro-link declares intent: the dispatch role, ≤1 per
                    // session (relink enforces it — never pre-delete here).
                    move |pid, cli, _w, app| {
                        e.update(app, |this, cx| {
                            if let Ok(store) = this.store.lock() {
                                let _ = store.relink_session_part(&cli, pid, &lslug);
                            }
                            this.outline_link_open = false;
                            cx.notify();
                        })
                    }
                }),
                toggle_link: Rc::new({
                    let e = e.clone();
                    move |_w, app| {
                        e.update(app, |this, cx| {
                            this.outline_link_open = !this.outline_link_open;
                            cx.notify();
                        })
                    }
                }),
            };
            let collapse = {
                let cslug = slug.clone();
                div()
                    .id("outline-collapse")
                    .absolute()
                    .top(px(8.))
                    .right(px(10.))
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED2))
                    .hover(|h| h.text_color(rgb(ACCENT)).bg(rgb(CARD)))
                    .child("⟩")
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.set_outline_open(&cslug, false);
                        cx.notify();
                    }))
            };
            // chevron is a sibling of the SCROLLED div, not a child — gpui
            // scroll offsets absolute children too, and it would ride away
            // with a long decision log (review slice 1).
            div()
                .w(px(400.))
                .flex_none()
                .min_h_0()
                .relative()
                .border_l_1()
                .border_color(rgb(HAIR_SOFT))
                .child(
                    div()
                        .id("outline-col")
                        .size_full()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(outlinepane::outline_pane(
                            node,
                            &notes,
                            &pending_for_node,
                            &self.outline_edit,
                            self.inline_caret,
                            &live_sessions,
                            self.outline_link_open,
                            &sessions,
                            &ancestry,
                            breaking_down,
                            now_secs,
                            &name_of,
                            &oh,
                        )),
                )
                .child(collapse)
                .into_any_element()
        });

        let mut body = div().flex_1().min_h_0().flex().flex_row();
        body = body.child(
            div()
                .id("map-scroll")
                .flex_1()
                .min_w_0()
                .min_h_0()
                .overflow_y_scroll()
                .overflow_x_scroll() // narrow windows must reach far-right nodes (review 1b)
                .child(canvas),
        );
        if let Some(o) = outline {
            body = body.child(o);
        } else {
            let rslug = slug.clone();
            body = body.child(
                div()
                    .id("outline-reopen")
                    .w(px(20.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .border_l_1()
                    .border_color(rgb(HAIR_SOFT))
                    .text_size(px(12.))
                    .text_color(rgb(MUTED2))
                    .hover(|h| h.text_color(rgb(ACCENT)).bg(rgb(PANEL)))
                    .child("⟨")
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.set_outline_open(&rslug, true);
                        cx.notify();
                    })),
            );
        }
        let mut root = div().flex_1().min_h_0().flex().flex_col();
        // ---- canvas toolbar (docs/019 GUI-first, ruling 13): the PERMANENT
        // "⟳ Re-ground" verb (the empty-tree-only CTA is dead) — Claude re-reads
        // the docs & code and proposes a cited DELTA changeset. Mouse-reachable;
        // the cmd bar's "re-ground" is the typed accelerator.
        {
            let reground_slug = slug.clone();
            let seeding = self
                .agentic
                .as_ref()
                .is_some_and(|r| r.slug == reground_slug && r.scope.is_none());
            root = root.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(px(6.))
                    .mx(px(14.))
                    .mt(px(8.))
                    .child(
                        div()
                            .id("map-reground")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(8.))
                            .cursor_pointer()
                            .border_1()
                            .border_color(rgb(if seeding { 0x3a4a55 } else { HAIR }))
                            .hover(|h| h.border_color(rgb(0x3a4a55)))
                            .text_size(px(11.5))
                            .text_color(rgb(if seeding { MUTED2 } else { MUTED }))
                            .child(if seeding {
                                "⟳ Re-grounding…".to_string()
                            } else {
                                "⟳ Re-ground from docs".to_string()
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.term_error = None;
                                this.start_agentic_run(AgenticKind::Seed { intent: None }, cx);
                            })),
                    ),
            );
        }
        // ---- breadcrumb (drilled canvases only): project ▸ … ▸ current root ----
        // Attention must survive the drill (docs/011 §B): the drilled root's
        // OWN state and anything OUTSIDE its subtree get markers up here —
        // the canvas below can't show either.
        let (root_attn, elsewhere_needs, elsewhere_live) = if let Some(rid) = map_root {
            let mut subtree: HashSet<PartId> = HashSet::new();
            if let Some(rn) = find_in(tree, rid) {
                fn collect(n: &TreeNode, out: &mut HashSet<PartId>) {
                    out.insert(n.part.id);
                    for c in &n.children {
                        collect(c, out);
                    }
                }
                collect(rn, &mut subtree);
            }
            let root_attn = (needs.contains(&rid), agents.contains_key(&rid));
            let en = needs.iter().filter(|id| !subtree.contains(id)).count();
            let el = agents
                .keys()
                .filter(|id| !subtree.contains(id) && !needs.contains(id))
                .count();
            (root_attn, en, el)
        } else {
            ((false, false), 0, 0)
        };
        if !crumbs.is_empty() {
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .mx(px(14.))
                .mt(px(8.))
                .text_size(px(11.5));
            let hslug = slug.clone();
            row = row.child(
                div()
                    .id("crumb-root")
                    .cursor_pointer()
                    .text_color(rgb(MUTED2))
                    .hover(|h| h.text_color(rgb(ACCENT)))
                    .child(SharedString::from(self.project().name.clone()))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.set_map_root(&hslug, None);
                        cx.notify();
                    })),
            );
            let last = crumbs.len() - 1;
            for (ix, (cid, cname)) in crumbs.iter().enumerate() {
                row = row.child(div().text_color(rgb(MUTED2)).child("▸"));
                if ix == last {
                    row = row.child(
                        div()
                            .text_color(rgb(TEXT))
                            .child(SharedString::from(cname.clone())),
                    );
                } else {
                    let cslug = slug.clone();
                    let cid = *cid;
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("crumb-{cid}")))
                            .cursor_pointer()
                            .text_color(rgb(MUTED2))
                            .hover(|h| h.text_color(rgb(ACCENT)))
                            .child(SharedString::from(cname.clone()))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.set_map_root(&cslug, Some(cid));
                                cx.notify();
                            })),
                    );
                }
            }
            if root_attn.0 {
                row = row.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(AMBER))
                        .child("⚠ this node needs you"),
                );
            } else if root_attn.1 {
                row = row.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0x5BB99B))
                        .child("● live here"),
                );
            }
            row = row.child(div().flex_1());
            if elsewhere_needs > 0 || elsewhere_live > 0 {
                let label = if elsewhere_needs > 0 {
                    format!("⚠ {elsewhere_needs} elsewhere")
                } else {
                    format!("● {elsewhere_live} live elsewhere")
                };
                let eslug = slug.clone();
                row = row.child(
                    div()
                        .id("crumb-elsewhere")
                        .px(px(7.))
                        .py(px(2.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_size(px(11.))
                        .text_color(rgb(if elsewhere_needs > 0 { AMBER } else { 0x5BB99B }))
                        .hover(|h| h.bg(rgb(CARD)))
                        .child(SharedString::from(label))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.set_map_root(&eslug, None);
                            cx.notify();
                        })),
                );
            }
            row = row.child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgb(MUTED2))
                    .child("⌫ up"),
            );
            root = root.child(row);
        }
        // the OPEN changeset (docs/019 slice 1c): the machine proposal the
        // user is reviewing, rendered as a diff-of-the-document card above
        // the canvas. One at a time; its OPS ride the pending rows already
        // loaded (filtered by changeset_id below), but the changeset METADATA
        // is one small indexed lookup per frame — cheap on a calm map; memoize
        // against write_gen if it ever shows up in a profile (review finding 7).
        let open_cs = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let opens = store.open_changesets(&slug);
            // prefer the changeset the user is actively reviewing — an
            // explicit context-menu Dissolve arms self.review on the NEW one,
            // which must surface immediately even if an older card is still
            // open (review finding 4); else fall back to the oldest open.
            let reviewing = self.review.as_ref().map(|r| r.id);
            reviewing
                .and_then(|rid| opens.iter().find(|c| c.0 == rid).cloned())
                .or_else(|| opens.into_iter().next())
        };
        // ---- THE COCKPIT (docs/019 slice 4, C6): the glance strip. Sits high
        // on the map — what needs me · what's moving · where was I · what to
        // start — in PLAIN WORDS. The user-set needs-you input bar and the
        // triage sweep banner ride above it when active; the ONE SUMMONS pulse,
        // the rollup line, the Next-up tray, and the suggestions drawer follow.
        let review_summons: Option<(i64, String)> = open_cs.as_ref().map(|c| (c.0, c.1.clone()));
        let drifted_ids: Vec<PartId> = agents
            .iter()
            .filter(|(_, c)| c.hollow)
            .map(|(id, _)| *id)
            .collect();
        if let Some(bar) = self.render_needs_you_bar(cx) {
            root = root.child(bar);
        }
        if self.triage_active {
            root = root.child(self.render_triage_banner(cx));
        }
        root = root.child(self.render_cockpit_strip(
            &slug,
            parts,
            tree,
            &building,
            &drifted_ids,
            &activity,
            &needs,
            &needs_ages,
            now_secs,
            review_summons,
            cx,
        ));
        // docs/019 slice 2: while a user-invoked cartographer run is in
        // flight, a progress card sits where its changeset will land (a silent
        // multi-minute call reads as a hang).
        if let Some(card) = self.render_agentic_progress(&slug) {
            root = root.child(card);
        }
        if let Some(cs) = open_cs {
            let cs_rows: Vec<orchestrator_store::PendingDiff> = pending
                .iter()
                .filter(|pd| pd.changeset_id == Some(cs.0))
                .cloned()
                .collect();
            let flat = orchestrator_store::flatten_changeset_ops(&cs_rows);
            let flags = orchestrator_store::flatten_changeset_flags(&cs_rows);
            root = root.child(self.render_changeset_review(&cs, &flat, &flags, parts, cx));
        }
        // the truth meter rides just above the proposal/honesty lines — the
        // first thing the map says is how far behind it is (docs/019 C4).
        root = root.child(truth_line);
        if let Some(b) = prop_banner {
            root = root.child(b);
        }
        if let Some(u) = unlinked_line {
            root = root.child(u);
        }
        // spawn/dispatch errors now surface once at the workspace root (visible in
        // every mode incl. the OSS map-off Agent stage) — see term_error_banner.
        root.child(body)
    }

}
