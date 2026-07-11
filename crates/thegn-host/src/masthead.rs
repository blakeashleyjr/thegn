//! The masthead (top bar) layout, built on the seg/`Line` layer — the same
//! machinery the statusbar uses — so the top bar degrades at narrow widths
//! exactly like the bottom bar: display-width measurement, atomic-unit left
//! fit, a ghost `…` on an overlong breadcrumb, and the right stats cluster
//! shedding by priority. Nothing clips mid-glyph or overlaps.
//!
//! Layout is a two-pass budget (the priority ladder, softest first):
//! 1. Right **stats** shed by priority (`chrome::fit_stats_cluster`).
//! 2. `top_left` **breadcrumb/clock** widgets truncate with `…`, then drop.
//! 3. **App-tab chips** — the *active* chip is protected; inactive chips drop.
//! 4. The **brand** shrinks last (full → compact → hidden).
//!
//! The right cluster is reserved only what remains after the brand + active
//! chip are guaranteed, so those never lose their cells; `Line::split` then
//! right-aligns the stats and left-aligns the rest with a gutter between, its
//! own ellipsis acting purely as a floor for the pathological narrow case.

use termwiz::color::ColorAttribute;

use crate::chrome::{self, BarItemId, FrameModel, S};
use crate::seg::{Seg, Tok, cut, seg, seg_width};
use thegn_core::theme;

/// The resolved masthead: the committed left seg run (brand + chips +
/// breadcrumb), the committed right seg run (priority-fitted stats, with the
/// focus pill applied), and the right-cluster item spans (`id`, x-offset within
/// the cluster, width) for hit-testing / popup anchoring.
pub(crate) struct MastheadLayout {
    pub left: Vec<Seg>,
    pub right: Vec<Seg>,
    pub right_spans: Vec<(BarItemId, usize, usize)>,
}

/// The version wordmark shown in the full brand rung.
const BRAND_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// The brand logo segs for the current width's rung: `◆ thegn v0.0.0` (full),
/// `◆ thegn` (compact), or nothing (hidden). A leading pad is the caller's.
fn brand_segs(model: &FrameModel, brand_cols: usize) -> Vec<Seg> {
    if brand_cols == 0 {
        return Vec::new();
    }
    let accent = Tok::Attr(chrome::theme_color(model.accent_or_default()));
    let diamond = format!("{} ", crate::caps::active_glyphs().diamond_filled);
    let mut v = vec![seg(accent, diamond), seg(Tok::Slot(S::Text), "thegn")];
    if brand_cols >= chrome::BRAND_FULL_COLS {
        v.push(seg(Tok::Slot(S::Text), " "));
        v.push(seg(Tok::Slot(S::Ghost), BRAND_VERSION));
    }
    v
}

/// The app-tab chips as `(is_active, segs)` units in tab order. The active chip
/// reads as a focus-tinted pill (matching the stats focus selection); the rest
/// are quiet. Each unit carries a trailing gap space so chips don't butt.
fn app_chip_units(model: &FrameModel) -> Vec<(bool, Vec<Seg>)> {
    if model.app_tabs.is_empty() {
        return Vec::new();
    }
    let pill = chrome::theme_color(&theme::blend_over(
        &chrome::focus_rgb(),
        &chrome::panel_rgb(),
        0.28,
    ));
    model
        .app_tabs
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let active = i == model.active_app;
            let text = format!(" {label} ");
            let chip = if active {
                seg(Tok::Slot(S::Focus), text).bg(Tok::Attr(pill)).bold()
            } else {
                seg(Tok::Slot(S::Dim), text)
            };
            (active, vec![chip, seg(Tok::Slot(S::Text), " ")])
        })
        .collect()
}

/// The right stats cluster's candidate items — every resolvable `top_right`
/// widget as `(raw id, segs)`, in config order. The id string is kept so
/// [`chrome::fit_stats_cluster`] can drop by priority.
fn masthead_right_items(model: &FrameModel) -> Vec<(String, Vec<Seg>)> {
    model
        .bars
        .top_right
        .iter()
        .filter_map(|id| {
            chrome::masthead_widget(id, model)
                .map(|w| (id.clone(), vec![seg(Tok::Attr(w.fg), w.text)]))
        })
        .collect()
}

/// Lay out the right stats cluster from pre-fitted items: join with ` · `
/// separators, apply the focus pill to `sel`, and return the seg run plus each
/// item's `(id, x_offset, width)` within the cluster (offset 0 = its left
/// cell). Mirrors `chrome::statusbar_right_layout`, with the masthead's quiet
/// `·` separator and a trailing 1-col right margin.
fn masthead_right_layout(
    items: &[(BarItemId, Vec<Seg>)],
    sel: Option<(usize, ColorAttribute, ColorAttribute)>,
) -> (Vec<Seg>, Vec<(BarItemId, usize, usize)>) {
    let sel_idx = sel.map(|(i, _, _)| i.min(items.len().saturating_sub(1)));
    let mut r: Vec<Seg> = Vec::new();
    let mut spans: Vec<(BarItemId, usize, usize)> = Vec::new();
    let mut off = 0usize;
    for (idx, (id, segs)) in items.iter().enumerate() {
        if idx > 0 {
            r.push(seg(Tok::Slot(S::Ghost), " \u{00b7} "));
            off += 3;
        }
        let drawn = match (sel, sel_idx) {
            (Some((_, pill, fg)), Some(s)) if s == idx => chrome::highlight_segs(segs, pill, fg),
            _ => segs.clone(),
        };
        let w = seg_width(&drawn);
        spans.push((id.clone(), off, w));
        off += w;
        r.extend(drawn);
    }
    if !r.is_empty() {
        r.push(seg(Tok::Slot(S::Text), " "));
    }
    (r, spans)
}

/// Resolve the masthead for a width: the two-pass budget described in the module
/// docs. Shared by [`chrome::draw_masthead`] (placement + highlight) and
/// [`masthead_item_spans`] (navigation + hit-testing), so which stats show — and
/// where — never disagrees.
pub(crate) fn masthead_layout(
    model: &FrameModel,
    cols: usize,
    sel: Option<(usize, ColorAttribute, ColorAttribute)>,
) -> MastheadLayout {
    let brand_cols = chrome::masthead_brand_cols(cols);
    let brand = brand_segs(model, brand_cols);
    let chips = app_chip_units(model);

    // Reserve the left's protected core — leading pad + brand + the active chip
    // — so the right cluster can never eat those cells.
    let active_chip_w = chips
        .iter()
        .find(|(a, _)| *a)
        .map(|(_, s)| seg_width(s))
        .unwrap_or(0);
    let protected = 1 + seg_width(&brand) + active_chip_w;

    // Priority-fit the stats into whatever remains (keep a 1-col gutter).
    let candidates = masthead_right_items(model);
    let widths: Vec<(String, usize)> = candidates
        .iter()
        .map(|(id, segs)| (id.clone(), seg_width(segs)))
        .collect();
    let right_avail = cols.saturating_sub(protected + 1);
    let kept = chrome::fit_stats_cluster(&widths, right_avail);
    let items: Vec<(BarItemId, Vec<Seg>)> = kept
        .iter()
        .map(|&i| {
            (
                BarItemId::Widget(candidates[i].0.clone()),
                candidates[i].1.clone(),
            )
        })
        .collect();
    let (right, right_spans) = masthead_right_layout(&items, sel);
    let right_w = seg_width(&right);

    // The cells the left gets once the right has won its space — mirrors
    // `Line::split`'s math so units commit at their own boundaries here rather
    // than being cut mid-unit by the generic ellipsis.
    let left_budget = cols.saturating_sub(right_w + usize::from(right_w > 0));

    let mut left: Vec<Seg> = vec![seg(Tok::Slot(S::Text), " ")];
    left.extend(brand);
    // Chips in tab order: the active chip is unconditional (its slot was held in
    // `protected`); inactive chips join only while they fit, holding the active
    // chip's reserve until it lands so an earlier inactive can't crowd it out.
    let mut reserve = active_chip_w;
    for (active, chip) in &chips {
        let w = seg_width(chip);
        if *active {
            left.extend(chip.clone());
            reserve = 0;
        } else if seg_width(&left) + w + reserve <= left_budget {
            left.extend(chip.clone());
        }
    }
    // Breadcrumb / clock widgets: join whole while they fit; the first that
    // overflows is truncated with `…` (if there's meaningful room) and ends it.
    for id in &model.bars.top_left {
        if id == "brand" {
            continue;
        }
        let Some(wd) = chrome::masthead_widget(id, model) else {
            continue;
        };
        let unit = vec![
            seg(Tok::Slot(S::Ghost), " \u{00b7} "),
            seg(Tok::Attr(wd.fg), wd.text),
        ];
        let w = seg_width(&unit);
        if seg_width(&left) + w <= left_budget {
            left.extend(unit);
        } else {
            let room = left_budget.saturating_sub(seg_width(&left));
            if room > 3 {
                left.extend(cut(&unit, room));
            }
            break;
        }
    }

    MastheadLayout {
        left,
        right,
        right_spans,
    }
}

/// The masthead right-cluster items' absolute `(id, Rect)` spans for the given
/// chrome layout — mouse hit-testing and detail-popup anchoring. Only the right
/// stats are navigable (the brand, chips, and breadcrumb are not). Mirrors
/// `chrome::statusbar_item_spans`: `Line::split` right-aligns the cluster, so it
/// begins at `x + cols - right_width`.
pub fn masthead_item_spans(
    model: &FrameModel,
    layout: &crate::layout::ChromeLayout,
) -> Vec<(BarItemId, crate::compositor::Rect)> {
    let rect = layout.masthead_stats_row();
    if rect.rows == 0 || rect.cols == 0 {
        return Vec::new();
    }
    let lay = masthead_layout(model, rect.cols, None);
    let rl = seg_width(&lay.right);
    let base = (rect.x + rect.cols).saturating_sub(rl);
    lay.right_spans
        .into_iter()
        .map(|(id, off, w)| {
            (
                id,
                crate::compositor::Rect {
                    x: base + off,
                    y: rect.y,
                    cols: w,
                    rows: 1,
                },
            )
        })
        .collect()
}
