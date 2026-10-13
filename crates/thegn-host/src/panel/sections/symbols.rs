//! Symbols section — a document-symbol outline for the selected file.
//!
//! Populated from the language server when one is available, falling back to the
//! tree-sitter entity parser otherwise (see `run.rs`'s outline fetch). Selecting
//! a row navigates to the symbol's definition via the `open-file:` primitive.
//!
//! Widths:
//!   Normal (39): kind glyph + name (indented by nesting depth)
//!   Half   (75): + file:line on the right
//!   Full  (150): breadcrumb header + the same list

use std::path::PathBuf;

use crate::seg::{Line, seg};

use super::{PanelHit, PanelRow, Section, SectionCtx, d, g2, g3, hint_row, rule, t};

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

pub fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let data = &ctx.model.panel;
    if picker_open(ctx.ui, data) {
        return picker_view(ctx);
    }
    if ctx.ui.symbols_show_refs && data.symbols.is_empty() && !data.symbols_note.is_empty() {
        return vec![
            PanelRow::plain(Line::segs(vec![seg(g2(), data.symbols_note.clone())])),
            hint_row(&[("o", "outline"), ("f", "pick file"), ("j/k", "select")]),
        ];
    }
    if data.symbols.is_empty() {
        return empty_view(ctx);
    }
    if ctx.full() {
        full_view(ctx)
    } else {
        list_view(ctx, ctx.deep())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct OutlineKey {
    pub root: PathBuf,
    pub file: String,
    pub stamp: Option<(String, u32, u32)>,
}

pub(crate) fn outline_key(
    root: PathBuf,
    file: &str,
    changes: &[super::super::ChangeRow],
) -> OutlineKey {
    let stamp = changes
        .iter()
        .find(|change| change.path == file)
        .map(|change| (change.status.clone(), change.added, change.deleted));
    OutlineKey {
        root,
        file: file.to_string(),
        stamp,
    }
}

pub(crate) fn symbols_target(pick: Option<&str>, editor: Option<&str>) -> Option<String> {
    pick.or(editor).map(str::to_string)
}

pub(crate) struct OutlineDemand<'a> {
    pub visible: bool,
    pub show_refs: bool,
    pub target: Option<&'a OutlineKey>,
    pub cached: Option<&'a OutlineKey>,
    pub inflight: Option<&'a OutlineKey>,
    pub busy: bool,
}

pub(crate) fn outline_fetch_needed(demand: &OutlineDemand<'_>) -> bool {
    if demand.visible
        && !demand.show_refs
        && let Some(target) = demand.target
    {
        demand.cached != Some(target) && (demand.inflight != Some(target) || !demand.busy)
    } else {
        false
    }
}

pub(crate) fn accept_outline(current_root: &std::path::Path, key: &OutlineKey) -> bool {
    key.root == current_root
}

pub(crate) fn accept_refs(current_root: &std::path::Path, root: &std::path::Path) -> bool {
    root == current_root
}

pub(crate) fn leave_refs(ui: &mut super::super::PanelUi) -> bool {
    if !ui.symbols_show_refs {
        return false;
    }
    ui.symbols_show_refs = false;
    ui.symbols_cursor = 0;
    ui.cursor = 0;
    true
}

pub(crate) fn picker_open(ui: &super::super::PanelUi, panel: &super::super::PanelData) -> bool {
    ui.symbols_picker || panel.symbols_file.is_empty()
}

pub(crate) fn refs_note(label: &str, file: &str, server: bool, n: usize) -> String {
    if n > 0 {
        format!("{n} references to {label}")
    } else if server {
        format!("no references to {label}")
    } else {
        let ext = std::path::Path::new(file)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .map(|ext| format!(".{ext}"))
            .unwrap_or_else(|| "this file".to_string());
        format!("no references (no language server for {ext})")
    }
}

fn picker_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let changes = &ctx.model.panel.changes;
    let mut rows = if changes.is_empty() {
        vec![PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "no changed files — open a file in an editor",
        )]))]
    } else {
        changes
            .iter()
            .enumerate()
            .map(|(index, change)| {
                PanelRow::plain(Line::segs(vec![seg(g2(), change.path.clone())]))
                    .with_hit(PanelHit::Row(Section::Symbols, index))
            })
            .collect()
    };
    rows.push(hint_row(&[
        ("↵", "pick file"),
        ("f", "pick file"),
        ("j/k", "select"),
    ]));
    rows
}

fn empty_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let file = &ctx.model.panel.symbols_file;
    let msg = if file.is_empty() {
        "select a file to outline"
    } else {
        "no symbols in this file"
    };
    let mut rows = vec![PanelRow::plain(Line::segs(vec![seg(g2(), msg)]))];
    if ctx.full() && !file.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(g3(), file.clone())])));
    }
    rows.push(hint_row(&[
        ("↵", "go to def"),
        ("r", "refs"),
        ("o", "outline"),
        ("f", "pick file"),
        ("j/k", "select"),
    ]));
    rows
}

fn list_view(ctx: &SectionCtx, half: bool) -> Vec<PanelRow> {
    let data = &ctx.model.panel;
    let cursor = ctx.ui.symbols_cursor;
    let mut rows = Vec::new();

    for (i, s) in data.symbols.iter().enumerate() {
        let selected = i == cursor;
        let indent = "  ".repeat(s.depth as usize);
        let name_col = if selected { t() } else { d() };
        let location = format!(
            "{}:{}",
            s.file.rsplit('/').next().unwrap_or(&s.file),
            s.line
        );

        let left = vec![
            seg(g3(), indent.clone()),
            seg(g2(), format!("{} ", s.kind)),
            seg(
                name_col,
                truncate(&s.name, ctx.cols.saturating_sub(indent.len() + 12)),
            ),
        ];
        let line = if half {
            Line::split(left, vec![seg(g2(), location)])
        } else {
            Line::segs(left)
        };

        let row = PanelRow::plain(line).with_hit(PanelHit::Row(Section::Symbols, i));
        let row = if selected {
            row.with_bg(crate::seg::Tok::SelAccent)
        } else {
            row
        };
        rows.push(row);
    }

    rows.push(hint_row(&[
        ("↵", "go to def"),
        ("r", "refs"),
        ("o", "outline"),
        ("f", "pick file"),
        ("j/k", "select"),
    ]));
    rows
}

fn full_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let file = &ctx.model.panel.symbols_file;
    let mut rows = vec![
        PanelRow::plain(Line::segs(vec![seg(d(), "OUTLINE")])),
        PanelRow::plain(Line::segs(vec![seg(g3(), file.clone())])),
        rule(),
    ];
    rows.extend(list_view(ctx, true));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::FrameModel;
    use crate::panel::{PanelUi, SymbolRow};

    fn change(path: &str, added: u32, deleted: u32) -> super::super::super::ChangeRow {
        super::super::super::ChangeRow {
            status: "M".into(),
            stage: super::super::super::Stage::Unstaged,
            dir: String::new(),
            name: path.into(),
            path: path.into(),
            added,
            deleted,
            count: thegn_svc::git::DiffCount::Exact(added + deleted),
            submodule: None,
            incoming: false,
        }
    }

    fn make_ctx<'a>(model: &'a FrameModel, ui: &'a PanelUi) -> SectionCtx<'a> {
        SectionCtx {
            model,
            ui,
            cols: 40,
            rows: 20,
        }
    }

    fn sym(kind: &str, name: &str, line: u64, depth: u16) -> SymbolRow {
        SymbolRow {
            kind: kind.into(),
            name: name.into(),
            file: "src/a.rs".into(),
            line,
            col: 0,
            depth,
        }
    }

    #[test]
    fn empty_outline_prompts_for_a_file() {
        let model = FrameModel::default();
        let ui = PanelUi::default();
        let rows = content(&make_ctx(&model, &ui));
        // Prompt row + hint row, and no row hits into the Symbols section.
        assert!(rows.iter().all(|r| r.hit.is_none()));
        assert!(rows.len() >= 2);
    }

    #[test]
    fn renders_symbol_rows_with_hits() {
        let mut model = FrameModel::default();
        model.panel.symbols = vec![sym("struct", "Foo", 1, 0), sym("fn", "bar", 3, 1)];
        model.panel.symbols_file = "src/a.rs".into();
        let ui = PanelUi::default();
        let rows = content(&make_ctx(&model, &ui));
        // 2 symbol rows (each a hit) + the hint row.
        let hits = rows
            .iter()
            .filter(|r| matches!(r.hit, Some(PanelHit::Row(Section::Symbols, _))))
            .count();
        assert_eq!(hits, 2);
    }

    #[test]
    fn selected_row_has_sel_accent_bg() {
        let mut model = FrameModel::default();
        model.panel.symbols = vec![sym("fn", "a", 1, 0), sym("fn", "b", 2, 0)];
        let ui = PanelUi {
            symbols_cursor: 1,
            ..Default::default()
        };
        let rows = content(&make_ctx(&model, &ui));
        assert_eq!(rows[1].bg, Some(crate::seg::Tok::SelAccent));
    }

    #[test]
    fn same_rel_path_in_another_worktree_refetches() {
        let changes = [change("src/a.rs", 1, 0)];
        let cached = outline_key(PathBuf::from("/a"), "src/a.rs", &changes);
        let target = outline_key(PathBuf::from("/b"), "src/a.rs", &changes);
        assert!(outline_fetch_needed(&OutlineDemand {
            visible: true,
            show_refs: false,
            target: Some(&target),
            cached: Some(&cached),
            inflight: None,
            busy: false,
        }));
    }

    #[test]
    fn stale_root_result_is_dropped() {
        let key = outline_key(PathBuf::from("/old"), "src/a.rs", &[]);
        assert!(!accept_outline(PathBuf::from("/new").as_path(), &key));
    }

    #[test]
    fn edit_counts_change_refetches() {
        let old = outline_key(
            PathBuf::from("/repo"),
            "src/a.rs",
            &[change("src/a.rs", 1, 0)],
        );
        let edited = outline_key(
            PathBuf::from("/repo"),
            "src/a.rs",
            &[change("src/a.rs", 2, 0)],
        );
        assert!(outline_fetch_needed(&OutlineDemand {
            visible: true,
            show_refs: false,
            target: Some(&edited),
            cached: Some(&old),
            inflight: None,
            busy: false,
        }));
    }

    #[test]
    fn no_outline_fetch_while_panel_hidden() {
        let key = outline_key(PathBuf::from("/repo"), "src/a.rs", &[]);
        assert!(!outline_fetch_needed(&OutlineDemand {
            visible: false,
            show_refs: false,
            target: Some(&key),
            cached: None,
            inflight: None,
            busy: false,
        }));
    }

    #[test]
    fn esc_in_refs_returns_to_outline() {
        let mut ui = PanelUi {
            symbols_show_refs: true,
            symbols_cursor: 4,
            cursor: 4,
            ..Default::default()
        };
        assert!(leave_refs(&mut ui));
        assert!(!ui.symbols_show_refs);
        assert_eq!(ui.symbols_cursor, 0);
        assert_eq!(ui.cursor, 0);
        assert!(!leave_refs(&mut ui));
    }

    #[test]
    fn empty_refs_without_server_names_the_extension() {
        let note = refs_note("Widget", "src/widget.rs", false, 0);
        assert!(note.contains("no language server for .rs"));
        assert!(!note.contains("no symbols in this file"));
        let model = FrameModel {
            panel: crate::panel::PanelData {
                symbols_note: note,
                symbols_file: "refs: Widget".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let ui = PanelUi {
            symbols_show_refs: true,
            ..Default::default()
        };
        assert!(
            format!("{:?}", content(&make_ctx(&model, &ui))).contains("no language server for .rs")
        );
    }

    #[test]
    fn no_target_shows_changed_file_picker() {
        let mut model = FrameModel::default();
        model.panel.changes = vec![change("src/a.rs", 1, 0), change("src/b.rs", 0, 1)];
        let rows = content(&make_ctx(&model, &PanelUi::default()));
        let hits = rows
            .iter()
            .filter(|row| matches!(row.hit, Some(PanelHit::Row(Section::Symbols, _))))
            .count();
        assert_eq!(hits, 2);
    }

    #[test]
    fn pick_beats_editor_file() {
        assert_eq!(
            symbols_target(Some("src/picked.rs"), Some("src/editor.rs")),
            Some("src/picked.rs".into())
        );
    }
}
