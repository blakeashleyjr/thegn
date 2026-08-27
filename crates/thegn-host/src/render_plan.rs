//! The render decision, isolated as a pure function so it can be exhaustively
//! unit-tested in CI — the deterministic enforcement of the compositor's
//! performance invariants. Wall-clock benchmarks are machine-dependent and
//! excluded from `just ci`; these work-shape invariants are not:
//!
//! - an idle wake (no damage) ⇒ [`RenderPlan::Skip`] (the ~0%-idle invariant);
//! - pane output and/or a bars (stats/clock) tick and nothing else ⇒
//!   [`RenderPlan::Incremental`] — recompose + bounded-diff ONLY those regions,
//!   never the full chrome;
//! - any heavy-chrome/overlay/geometry change ⇒ [`RenderPlan::Full`].
//!
//! The event loop tracks the damage channels ([`Damage`]) and the set of live
//! overlays ([`Overlays`]); [`plan`] maps them to the cheapest correct frame.
//! See `run.rs` for the dispatch that executes the plan.

use crate::center::PaneId;
use std::collections::HashSet;

/// Per-frame damage: which classes of on-screen content changed since the last
/// flush. The loop sets the narrowest channel that applies; pure pane output
/// touches only [`Damage::panes`], leaving the expensive chrome untouched.
#[derive(Debug, Default, Clone)]
pub struct Damage {
    /// Geometry changed (resize, scratch realloc, panel/strip/drawer toggle):
    /// the whole screen is cleared, recomposed, and the diff baseline reset.
    pub full: bool,
    /// Heavy chrome / model state changed (sidebar tree, panel, tabbar, focus
    /// ring, hydration carrying real changes): recompose chrome + all panes.
    pub chrome: bool,
    /// A tab/worktree switch is awaiting its first frame (the loop's
    /// `switch_at` stamp is live). Forces a full frame like `chrome` — the
    /// whole center band + tabbar + panel changed — but as its own channel so
    /// (a) full frames attribute to switches vs hydration vs overlays in the
    /// perf rollup, and (b) a future switch-back blit plan can key off it
    /// without re-plumbing the loop.
    pub switch: bool,
    /// Pane content changed (PTY output): recompose + bounded-diff ONLY these.
    pub panes: HashSet<PaneId>,
    /// Only the masthead/statusbar bars changed — the high-frequency stats tick,
    /// the live clock, AI metrics. Recompose just those two 1-row rects and
    /// bounded-diff them, instead of a full-chrome repaint ~1×/s while idle.
    pub bars: bool,
    /// Only the sidebar changed — cursor navigation, collapse/expand, multi-select
    /// (D5). The panel shows the *active* worktree (not the sidebar highlight), so
    /// it's untouched; recompose + bounded-diff just the sidebar rect (paired with
    /// `bars` for any selection-count display) instead of the full chrome/panel.
    pub sidebar: bool,
}

#[allow(dead_code)] // is_empty/clear are part of the Damage API + exercised by tests
impl Damage {
    /// True when nothing changed — the loop woke but has no frame to paint.
    pub fn is_empty(&self) -> bool {
        !self.full
            && !self.chrome
            && !self.switch
            && !self.bars
            && !self.sidebar
            && self.panes.is_empty()
    }

    /// Clear all channels — called after a frame is flushed.
    pub fn clear(&mut self) {
        self.full = false;
        self.chrome = false;
        self.switch = false;
        self.bars = false;
        self.sidebar = false;
        self.panes.clear();
    }
}

/// Live overlays/interactions that composite ON TOP of the center band and so
/// would be erased by a pane-only recompose (which repaints a pane's rect over
/// whatever the prior full frame left there). Any of these forces a full frame.
///
/// This used to be one bool per popup, hand-maintained at the construction site
/// in `run.rs` — and it drifted, silently: help, the PR and diff views, replay
/// and the rollback modal were all missing, so PTY output underneath them
/// punched holes straight through. `layers` replaces eleven of those bits with a
/// fact recorded by `layer::open_layer` itself (via [`crate::caret`]), so a
/// popup added later is covered without anyone remembering this struct exists.
/// Only the things that are *not* boxed layers are still named individually.
///
/// The drawer is deliberately absent: it's a reserved, disjoint panel rect, not
/// an overlay over a pane, so streaming output beside an open drawer still take
/// the fast pane-only path.
#[derive(Debug, Default, Clone, Copy)]
pub struct Overlays {
    /// A boxed layer painted over the band on the last full frame — every
    /// popup, picker, wizard, menu, toast, hover card and modal at once.
    /// Derived, never hand-listed: see [`crate::caret::no_covers`].
    pub layers: bool,
    /// An embedded app composed IN PLACE OF the band, not a layer over it.
    pub app_tile: bool,
    /// A mouse copy-mode selection tint, painted onto pane cells.
    pub selection: bool,
    /// The replay scrubber: a raw grid blit with no box, so it registers its
    /// own cover rather than going through `open_layer`.
    pub replay: bool,
}

impl Overlays {
    /// True when some overlay is live and a pane-only frame would corrupt it.
    pub fn any(&self) -> bool {
        self.layers || self.app_tile || self.selection || self.replay
    }
}

/// What the renderer should do this frame — the cheapest correct option.
#[derive(Debug, PartialEq, Eq)]
pub enum RenderPlan {
    /// Nothing changed: skip the frame entirely (no compose, no diff, no flush).
    Skip,
    /// Recompose chrome + all panes and diff the whole screen. Covers geometry
    /// changes (with a clear + baseline reset, driven separately by the
    /// `full_repaint` flag) and any heavy-chrome/overlay change.
    Full,
    /// Reuse the prior frame in `scratch`; recompose + bounded-diff only the
    /// damaged regions — the named `panes` (sorted, deduped), the masthead+
    /// statusbar `bars`, and/or the `sidebar`. The streaming-output + stats-tick +
    /// sidebar-nav fast path. At least one of `panes`/`bars`/`sidebar` is set.
    Incremental {
        panes: Vec<PaneId>,
        bars: bool,
        sidebar: bool,
    },
}

/// Map this frame's damage + overlay state to the cheapest correct plan.
///
/// Precedence: geometry > heavy-chrome/overlays > pane/bars content > nothing. A
/// chrome or overlay change always wins (the full recompose repaints panes+bars
/// anyway, and a partial frame can't safely carry an overlay).
pub fn plan(damage: &Damage, overlays: &Overlays) -> RenderPlan {
    if damage.full {
        return RenderPlan::Full;
    }
    if damage.chrome || damage.switch || overlays.any() {
        return RenderPlan::Full;
    }
    if !damage.panes.is_empty() || damage.bars || damage.sidebar {
        let mut panes: Vec<PaneId> = damage.panes.iter().copied().collect();
        panes.sort_unstable();
        return RenderPlan::Incremental {
            panes,
            bars: damage.bars,
            sidebar: damage.sidebar,
        };
    }
    RenderPlan::Skip
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panes(ids: &[PaneId]) -> Damage {
        Damage {
            panes: ids.iter().copied().collect(),
            ..Default::default()
        }
    }

    #[test]
    fn idle_wake_skips() {
        assert_eq!(
            plan(&Damage::default(), &Overlays::default()),
            RenderPlan::Skip
        );
    }

    #[test]
    fn pure_pane_output_is_panes_only_never_chrome() {
        // The core active-CPU invariant: PTY output recomposes only its pane.
        assert_eq!(
            plan(&panes(&[3]), &Overlays::default()),
            RenderPlan::Incremental {
                panes: vec![3],
                bars: false,
                sidebar: false
            }
        );
        assert_eq!(
            plan(&panes(&[7, 2, 7, 4]), &Overlays::default()),
            RenderPlan::Incremental {
                panes: vec![2, 4, 7],
                bars: false,
                sidebar: false
            },
            "ids are sorted + deduped"
        );
    }

    /// The pipeline board's load-bearing render invariant (inherited from the
    /// superseded `add-fleet-view` design, which lost its metrics source but not
    /// this rule): **a roster update is a bounded diff, never a Full chrome
    /// recompose.**
    ///
    /// The board's data feed (`RefreshKind::Dispatches`) touches
    /// `FrameModel::dispatches` and nothing else, and its sidebar half touches
    /// only the stage tags — so the widest damage a roster change may raise is
    /// `sidebar` (plus, while the board is open, the overlay rule below). If a
    /// future change routes a roster sample through `damage.chrome`, this test
    /// is what fails.
    #[test]
    fn a_roster_update_is_a_bounded_diff_never_a_full_recompose() {
        // Sidebar stage tags moved and nothing else: sidebar-only diff.
        let d = Damage {
            sidebar: true,
            ..Default::default()
        };
        assert_eq!(
            plan(&d, &Overlays::default()),
            RenderPlan::Incremental {
                panes: vec![],
                bars: false,
                sidebar: true
            }
        );
        // A sample that found nothing new raises no damage at all — the idle
        // contract holds through the board's own feed.
        assert_eq!(
            plan(&Damage::default(), &Overlays::default()),
            RenderPlan::Skip
        );
        // A stage agent's pane streaming underneath stays a pane-only diff:
        // roster liveness must never drag chrome into a pane's frame.
        assert_eq!(
            plan(&panes(&[5]), &Overlays::default()),
            RenderPlan::Incremental {
                panes: vec![5],
                bars: false,
                sidebar: false
            }
        );
    }

    /// The board itself is a boxed layer, so while it is OPEN the pre-existing
    /// overlay rule governs every frame — the same contract the Containers tab
    /// it was cloned from lives under. Pinned here so the invariant above is
    /// read for what it is (about the FEED, not about the open modal) and so a
    /// change to the overlay rule is a deliberate one.
    #[test]
    fn an_open_board_takes_the_overlay_rule_like_every_other_modal() {
        let overlays = Overlays {
            layers: true,
            ..Default::default()
        };
        assert_eq!(plan(&Damage::default(), &overlays), RenderPlan::Full);
        assert_eq!(
            plan(
                &Damage {
                    sidebar: true,
                    ..Default::default()
                },
                &overlays
            ),
            RenderPlan::Full
        );
    }

    #[test]
    fn bars_only_tick_is_incremental_not_full() {
        // The idle-residual fix: a stats/clock tick recomposes only the bars.
        let d = Damage {
            bars: true,
            ..Default::default()
        };
        assert_eq!(
            plan(&d, &Overlays::default()),
            RenderPlan::Incremental {
                panes: vec![],
                bars: true,
                sidebar: false
            }
        );
    }

    #[test]
    fn bars_tick_under_an_open_detail_popup_is_full() {
        // The once-a-minute clock tick must not bounded-diff just the bars while
        // the date/clock calendar popup is up — the popup is painted over the
        // composed frame, so a bars-only recompose would leave it half-erased.
        // The popup reaches us as `layers`, set from what `open_layer` painted.
        let d = Damage {
            bars: true,
            ..Default::default()
        };
        assert_eq!(
            plan(
                &d,
                &Overlays {
                    layers: true,
                    ..Default::default()
                }
            ),
            RenderPlan::Full
        );
    }

    /// A weather delivery is the clock tick's damage class, not hydration's.
    ///
    /// `RefreshKind::Weather` touches `FrameModel::weather` and nothing else, so
    /// the loop raises `bars` — two 1-row rects. Routing it through `chrome`
    /// instead would turn a half-hourly datum into a half-hourly full-chrome
    /// repaint on an otherwise idle machine; this test is what fails if someone
    /// does. (The open-popup case is governed by the overlay rule, pinned by
    /// `bars_tick_under_an_open_detail_popup_is_full` just above.)
    #[test]
    fn a_weather_delivery_is_bars_only() {
        let d = Damage {
            bars: true,
            ..Default::default()
        };
        assert_eq!(
            plan(&d, &Overlays::default()),
            RenderPlan::Incremental {
                panes: vec![],
                bars: true,
                sidebar: false
            }
        );
        // A redelivery of an identical cached reading raises no damage at all —
        // the loop compares before it dirties — so the idle contract holds
        // through the weather feed too.
        assert_eq!(
            plan(&Damage::default(), &Overlays::default()),
            RenderPlan::Skip
        );
    }

    #[test]
    fn a_clock_tick_that_changed_nothing_still_skips() {
        // The ticker only sends ClockTick on a real display-boundary crossing,
        // but if the loop ever wakes with no damage the answer stays Skip — the
        // ~0%-idle contract is not weakened by having a clock.
        assert_eq!(
            plan(&Damage::default(), &Overlays::default()),
            RenderPlan::Skip
        );
    }

    #[test]
    fn pane_output_and_bars_tick_combine() {
        let mut d = panes(&[5]);
        d.bars = true;
        assert_eq!(
            plan(&d, &Overlays::default()),
            RenderPlan::Incremental {
                panes: vec![5],
                bars: true,
                sidebar: false
            }
        );
    }

    #[test]
    fn sidebar_only_change_is_incremental_not_full() {
        // D5: sidebar cursor-nav recomposes just the sidebar (+ bars), never the
        // full chrome/panel — the panel tracks the ACTIVE worktree, not the
        // sidebar highlight, so it's untouched.
        let d = Damage {
            sidebar: true,
            ..Default::default()
        };
        assert_eq!(
            plan(&d, &Overlays::default()),
            RenderPlan::Incremental {
                panes: vec![],
                bars: false,
                sidebar: true
            }
        );
        // A chrome/overlay change still escalates a sidebar-only frame to Full.
        let d2 = Damage {
            sidebar: true,
            chrome: true,
            ..Default::default()
        };
        assert_eq!(plan(&d2, &Overlays::default()), RenderPlan::Full);
    }

    #[test]
    fn chrome_change_forces_full_even_with_pane_or_bars() {
        let mut d = panes(&[1]);
        d.bars = true;
        d.chrome = true;
        assert_eq!(plan(&d, &Overlays::default()), RenderPlan::Full);
    }

    #[test]
    fn geometry_change_forces_full() {
        let mut d = panes(&[1]);
        d.full = true;
        assert_eq!(plan(&d, &Overlays::default()), RenderPlan::Full);
    }

    #[test]
    fn any_overlay_forces_full_over_pane_output() {
        // Each channel independently escalates a pane-only frame to full, so an
        // overlay painted over a pane is never silently erased. `layers` covers
        // every boxed popup at once — it is set from what `open_layer` actually
        // painted, so this can't drift the way the old per-popup bools did.
        let cases = [
            Overlays {
                layers: true,
                ..Default::default()
            },
            Overlays {
                app_tile: true,
                ..Default::default()
            },
            Overlays {
                selection: true,
                ..Default::default()
            },
            Overlays {
                replay: true,
                ..Default::default()
            },
        ];
        for ov in cases {
            assert!(ov.any());
            assert_eq!(plan(&panes(&[1]), &ov), RenderPlan::Full);
        }
    }

    #[test]
    fn a_layer_forces_full_for_every_content_channel() {
        // The regression this fix is for: help (or any popup) is open and a pane
        // streams output with no keystroke to mark chrome dirty. Before, that
        // took the Incremental path and repainted the pane straight through the
        // overlay box.
        let ov = Overlays {
            layers: true,
            ..Default::default()
        };
        for d in [
            panes(&[1]),
            Damage {
                bars: true,
                ..Default::default()
            },
            Damage {
                sidebar: true,
                ..Default::default()
            },
        ] {
            assert_eq!(plan(&d, &ov), RenderPlan::Full);
        }
        // Note on the ~0%-idle contract: an overlay escalates *content* damage,
        // and `overlays.any()` is checked ahead of the damage classes, so a
        // no-damage call with a layer open answers Full. That is unreachable in
        // the loop, which only calls `plan` behind `have_damage` — the idle
        // guarantee lives there, not here. `idle_wake_skips` pins the pure case.
    }

    #[test]
    fn switch_forces_full_frame() {
        // A tab/worktree switch swaps the whole center band + tabbar + panel:
        // its first frame is a full recompose, on its own damage channel (so
        // the perf rollup attributes it and a future blit plan can hook it).
        let d = Damage {
            switch: true,
            ..Default::default()
        };
        assert_eq!(plan(&d, &Overlays::default()), RenderPlan::Full);
    }

    #[test]
    fn switch_with_pane_damage_still_full() {
        // Pane output racing the switch frame can't demote it to Incremental.
        let mut d = panes(&[2, 9]);
        d.switch = true;
        d.bars = true;
        assert_eq!(plan(&d, &Overlays::default()), RenderPlan::Full);
    }

    #[test]
    fn empty_and_clear() {
        assert!(Damage::default().is_empty());
        let mut d = panes(&[1]);
        d.chrome = true;
        d.full = true;
        assert!(!d.is_empty());
        d.clear();
        assert!(d.is_empty());
        // The switch channel participates in both is_empty and clear.
        let mut s = Damage {
            switch: true,
            ..Default::default()
        };
        assert!(!s.is_empty());
        s.clear();
        assert!(s.is_empty());
    }

    // ── control-plane contract (add-control-plane-and-remote) ────────────────
    // A daemon-backed pane is a `Stream` pane: its bytes arrive as pane damage
    // via mpsc + waker exactly like a local PTY's, so the same invariants hold.
    // These re-assert them under the control-plane scenarios by name.

    #[test]
    fn daemon_reattach_snapshot_is_panes_only() {
        // The warm-reattach snapshot is one (large) output chunk for one pane:
        // a bounded pane diff, never a chrome recompose (spec: "Streaming
        // output is a pane-only frame").
        assert_eq!(
            plan(&panes(&[5]), &Overlays::default()),
            RenderPlan::Incremental {
                panes: vec![5],
                bars: false,
                sidebar: false
            }
        );
    }

    #[test]
    fn idle_attached_daemon_wake_skips() {
        // An attached-but-quiet daemon generates zero damage; a spurious wake
        // maps to Skip (spec: "Daemon work stays off the render loop").
        assert_eq!(
            plan(&Damage::default(), &Overlays::default()),
            RenderPlan::Skip
        );
    }

    #[test]
    fn daemon_attach_status_chrome_is_full() {
        // Attach/detach status and the pairing-approval overlay are chrome:
        // they map to Full, the sanctioned path for overlay changes.
        let d = Damage {
            chrome: true,
            ..Default::default()
        };
        assert_eq!(plan(&d, &Overlays::default()), RenderPlan::Full);
    }
}
