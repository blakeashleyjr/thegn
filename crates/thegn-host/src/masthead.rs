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
///
/// `chip_spans` and `sidebar_chip` are the left cluster's clickable units, as
/// x-offsets within the left run (offset 0 = the masthead row's first cell).
/// A chip dropped for width has no span.
pub(crate) struct MastheadLayout {
    pub left: Vec<Seg>,
    pub right: Vec<Seg>,
    pub right_spans: Vec<(BarItemId, usize, usize)>,
    /// `(app tab index, x offset, width)` for each placed app chip; the width
    /// covers the pill only, not its trailing gap space.
    pub chip_spans: Vec<(usize, usize, usize)>,
    /// `(x offset, width)` of the hidden-sidebar chip, when shown.
    pub sidebar_chip: Option<(usize, usize)>,
}

/// The brand logo segs for the current width's rung: `þ thegn v0.0.0` (full),
/// `þ thegn` (compact), or nothing (hidden). A leading pad is the caller's.
/// The sigil is the Old English thorn (`þegn`), `*` under ASCII caps.
///
/// When the config directory is relocated, the instance tag follows the brand
/// on both visible rungs. It is intentionally absent from the hidden rung.
fn brand_segs(model: &FrameModel, brand_cols: usize) -> Vec<Seg> {
    if brand_cols == 0 {
        return Vec::new();
    }
    let accent = Tok::Attr(chrome::theme_color(model.accent_or_default()));
    let sigil = format!("{} ", crate::caps::active_glyphs().brand_sigil);
    let mut v = vec![seg(accent, sigil), seg(Tok::Slot(S::Text), "thegn")];
    if let Some(tag) = current_instance_tag() {
        v.push(seg(Tok::Slot(S::Accent), format!(" [{tag}]")));
    }
    if brand_cols >= chrome::BRAND_FULL_COLS {
        v.push(seg(Tok::Slot(S::Text), " "));
        v.push(seg(Tok::Slot(S::Ghost), crate::e2e_freeze::version_label()));
    }
    v
}

/// The instance tag shown after the brand: the basename of `config_dir` when
/// it is not `default_dir`, control characters dropped, at most 16 characters;
/// `None` for the default. An empty basename becomes `"alt"`.
fn instance_tag(config_dir: &std::path::Path, default_dir: &std::path::Path) -> Option<String> {
    if config_dir == default_dir {
        return None;
    }

    let tag: String = config_dir
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(16)
        .collect();
    Some(if tag.is_empty() {
        "alt".to_owned()
    } else {
        tag
    })
}

fn current_instance_tag() -> Option<&'static str> {
    if cfg!(test) || crate::e2e_freeze::active() {
        return None;
    }
    static TAG: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TAG.get_or_init(|| {
        instance_tag(
            &thegn_core::util::thegn_config_dir(),
            &thegn_core::util::xdg_config_home().join("thegn"),
        )
    })
    .as_deref()
}

/// The faint button pill behind an inactive app chip (and the hidden-sidebar
/// chip): the dim text hue at ~10% over the panel, so a quiet chip still reads
/// as something to click without competing with the active chip's focus pill.
fn quiet_pill() -> ColorAttribute {
    let dim = chrome::with_palette(|p| chrome::slot_rgb(p, S::Dim).to_string());
    chrome::theme_color(&theme::blend_over(&dim, &chrome::panel_rgb(), 0.10))
}

/// The hidden-sidebar chip: present only when the sidebar is `Hidden`, so the
/// masthead says where the sidebar went. The mode glyph alone on narrow rungs;
/// glyph + "sidebar hidden" on the wide rung. Carries a trailing gap space like
/// the app chips.
fn sidebar_chip_unit(model: &FrameModel, wide: bool) -> Option<Vec<Seg>> {
    if model.sidebar_mode != crate::layout::SidebarMode::Hidden {
        return None;
    }
    let glyph = crate::caps::active_glyphs().sidebar_mode_hidden;
    let text = if wide {
        format!(" {glyph} sidebar hidden ")
    } else {
        format!(" {glyph} ")
    };
    Some(vec![
        seg(Tok::Slot(S::Dim), text).bg(Tok::Attr(quiet_pill())),
        seg(Tok::Slot(S::Text), " "),
    ])
}

/// The app-tab chips as `(is_active, segs)` units in tab order. The active chip
/// reads as a focus-tinted pill (matching the stats focus selection); the rest
/// sit on a faint pill so they read as buttons. Each unit carries a trailing
/// gap space so chips don't butt.
fn app_chip_units(model: &FrameModel) -> Vec<(bool, Vec<Seg>)> {
    if model.app_tabs.is_empty() {
        return Vec::new();
    }
    let pill = chrome::theme_color(&theme::blend_over(
        &chrome::focus_rgb(),
        &chrome::panel_rgb(),
        0.28,
    ));
    let quiet = quiet_pill();
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
                seg(Tok::Slot(S::Dim), text).bg(Tok::Attr(quiet))
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
    let sidebar_chip = sidebar_chip_unit(model, brand_cols >= chrome::BRAND_FULL_COLS);
    let sidebar_chip_w = sidebar_chip.as_deref().map(seg_width).unwrap_or(0);

    // Reserve the left's protected core — leading pad + brand + the hidden-
    // sidebar chip + the active chip — so the right cluster can never eat
    // those cells.
    let active_chip_w = chips
        .iter()
        .find(|(a, _)| *a)
        .map(|(_, s)| seg_width(s))
        .unwrap_or(0);
    // A quiet gap between the brand and the first chip so the highlighted pill
    // doesn't butt against the version — only when both are present.
    let gap = usize::from(!brand.is_empty() && (!chips.is_empty() || sidebar_chip.is_some()));
    let protected = 1 + seg_width(&brand) + gap + sidebar_chip_w + active_chip_w;

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
    if gap == 1 {
        left.push(seg(Tok::Slot(S::Text), " "));
    }
    // The hidden-sidebar chip leads the chips; like the active chip it is
    // unconditional (its slot was held in `protected`). Its span excludes the
    // trailing gap space.
    let mut sidebar_span = None;
    if let Some(unit) = sidebar_chip {
        sidebar_span = Some((seg_width(&left), sidebar_chip_w.saturating_sub(1)));
        left.extend(unit);
    }
    // Chips in tab order: the active chip is unconditional (its slot was held in
    // `protected`); inactive chips join only while they fit, holding the active
    // chip's reserve until it lands so an earlier inactive can't crowd it out.
    // Each placed chip records its span (pill only, not the trailing gap).
    let mut chip_spans = Vec::new();
    let mut reserve = active_chip_w;
    for (idx, (active, chip)) in chips.iter().enumerate() {
        let w = seg_width(chip);
        let place = if *active {
            reserve = 0;
            true
        } else {
            seg_width(&left) + w + reserve <= left_budget
        };
        if place {
            chip_spans.push((idx, seg_width(&left), w.saturating_sub(1)));
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
        chip_spans,
        sidebar_chip: sidebar_span,
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

/// The placed app chips' absolute `(app tab index, Rect)` spans for the given
/// chrome layout, in tab order. Same layout call as [`chrome::draw_masthead`],
/// so paint and hit-test never disagree; a chip dropped for width has no span.
/// The left cluster is left-aligned by `Line::split`, so offsets are relative
/// to the masthead row's first cell.
pub(crate) fn masthead_chip_spans(
    model: &FrameModel,
    layout: &crate::layout::ChromeLayout,
) -> Vec<(usize, crate::compositor::Rect)> {
    let rect = layout.masthead_stats_row();
    if rect.rows == 0 || rect.cols == 0 {
        return Vec::new();
    }
    let lay = masthead_layout(model, rect.cols, None);
    lay.chip_spans
        .into_iter()
        .map(|(idx, off, w)| {
            (
                idx,
                crate::compositor::Rect {
                    x: rect.x + off,
                    y: rect.y,
                    cols: w,
                    rows: 1,
                },
            )
        })
        .collect()
}

/// The hidden-sidebar chip's absolute span, when the sidebar is `Hidden` and
/// the masthead row is visible. Mirrors [`masthead_chip_spans`].
pub(crate) fn masthead_sidebar_chip_span(
    model: &FrameModel,
    layout: &crate::layout::ChromeLayout,
) -> Option<crate::compositor::Rect> {
    let rect = layout.masthead_stats_row();
    if rect.rows == 0 || rect.cols == 0 {
        return None;
    }
    let (off, w) = masthead_layout(model, rect.cols, None).sidebar_chip?;
    Some(crate::compositor::Rect {
        x: rect.x + off,
        y: rect.y,
        cols: w,
        rows: 1,
    })
}

/// The app tab index whose chip span contains `(x, y)`, if any. Pure, so the
/// click branch and tests share one hit rule.
pub(crate) fn chip_at(
    spans: &[(usize, crate::compositor::Rect)],
    x: usize,
    y: usize,
) -> Option<usize> {
    spans
        .iter()
        .find(|(_, r)| r.contains(x, y))
        .map(|(idx, _)| *idx)
}

#[cfg(test)]
mod tests {
    use super::instance_tag;
    use std::path::Path;

    #[test]
    fn instance_tag_table() {
        let cases = [
            (
                Path::new("/home/u/.config/thegn"),
                Path::new("/home/u/.config/thegn"),
                None,
            ),
            (
                Path::new("/home/u/.config/thegn-dev"),
                Path::new("/home/u/.config/thegn"),
                Some("thegn-dev"),
            ),
            (
                Path::new("/home/u/1234567890123456789012345678901234567890"),
                Path::new("/home/u/.config/thegn"),
                Some("1234567890123456"),
            ),
            (
                Path::new("/home/u/thegn-\u{1b}dev"),
                Path::new("/home/u/.config/thegn"),
                Some("thegn-dev"),
            ),
            (
                Path::new(""),
                Path::new("/home/u/.config/thegn"),
                Some("alt"),
            ),
        ];

        for (config_dir, default_dir, expected) in cases {
            assert_eq!(instance_tag(config_dir, default_dir).as_deref(), expected);
        }
    }
}
