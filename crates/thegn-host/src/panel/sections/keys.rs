//! The Keys section: the effective cheatsheet, panel controls, and each
//! section's keys, with a local filter and source lookup.

use std::collections::BTreeSet;

use crate::panel::{PanelUi, SECTION_ORDER, Section, SectionCtx};
use crate::seg::{Line, Seg, Tok, seg, sp};

use super::{PanelRow, d, f, g2, g3, hint_row, t, two_col};

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

fn full_lines(ui: &PanelUi, groups: &[KeyGroup], cols: usize) -> Vec<Line> {
    let mut lines = filter_bar(ui).map_or_else(Vec::new, |row| vec![row.line]);
    if groups.is_empty() {
        lines.extend(no_matches(ui));
        return lines;
    }

    let total: usize = groups.iter().map(|group| group.rows.len() + 2).sum();
    let mut left = Vec::new();
    let mut right = Vec::new();
    for group in groups {
        let group = group_lines(group);
        if left.len() + group.len() <= total.div_ceil(2) || left.is_empty() {
            left.extend(group);
        } else {
            right.extend(group);
        }
    }
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

fn visible_groups(ctx: &SectionCtx) -> Vec<KeyGroup> {
    let mut groups = groups(ctx.ui);
    if ctx.ui.keys_filter.is_empty() && ctx.ui.width == crate::layout::PanelWidth::Normal {
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
    let groups = visible_groups(ctx);
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
    let groups = visible_groups(ctx);
    if ctx.full() {
        full_lines(ctx.ui, &groups, ctx.cols).len()
    } else {
        half_lines(ctx.ui, &groups).len()
    }
}

/// Find the first filtered key row at or below the current line scroll.
pub(crate) fn top_row(ui: &PanelUi) -> Option<KeyRow> {
    groups(ui)
        .into_iter()
        .flat_map(|group| group.rows)
        .nth(ui.scroll)
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
    use crate::panel::PanelWidth;

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
}
