//! The Keys section: the effective cheatsheet, panel controls, and each
//! section's keys, with a local filter and source lookup.

use std::collections::BTreeSet;

use crate::panel::sections::SectionCtx;
use crate::panel::{PanelUi, SECTION_ORDER, Section};
use crate::seg::{Line, Seg, Tok, seg, sp};

use super::{PanelRow, d, g2, g3, hint_row, t, two_col};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyOrigin {
    Cheatsheet,
    Panel,
    Section(Section),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyRow {
    pub(crate) chord: String,
    pub(crate) label: String,
    pub(crate) origin: KeyOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyGroup {
    title: String,
    rows: Vec<KeyRow>,
}

fn panel_group(ui: &PanelUi) -> KeyGroup {
    let row = |chord: &str, label: &str| KeyRow {
        chord: chord.into(),
        label: label.into(),
        origin: KeyOrigin::Panel,
    };
    let n = ui.tab_sections().len();
    let jump = match n {
        0..=9 => format!("1-{n}"),
        _ => "1-9,0".to_string(),
    };
    KeyGroup {
        title: "Panel".into(),
        rows: vec![
            row(&jump, "jump to section (this tab)"),
            row("e", "cycle width"),
            row("j/k", "walk rows"),
            row("J/K", "hop sections"),
            row("↵", "select row"),
            row("esc", "back"),
        ],
    }
}

/// Build every displayed key group from the cached cheatsheet and static
/// section tables. This never reads configuration or performs I/O.
fn groups(ui: &PanelUi) -> Vec<KeyGroup> {
    let mut out: Vec<KeyGroup> = ui
        .docs
        .cfg_keys
        .iter()
        .map(|group| KeyGroup {
            title: group.title.clone(),
            rows: group
                .rows
                .iter()
                .map(|row| KeyRow {
                    chord: row.chord.clone(),
                    label: row.label.clone(),
                    origin: KeyOrigin::Cheatsheet,
                })
                .collect(),
        })
        .collect();

    // The Panel keys depend on the active tab's numbered sections.
    out.push(panel_group(ui));

    for section in SECTION_ORDER {
        if ui.off.iter().any(|(off, _)| *off == section) {
            continue;
        }
        let mut rows = Vec::new();
        if let Some(view) = section.home_view() {
            let mut seen = BTreeSet::new();
            rows.extend(
                crate::panel::gitui::context_keys(view)
                    .into_iter()
                    .filter(|key| seen.insert(key.chord))
                    .map(|key| KeyRow {
                        chord: key.chord.into(),
                        label: key.label.into(),
                        origin: KeyOrigin::Section(section),
                    }),
            );
        } else {
            rows.extend(
                crate::panel::section_keys::section_keys(section)
                    .iter()
                    .filter(|key| key.key.is_some())
                    .map(|key| KeyRow {
                        chord: key.chord.into(),
                        label: key.label.into(),
                        origin: KeyOrigin::Section(section),
                    }),
            );
        }
        if !rows.is_empty() {
            out.push(KeyGroup {
                title: format!("{} ▸ {}", section.tab().label(), section.label()),
                rows,
            });
        }
    }

    let query = ui.keys_filter.to_lowercase();
    if query.is_empty() {
        return out;
    }
    out.into_iter()
        .filter_map(|mut group| {
            let title_matches = group.title.to_lowercase().contains(&query);
            if !title_matches {
                group.rows.retain(|row| {
                    row.chord.to_lowercase().contains(&query)
                        || row.label.to_lowercase().contains(&query)
                });
            }
            (!group.rows.is_empty()).then_some(group)
        })
        .collect()
}

fn filter_bar(ui: &PanelUi) -> Option<PanelRow> {
    if !ui.keys_filter_editing && ui.keys_filter.is_empty() {
        return None;
    }
    let mut segs = vec![seg(g2(), "/ "), seg(t(), ui.keys_filter.clone())];
    if ui.keys_filter_editing {
        segs.push(seg(d(), "▌"));
    } else {
        segs.push(seg(g3(), "  (Esc clears)"));
    }
    Some(PanelRow::plain(Line::segs(segs)))
}

fn group_lines(group: &KeyGroup) -> Vec<Line> {
    let mut out = vec![Line::segs(vec![
        seg(g2(), group.title.to_uppercase()).bold(),
    ])];
    out.extend(group.rows.iter().map(|row| {
        Line::segs(vec![
            sp(1),
            Seg::chip(
                Tok::Slot(crate::chrome::S::Raise),
                format!(" {} ", row.chord),
            ),
            seg(d(), format!("  {}", row.label)),
        ])
    }));
    out.push(Line::Blank);
    out
}

fn no_matches(ui: &PanelUi) -> Vec<Line> {
    if ui.keys_filter.is_empty() {
        Vec::new()
    } else {
        vec![Line::segs(vec![seg(
            g3(),
            format!("no keys match `{}`", ui.keys_filter),
        )])]
    }
}

fn half_lines(ui: &PanelUi, groups: &[KeyGroup]) -> Vec<Line> {
    let mut lines = filter_bar(ui).map_or_else(Vec::new, |row| vec![row.line]);
    if groups.is_empty() {
        lines.extend(no_matches(ui));
    } else {
        lines.extend(groups.iter().flat_map(group_lines));
    }
    lines
}

fn split_full_groups(groups: &[KeyGroup]) -> (Vec<KeyGroup>, Vec<KeyGroup>) {
    let total: usize = groups.iter().map(|group| group.rows.len() + 2).sum();
    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut left_len = 0;
    for group in groups {
        let group_len = group.rows.len() + 2;
        if left_len + group_len <= total.div_ceil(2) || left.is_empty() {
            left_len += group_len;
            left.push(group.clone());
        } else {
            right.push(group.clone());
        }
    }
    (left, right)
}

fn full_lines(ui: &PanelUi, groups: &[KeyGroup], cols: usize) -> Vec<Line> {
    let mut lines = filter_bar(ui).map_or_else(Vec::new, |row| vec![row.line]);
    if groups.is_empty() {
        lines.extend(no_matches(ui));
        return lines;
    }
    let (left_groups, right_groups) = split_full_groups(groups);
    let left: Vec<Line> = left_groups.iter().flat_map(group_lines).collect();
    let right: Vec<Line> = right_groups.iter().flat_map(group_lines).collect();
    let to_segs = |lines: Vec<Line>| -> Vec<Vec<Seg>> {
        lines
            .into_iter()
            .map(|line| match line {
                Line::Segs(segs) => segs,
                _ => Vec::new(),
            })
            .collect()
    };
    let col_w = cols.saturating_sub(2) / 2;
    lines.extend(two_col(&to_segs(left), &to_segs(right), col_w, 2));
    lines
}

fn visible_groups(ui: &PanelUi, width: crate::layout::PanelWidth) -> Vec<KeyGroup> {
    let mut groups = groups(ui);
    if ui.keys_filter.is_empty() && width == crate::layout::PanelWidth::Normal {
        groups.retain(|group| group.title == "Panel");
    }
    groups
}

fn normal(ctx: &SectionCtx, groups: &[KeyGroup]) -> Vec<PanelRow> {
    let mut rows = Vec::new();
    if let Some(bar) = filter_bar(ctx.ui) {
        rows.push(bar);
    }
    if groups.is_empty() {
        rows.extend(no_matches(ctx.ui).into_iter().map(PanelRow::plain));
    } else {
        rows.extend(groups.iter().flat_map(group_lines).map(PanelRow::plain));
    }
    rows.push(PanelRow::blank());
    rows.push(hint_row(&[("/", "filter"), ("e", "full cheatsheet")]));
    rows
}

fn half(ctx: &SectionCtx, groups: &[KeyGroup]) -> Vec<PanelRow> {
    let lines = half_lines(ctx.ui, groups);
    let max = lines.len().saturating_sub(ctx.rows);
    lines
        .into_iter()
        .skip(ctx.ui.scroll.min(max))
        .map(PanelRow::plain)
        .collect()
}

fn full(ctx: &SectionCtx, groups: &[KeyGroup]) -> Vec<PanelRow> {
    let lines = full_lines(ctx.ui, groups, ctx.cols);
    let body = ctx.rows.saturating_sub(2);
    let scroll = ctx.ui.scroll.min(lines.len().saturating_sub(body));
    let mut rows: Vec<PanelRow> = lines
        .into_iter()
        .skip(scroll)
        .take(body)
        .map(PanelRow::plain)
        .collect();
    rows.push(PanelRow::blank());
    rows.push(hint_row(&[
        ("/", "filter"),
        ("j/k", "scroll"),
        ("↵", "where bound"),
        ("esc", "back"),
    ]));
    rows
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let groups = visible_groups(ctx.ui, ctx.ui.width);
    if ctx.full() {
        full(ctx, &groups)
    } else if ctx.deep() {
        half(ctx, &groups)
    } else {
        normal(ctx, &groups)
    }
}

/// Number of unscrolled content lines rendered by the scrollable Keys views.
pub(crate) fn line_count(ctx: &SectionCtx) -> usize {
    let groups = visible_groups(ctx.ui, ctx.ui.width);
    if ctx.full() {
        full_lines(ctx.ui, &groups, ctx.cols).len()
    } else {
        half_lines(ctx.ui, &groups).len()
    }
}

fn half_line_rows(ui: &PanelUi, groups: &[KeyGroup]) -> Vec<Option<KeyRow>> {
    let mut rows = Vec::new();
    if filter_bar(ui).is_some() {
        rows.push(None);
    }
    if groups.is_empty() {
        rows.extend(no_matches(ui).into_iter().map(|_| None));
        return rows;
    }
    for group in groups {
        rows.push(None);
        rows.extend(group.rows.iter().cloned().map(Some));
        rows.push(None);
    }
    rows
}

fn full_line_rows(ui: &PanelUi, groups: &[KeyGroup]) -> Vec<Option<KeyRow>> {
    let mut rows = Vec::new();
    if filter_bar(ui).is_some() {
        rows.push(None);
    }
    if groups.is_empty() {
        rows.extend(no_matches(ui).into_iter().map(|_| None));
        return rows;
    }
    let (left_groups, right_groups) = split_full_groups(groups);
    let mut left: Vec<Option<KeyRow>> = left_groups
        .iter()
        .flat_map(|group| {
            std::iter::once(None)
                .chain(group.rows.iter().cloned().map(Some))
                .chain(std::iter::once(None))
        })
        .collect();
    let right_len: usize = right_groups.iter().map(|group| group.rows.len() + 2).sum();
    left.resize(left.len().max(right_len), None);
    rows.extend(left);
    rows
}

/// Find the first visible key row at or below the current line scroll.
pub(crate) fn top_row(ui: &PanelUi, viewport: usize) -> Option<KeyRow> {
    let groups = visible_groups(ui, ui.width);
    let rows = if ui.width == crate::layout::PanelWidth::Full {
        full_line_rows(ui, &groups)
    } else {
        half_line_rows(ui, &groups)
    };
    let visible = if ui.width == crate::layout::PanelWidth::Full {
        viewport.saturating_sub(2)
    } else {
        viewport
    };
    let max = rows.len().saturating_sub(visible);
    rows.into_iter()
        .skip(ui.scroll.min(max))
        .find_map(|row| row)
}

/// Explain the source of a displayed binding without performing any I/O.
pub(crate) fn where_bound(row: &KeyRow, bindings: &[crate::keymap_merge::Binding]) -> String {
    let prefix = format!("`{}` {}", row.chord, row.label);
    match row.origin {
        KeyOrigin::Cheatsheet => {
            let binding = bindings.iter().find(|binding| {
                binding.label == row.label && binding.chords.iter().any(|chord| chord == &row.chord)
            });
            match binding {
                Some(binding) => match binding.source {
                    crate::keymap_merge::Source::Registry => format!(
                        "{prefix} — `{}`, default binding; rebind with [keybinds] {} in config.toml (`thegn keys list` lists all)",
                        binding.id, binding.id
                    ),
                    crate::keymap_merge::Source::Config => {
                        format!("{prefix} — your [[actions]] entry `{}`", binding.id)
                    }
                    crate::keymap_merge::Source::Host | crate::keymap_merge::Source::ZoneTable => {
                        format!("{prefix} — run `thegn keys list` for its source")
                    }
                },
                None => format!("{prefix} — run `thegn keys list` for its source"),
            }
        }
        KeyOrigin::Section(section) => {
            format!(
                "{prefix} — the {} panel table (fixed, not rebindable)",
                section.label()
            )
        }
        KeyOrigin::Panel => format!("{prefix} — panel accordion key (fixed)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::PanelWidth;

    fn test_ui() -> PanelUi {
        PanelUi {
            docs: crate::panel::docs::PanelDocs {
                cfg_keys: crate::keyhint::cheatsheet_groups(&thegn_core::config::Config::default()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn filter_narrows_the_rows() {
        let mut ui = test_ui();
        let all = groups(&ui);
        ui.keys_filter = "stage".into();
        let filtered = groups(&ui);
        assert!(filtered.iter().flat_map(|group| &group.rows).all(|row| {
            row.chord.to_lowercase().contains("stage")
                || row.label.to_lowercase().contains("stage")
                || filtered.iter().any(|group| {
                    group.title.to_lowercase().contains("stage") && group.rows.contains(row)
                })
        }));
        assert!(
            filtered.iter().map(|group| group.rows.len()).sum::<usize>()
                < all.iter().map(|group| group.rows.len()).sum::<usize>()
        );
    }

    #[test]
    fn section_key_groups_are_present_and_gated_sections_are_absent() {
        let mut ui = test_ui();
        ui.off.push((Section::Issues, "disabled"));
        let groups = groups(&ui);
        assert!(groups.iter().any(|group| {
            group.title == "system ▸ logs"
                && group
                    .rows
                    .iter()
                    .any(|row| row.chord == "/" && row.label == "filter")
        }));
        assert!(groups.iter().any(|group| group.title == "git ▸ changes"));
        assert!(!groups.iter().any(|group| group.title.ends_with("▸ issues")));
    }

    #[test]
    fn where_bound_names_each_source() {
        let bindings = crate::keymap_merge::collect(&thegn_core::config::Config::default());
        let registry = bindings
            .iter()
            .find(|binding| {
                binding.source == crate::keymap_merge::Source::Registry
                    && !binding.chords.is_empty()
            })
            .expect("default registry binding");
        let registry_row = KeyRow {
            chord: registry.chords[0].clone(),
            label: registry.label.clone(),
            origin: KeyOrigin::Cheatsheet,
        };
        assert!(where_bound(&registry_row, &bindings).contains("default binding; rebind"));

        let section_row = KeyRow {
            chord: "/".into(),
            label: "filter".into(),
            origin: KeyOrigin::Section(Section::Logs),
        };
        assert!(where_bound(&section_row, &bindings).contains("logs panel table (fixed"));

        let panel_row = KeyRow {
            chord: "j/k".into(),
            label: "walk rows".into(),
            origin: KeyOrigin::Panel,
        };
        assert!(where_bound(&panel_row, &bindings).contains("panel accordion key (fixed)"));

        let missing = KeyRow {
            chord: "not-bound".into(),
            label: "unknown".into(),
            origin: KeyOrigin::Cheatsheet,
        };
        assert!(where_bound(&missing, &bindings).contains("thegn keys list` for its source"));
    }

    #[test]
    fn half_top_row_tracks_rendered_line_offset() {
        let ui = PanelUi {
            width: PanelWidth::Half,
            ..test_ui()
        };
        let groups = visible_groups(&ui, ui.width);
        let expected = groups[1].rows[0].clone();
        let target_line = groups[0].rows.len() + 3;
        assert_eq!(
            half_line_rows(&ui, &groups)[target_line],
            Some(expected.clone())
        );
        let mut ui = ui;
        ui.scroll = target_line;

        assert_eq!(top_row(&ui, 1), Some(expected));
    }

    #[test]
    fn line_count_matches_render_at_half_and_full_width() {
        for width in [PanelWidth::Half, PanelWidth::Full] {
            let mut ui = test_ui();
            ui.width = width;
            ui.keys_filter = "stage".into();
            let model = crate::chrome::FrameModel::default();
            let ctx = SectionCtx {
                model: &model,
                ui: &ui,
                cols: 80,
                rows: 500,
            };
            let rendered = content(&ctx);
            let actual = if width == PanelWidth::Full {
                rendered.len().saturating_sub(2)
            } else {
                rendered.len()
            };
            let expected = line_count(&ctx);
            assert_eq!(actual, expected, "{width:?}");
        }
    }

    #[test]
    fn unfiltered_scroll_extent_reaches_the_end_at_half_and_full_width() {
        let model = crate::chrome::FrameModel::default();
        for width in [PanelWidth::Half, PanelWidth::Full] {
            let mut ui = PanelUi { width, ..test_ui() };
            let cols = 80;
            let rows = 20;
            let extent =
                crate::panel::sections::scroll_extent(Section::Keys, &model, &ui, cols, rows)
                    .expect("expanded Keys view owns scrolling");
            let viewport = crate::panel::sections::scroll_viewport(&model, &ui, cols, rows);
            let visible = if width == PanelWidth::Full {
                viewport.saturating_sub(2)
            } else {
                viewport
            };
            assert!(visible > 0, "test geometry has visible body rows");
            ui.scroll = extent.max;
            let rendered = content(&SectionCtx {
                model: &model,
                ui: &ui,
                cols: cols - 2,
                rows: viewport,
            });
            let lines = if width == PanelWidth::Full {
                full_line_rows(&ui, &visible_groups(&ui, width))
            } else {
                half_line_rows(&ui, &visible_groups(&ui, width))
            };
            assert_eq!(extent.max + visible, lines.len(), "{width:?}");
            let last_visible = lines[extent.max..]
                .iter()
                .take(visible)
                .rev()
                .find_map(Option::as_ref)
                .expect("the scroll tail contains a key row");
            assert!(
                format!("{rendered:?}").contains(&last_visible.label),
                "{width:?}"
            );
        }
    }
}
