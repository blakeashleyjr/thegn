//! The chrome **element contract** — one declarative shape for a chrome
//! element, native or plugin.
//!
//! The audit behind `openspec/changes/add-ui-component-contract` found the same
//! three-layer story in every chrome zone (masthead, statusbar, tabbar, pins,
//! sidebar, panel): content, hit targets and keys, each solved well once and
//! then re-derived by hand everywhere else. This module names the pattern that
//! already won inside the tree — **one build pass produces both the painted
//! output and the interaction tables** (`statusbar_fit::fit`, `panel/frame.rs`) —
//! and makes it the one shape a new element takes.
//!
//! An element is **data, not behavior**: there is no per-element `paint()`
//! callback, no layout engine, no view diffing. The compositor consumes the
//! declaration exactly as it consumes chrome today, so `render_plan::plan` is
//! untouched (a dirty element is a `Full` frame, as a dirty badge is today).
//!
//! ## The builder rule (why this module exists)
//!
//! [`statusbar_fit`]'s docstring records the bug class this closes: a hit table
//! built from a *different* list than the painter used ⇒ clicks land on the
//! wrong badge. The structural fix — the load-bearing rule of the whole
//! contract — is that **painting and hit-emission are one function's two
//! outputs**. [`ChipRow`] enforces it for horizontally-laid chip strips (pins,
//! tabs, badges): every [`ChipRow::push`] appends a paintable [`Seg`] *and*
//! records a [`HitSpan`] covering exactly the cells that seg paints, from the
//! same call. A shed chip is never pushed, so it can neither be painted nor
//! clicked. [`hit_at`] is the one shared mouse resolution over the emitted
//! spans, replacing per-zone geometry re-derivation.
//!
//! [`statusbar_fit`]: crate::statusbar_fit

// The element contract's full vocabulary is defined whole here; zones adopt its
// variants as they migrate off the legacy draw sites pinned in
// `test/element-ratchet.txt`. Same rationale as `seg.rs`'s module allow: the
// shape is the contract, and it is complete before every zone has adopted it.
#![allow(dead_code)]

use crate::compositor::Rect;
use crate::seg::{Line, Seg, Tok, seg_width};

/// A stable element id — one namespace across native and plugin elements,
/// because the placement grammar (`[bars]` / `[panel] sections`) addresses
/// elements by id. Native ids are bare (`"badge:ci"`, `"panel:changes"`);
/// plugin ids are `plugin:<plugin>:<contribution>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ElementId(String);

impl ElementId {
    /// A native element id (e.g. `"badge:ci"`, `"pins"`).
    pub fn native(id: impl Into<String>) -> Self {
        ElementId(id.into())
    }

    /// A plugin contribution's id: `plugin:<plugin>:<contribution>`.
    pub fn plugin(plugin: &str, contribution: &str) -> Self {
        ElementId(format!("plugin:{plugin}:{contribution}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this id names a plugin contribution (the untrusted-content path).
    pub fn is_plugin(&self) -> bool {
        self.0.starts_with("plugin:")
    }
}

impl std::fmt::Display for ElementId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The chrome region that owns an element. The ids mirror the help system's
/// `zone:*` context vocabulary ([`crate::help::context::zone_key`]) so an
/// element ties to its docs page for free; [`Zone::context_key`] is asserted
/// against that vocabulary in the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Zone {
    Masthead,
    Statusbar,
    /// The center column's tab strip.
    Tabbar,
    /// The right-aligned pin chips in the tab strip.
    Pins,
    Sidebar,
    Panel,
    /// A floating overlay (`layer.rs`); out of scope for migration, present for
    /// completeness.
    Layer,
}

impl Zone {
    /// The help context key for this zone — the same string
    /// [`crate::help::context::zone_key`] resolves a focus zone to, so an
    /// element's zone and its documentation page share one vocabulary. The
    /// center column (tabbar + pins) and floating layers resolve to
    /// `zone:center`, matching how focus resolves there today.
    pub fn context_key(self) -> &'static str {
        match self {
            Zone::Masthead => "zone:masthead",
            Zone::Statusbar => "zone:statusbar",
            Zone::Tabbar | Zone::Pins | Zone::Layer => "zone:center",
            Zone::Sidebar => "zone:sidebar",
            Zone::Panel => "zone:panel",
        }
    }
}

/// What activating an element's hit span does. One host-wide vocabulary (the
/// design's "one signature, emitted by the painting build"): element actions
/// resolve to the same host dispatch that exists — this type only changes
/// *where the tables come from*. Plugin content resolves to [`ElementAction::PluginRow`]
/// only: a plugin element can never name a host action (no confused deputy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElementAction {
    /// Switch to a tab within the active worktree (0-based index). Tabbar.
    SelectTab(usize),
    /// Summon / focus a pin by its 1-based `Alt-N` index. Pins.
    SummonPin(usize),
    /// Open a statusbar/masthead bar item's detail view, by element id.
    OpenBarDetail(ElementId),
    /// Deliver activation to a plugin element as an `on_event` — the
    /// contribution's id plus the activated row id. Never a host action.
    PluginRow { element: ElementId, row: String },
}

/// A screen rectangle paired with the action a click there performs — the one
/// shared hit-span signature every element zone uses. Emitted by the same build
/// that paints (see the module docs and [`ChipRow`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitSpan {
    pub rect: Rect,
    pub action: ElementAction,
}

/// One row of an element — the panel's `PanelRow` generalized: a fitted [`Line`],
/// an optional row-background tint, and an optional whole-row hit action.
/// Horizontal (within-row) hit targets live in [`ElementBuild::hits`], emitted
/// by the build; the row-level `hit` is the convenience form a full-width
/// clickable row (a sidebar/panel list row) uses.
#[derive(Debug, Clone)]
pub struct ElementRow {
    pub line: Line,
    pub bg: Option<Tok>,
    pub hit: Option<ElementAction>,
}

impl ElementRow {
    pub fn new(line: Line) -> Self {
        ElementRow {
            line,
            bg: None,
            hit: None,
        }
    }
    pub fn bg(mut self, bg: Tok) -> Self {
        self.bg = Some(bg);
        self
    }
    pub fn hit(mut self, action: ElementAction) -> Self {
        self.hit = Some(action);
        self
    }
}

/// The output of one element build pass: the rows to paint and the hit spans,
/// produced **together** so a hit can never reference a row the painter did not
/// draw. This is the whole contract in one struct.
#[derive(Debug, Clone)]
pub struct ElementBuild {
    pub id: ElementId,
    pub zone: Zone,
    pub rows: Vec<ElementRow>,
    pub hits: Vec<HitSpan>,
}

/// A horizontally-laid run of hit-testable chips built as **one row plus its hit
/// spans**, in one pass. This is the builder rule for chip strips (pins, tabs,
/// badges): each [`push`](ChipRow::push) appends the chip's [`Seg`] to the row
/// *and* records a [`HitSpan`] covering exactly the cells that seg paints, so
/// the painted list and the hit list are the same list by construction. A chip
/// that is shed under width pressure is simply never pushed — it can neither be
/// painted nor clicked.
///
/// `x`/`y` are the row's origin in screen cells; the cursor advances by each
/// chip's display width so the hit rect and the painted cells coincide.
pub struct ChipRow {
    y: usize,
    start_x: usize,
    x: usize,
    segs: Vec<Seg>,
    hits: Vec<HitSpan>,
}

impl ChipRow {
    /// A new chip row whose first chip starts at `(x, y)` in screen cells.
    pub fn new(x: usize, y: usize) -> Self {
        ChipRow {
            y,
            start_x: x,
            x,
            segs: Vec::new(),
            hits: Vec::new(),
        }
    }

    /// Cells consumed so far — the row's current width.
    pub fn width(&self) -> usize {
        self.x - self.start_x
    }

    /// Push an interactive chip: its `seg` is appended to the row and a hit span
    /// covering exactly the chip's cells is recorded with `action`. Painting and
    /// hit-emission from one call — the builder rule.
    pub fn push(&mut self, seg: Seg, action: ElementAction) {
        let w = seg_width(std::slice::from_ref(&seg));
        self.hits.push(HitSpan {
            rect: Rect {
                x: self.x,
                y: self.y,
                cols: w,
                rows: 1,
            },
            action,
        });
        self.x += w;
        self.segs.push(seg);
    }

    /// Push a non-interactive seg (a leading label, a separator) — painted, no
    /// hit span.
    pub fn push_plain(&mut self, seg: Seg) {
        let w = seg_width(std::slice::from_ref(&seg));
        self.x += w;
        self.segs.push(seg);
    }

    /// The paintable line for this row.
    pub fn line(&self) -> Line {
        Line::Segs(self.segs.clone())
    }

    /// The recorded hit spans (borrow).
    pub fn hits(&self) -> &[HitSpan] {
        &self.hits
    }

    /// Consume into the paintable line and its hit spans.
    pub fn into_parts(self) -> (Line, Vec<HitSpan>) {
        (Line::Segs(self.segs), self.hits)
    }
}

/// The one shared mouse resolution over a build's hit spans: the action at cell
/// `(x, y)`, or `None`. Element zones route mouse clicks through this instead of
/// re-deriving per-zone geometry.
pub fn hit_at(hits: &[HitSpan], x: usize, y: usize) -> Option<&ElementAction> {
    hits.iter()
        .find(|h| h.rect.contains(x, y))
        .map(|h| &h.action)
}

/// Build a horizontal chip strip that sheds trailing chips that do not fit
/// `avail` cells, emitting a [`HitSpan`] **only** for the chips that survived —
/// the contract's builder rule made concrete and reusable. Returns the row's
/// [`Line`] and its hit spans, which are the same list by construction.
pub fn build_chip_strip(
    x: usize,
    y: usize,
    avail: usize,
    chips: impl IntoIterator<Item = (Seg, ElementAction)>,
) -> (Line, Vec<HitSpan>) {
    let mut row = ChipRow::new(x, y);
    for (seg, action) in chips {
        let w = seg_width(std::slice::from_ref(&seg));
        if row.width() + w > avail {
            break; // shed: no seg painted, no hit emitted
        }
        row.push(seg, action);
    }
    row.into_parts()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::S;
    use crate::seg::seg;

    fn line_width(line: &Line) -> usize {
        match line {
            Line::Segs(v) => seg_width(v),
            _ => 0,
        }
    }

    /// The builder rule (Task 1.3): a build's hit spans reference only the rows
    /// it painted. Constructed as a shedding case, in the style of the
    /// `statusbar_fit` tests — the drift bug (a hit table built from a different
    /// list than the painter used) is made structurally unrepresentable.
    #[test]
    fn hits_reference_only_painted_chips() {
        let chips = vec![
            (seg(Tok::Slot(S::Text), "AA"), ElementAction::SummonPin(1)),
            (seg(Tok::Slot(S::Text), "BB"), ElementAction::SummonPin(2)),
            (seg(Tok::Slot(S::Text), "CC"), ElementAction::SummonPin(3)),
        ];
        // 4 cells fits AA+BB (2+2); CC sheds.
        let (line, hits) = build_chip_strip(0, 0, 4, chips);

        // Exactly the two painted chips have hit spans — the third is absent.
        assert_eq!(hits.len(), 2, "shed chip emits no hit span");
        assert_eq!(hit_at(&hits, 0, 0), Some(&ElementAction::SummonPin(1)));
        assert_eq!(hit_at(&hits, 1, 0), Some(&ElementAction::SummonPin(1)));
        assert_eq!(hit_at(&hits, 2, 0), Some(&ElementAction::SummonPin(2)));
        assert_eq!(hit_at(&hits, 3, 0), Some(&ElementAction::SummonPin(2)));

        // A click at the shed chip's would-be cells resolves to nothing — it
        // cannot dispatch SummonPin(3), because that chip was never painted.
        assert_eq!(hit_at(&hits, 4, 0), None);

        // Every hit rect lies within the painted line, and the painted width
        // equals the sum of the surviving chips (2 + 2).
        assert_eq!(line_width(&line), 4);
        for h in &hits {
            assert!(
                h.rect.x + h.rect.cols <= line_width(&line),
                "hit span outside painted cells: {h:?}"
            );
        }
    }

    #[test]
    fn chip_row_paints_and_hits_from_one_pass() {
        let mut row = ChipRow::new(10, 2);
        row.push_plain(seg(Tok::Slot(S::Dim), "> ")); // 2-cell non-interactive lead
        row.push(seg(Tok::Slot(S::Text), " a "), ElementAction::SummonPin(1)); // 3 cells
        row.push(seg(Tok::Slot(S::Text), " bb "), ElementAction::SummonPin(2)); // 4 cells

        // The lead has no hit; the two chips hit at their painted cells.
        assert_eq!(hit_at(row.hits(), 10, 2), None, "lead is not interactive");
        assert_eq!(
            hit_at(row.hits(), 12, 2),
            Some(&ElementAction::SummonPin(1))
        );
        assert_eq!(
            hit_at(row.hits(), 15, 2),
            Some(&ElementAction::SummonPin(2))
        );
        assert_eq!(row.width(), 2 + 3 + 4);
    }

    #[test]
    fn plugin_and_native_ids_share_one_namespace() {
        assert_eq!(ElementId::native("badge:ci").as_str(), "badge:ci");
        assert!(!ElementId::native("badge:ci").is_plugin());
        let p = ElementId::plugin("hello", "todo");
        assert_eq!(p.as_str(), "plugin:hello:todo");
        assert!(p.is_plugin());
        assert_eq!(p.to_string(), "plugin:hello:todo");
    }

    #[test]
    fn zone_context_keys_are_in_the_help_vocabulary() {
        // The contract's promise: an element's zone key is the SAME string the
        // help context system understands, so the help-context ratchet ties an
        // element to its docs page. A drift here (a renamed zone key) fails.
        let vocab = crate::help::context::vocabulary();
        for z in [
            Zone::Masthead,
            Zone::Statusbar,
            Zone::Tabbar,
            Zone::Pins,
            Zone::Sidebar,
            Zone::Panel,
            Zone::Layer,
        ] {
            assert!(
                vocab.iter().any(|k| k == z.context_key()),
                "{z:?} -> {} not in help vocabulary",
                z.context_key()
            );
        }
    }
}
