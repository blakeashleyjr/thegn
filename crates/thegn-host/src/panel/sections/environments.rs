//! The System ▸ Environments section: one row per configured `[env.<name>]` with
//! a token/health glyph, its placement kind, and region/size. Reads
//! `model.panel.environments` (built off-loop by hydration from the config;
//! see `crate::env_ui`). Read-only today — authoring is the palette "New
//! environment…" wizard and `thegn env create`; binding is `thegn env set`.

use thegn_core::pipeline_report::{display_cell, display_clip};
use thegn_core::theme::Hue;

use crate::env_ui::EnvSnapshot;
use crate::seg::{Line, Seg, seg};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, d, g, g2, hint_row, hue, list_detail, rule, t,
    wrap_text,
};

fn toolchain_label(toolchain: &crate::toolchain_ui::ToolchainStatus) -> String {
    let mut label = format!(
        "{} · {} · {}",
        toolchain.provider, toolchain.tier, toolchain.state
    );
    if !toolchain.missing_tools.is_empty() {
        label.push_str(&format!(
            " · missing {}",
            toolchain.missing_tools.join(", ")
        ));
    }
    if let Some(reason) = &toolchain.reason {
        label.push_str(&format!(" · {reason}"));
    }
    label
}

/// ● token present (green) · ✗ token missing (red) · ● no token needed (dim).
fn glyph(e: &EnvSnapshot) -> Seg {
    let glyphs = crate::caps::active_glyphs();
    match e.token {
        Some(true) => seg(hue(Hue::Green), glyphs.dot_filled),
        Some(false) => seg(hue(Hue::Red), glyphs.cross),
        None => seg(g(), glyphs.dot_filled),
    }
}

/// Closed-row summary: env count + a `✗N` badge when any provider token is missing.
pub(super) fn summary(model: &crate::chrome::FrameModel) -> Vec<Seg> {
    let envs = &model.panel.environments;
    if envs.is_empty() {
        return vec![seg(g2(), "—")];
    }
    let missing = envs.iter().filter(|e| e.token == Some(false)).count();
    let mut v = vec![seg(g(), format!("{}", envs.len()))];
    if missing > 0 {
        v.push(seg(g(), " "));
        v.push(seg(hue(Hue::Red), format!("✗{missing}")));
    }
    v
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let envs = &ctx.model.panel.environments;
    let mut rows: Vec<PanelRow> = Vec::new();
    if ctx.full() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(d(), "ENVIRONMENTS")])));
    }
    if envs.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g(),
            "no environments — palette → ＋ New environment…",
        )])));
        if let Some(toolchain) = &ctx.model.panel.toolchain {
            rows.push(PanelRow::plain(Line::segs(vec![seg(
                d(),
                "WORKTREE TOOLCHAIN",
            )])));
            rows.push(PanelRow::plain(Line::segs(vec![seg(
                g(),
                toolchain_label(toolchain),
            )])));
        }
        return rows;
    }
    if ctx.full() {
        let list_w = 45_usize.min(ctx.cols / 2);
        let cursor = ctx.ui.cursor.min(envs.len().saturating_sub(1));
        let list: Vec<(Vec<Seg>, Option<usize>)> = envs
            .iter()
            .enumerate()
            .map(|(i, env)| {
                let selected = i == cursor;
                let glyphs = crate::caps::active_glyphs();
                let mut line = vec![
                    seg(
                        if selected { t() } else { g() },
                        if selected { glyphs.arrow_right } else { "  " },
                    ),
                    glyph(env),
                    seg(
                        if selected { t() } else { d() },
                        format!(" {}", display_cell(&env.name)),
                    ),
                    seg(g(), format!("  {}", display_cell(&env.kind))),
                ];
                if env.bound {
                    line.push(seg(
                        hue(Hue::Green),
                        format!("  {} bound", glyphs.dot_filled),
                    ));
                }
                if !env.region.is_empty() {
                    line.push(seg(g(), format!("  {}", display_cell(&env.region))));
                }
                if !env.size.is_empty() {
                    line.push(seg(g(), format!(" {}", display_cell(&env.size))));
                }
                match env.token {
                    Some(true) => line.push(seg(g(), " token ✓")),
                    Some(false) => line.push(seg(hue(Hue::Red), " token ✗")),
                    None => {}
                }
                (line, Some(i))
            })
            .collect();
        let env = &envs[cursor];
        let detail_w = ctx.cols.saturating_sub(list_w + 2);
        let mut detail = Vec::new();
        for (label, value) in &env.detail {
            let prefix = format!("  {label:<9}");
            let value_width = detail_w.saturating_sub(prefix.chars().count()).max(1);
            let wrapped = wrap_text(&display_cell(value), value_width);
            for (line_index, line) in wrapped.iter().enumerate() {
                detail.push(vec![
                    seg(
                        g(),
                        if line_index == 0 {
                            prefix.clone()
                        } else {
                            "           ".to_string()
                        },
                    ),
                    seg(g(), display_clip(line, value_width)),
                ]);
            }
        }
        rows.push(PanelRow::plain(Line::segs(vec![seg(d(), "ENVIRONMENTS")])));
        rows.push(rule());
        rows.extend(list_detail(
            Section::Environments,
            &list,
            &detail,
            cursor,
            list_w,
            ctx.rows.saturating_sub(rows.len() + 1),
        ));
        rows.push(environment_hints(envs.get(cursor)));
        if let Some(toolchain) = &ctx.model.panel.toolchain {
            rows.push(PanelRow::plain(Line::segs(vec![seg(
                d(),
                format!("WORKTREE TOOLCHAIN  {}", toolchain_label(toolchain)),
            )])));
        }
        return rows;
    }
    // Each env row carries a `Row` hit so the enumerate index lines up with
    // `ui.cursor` + `panel.environments` — the action keys target `envs[cursor]`.
    for (i, e) in envs.iter().enumerate() {
        let mut left = vec![
            glyph(e),
            seg(d(), format!(" {}", e.name)),
            seg(g(), format!("  {}", e.kind)),
        ];
        if e.bound {
            left.push(seg(
                hue(Hue::Green),
                format!("  {} bound", crate::caps::active_glyphs().dot_filled),
            ));
        }
        if !e.region.is_empty() {
            left.push(seg(g(), format!("  {}", e.region)));
        }
        if !e.size.is_empty() {
            left.push(seg(g(), format!(" {}", e.size)));
        }
        let right = match e.token {
            Some(true) => seg(g(), "token ✓".to_string()),
            Some(false) => seg(hue(Hue::Red), "token ✗".to_string()),
            None => seg(g(), String::new()),
        };
        rows.push(
            PanelRow::plain(Line::split(left, vec![right]))
                .with_hit(PanelHit::Row(Section::Environments, i)),
        );
        if ctx.deep() && i == ctx.ui.cursor {
            rows.extend(detail_rows(e, ctx.cols));
        }
    }
    rows.push(environment_hints(envs.get(ctx.ui.cursor)));
    if let Some(toolchain) = &ctx.model.panel.toolchain {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            d(),
            format!("WORKTREE TOOLCHAIN  {}", toolchain_label(toolchain)),
        )])));
    }
    rows
}

fn environment_hints(selected: Option<&EnvSnapshot>) -> PanelRow {
    let enter_hint = selected
        .map(|env| if env.bound { "bound" } else { "rebind here" })
        .unwrap_or("rebind here");
    let mut hints = vec![("enter", enter_hint)];
    if selected.is_some_and(|env| env.placement == "provider") {
        hints.push(("t", "test"));
    }
    hints.extend([("x", "remove"), ("n", "new…")]);
    hint_row(&hints)
}

fn detail_rows(env: &EnvSnapshot, width: usize) -> Vec<PanelRow> {
    env.detail
        .iter()
        .map(|(label, value)| {
            PanelRow::plain(Line::segs(vec![
                seg(g(), format!("  {label:<9}")),
                seg(g(), display_clip(value, width.saturating_sub(13).max(1))),
            ]))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::FrameModel;
    use crate::layout::PanelWidth;
    use crate::panel::sections::SectionCtx;
    use crate::panel::{PanelData, PanelUi};

    #[test]
    fn bound_marker_renders_on_only_the_bound_env_row() {
        let model = FrameModel {
            panel: PanelData {
                environments: vec![
                    EnvSnapshot {
                        name: "a".into(),
                        kind: "local".into(),
                        placement: "local".into(),
                        region: String::new(),
                        size: String::new(),
                        bound: false,
                        token: None,
                        detail: Vec::new(),
                    },
                    EnvSnapshot {
                        name: "b".into(),
                        kind: "provider".into(),
                        placement: "provider".into(),
                        region: String::new(),
                        size: String::new(),
                        bound: true,
                        token: Some(false),
                        detail: Vec::new(),
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let ui = PanelUi {
            open: Section::Environments,
            width: PanelWidth::Normal,
            ..Default::default()
        };
        let ctx = SectionCtx {
            model: &model,
            ui: &ui,
            cols: 80,
            rows: 20,
        };
        let rows = content(&ctx);
        let text = |row: &PanelRow| match &row.line {
            Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => l
                .iter()
                .chain(r)
                .map(|part| part.text.as_str())
                .collect::<String>(),
            Line::Segs(parts) => parts.iter().map(|part| part.text.as_str()).collect(),
            Line::Blank | Line::Fill { .. } => String::new(),
        };

        let a = text(&rows[0]);
        let b = text(&rows[1]);
        assert!(!a.contains("bound"));
        assert!(b.contains("bound"));
    }

    fn env(name: &str, target: &str) -> EnvSnapshot {
        EnvSnapshot {
            name: name.into(),
            kind: "ssh".into(),
            placement: "ssh".into(),
            region: String::new(),
            size: String::new(),
            bound: false,
            token: None,
            detail: vec![
                ("placement".into(), "ssh".into()),
                ("target".into(), target.into()),
                ("data".into(), "in_env".into()),
            ],
        }
    }

    fn frame(envs: Vec<EnvSnapshot>) -> FrameModel {
        FrameModel {
            panel: PanelData {
                environments: envs,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn row_text(row: &PanelRow) -> String {
        match &row.line {
            Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => {
                l.iter().chain(r).map(|part| part.text.as_str()).collect()
            }
            Line::Segs(parts) => parts.iter().map(|part| part.text.as_str()).collect(),
            Line::Blank | Line::Fill { .. } => String::new(),
        }
    }

    #[test]
    fn half_width_shows_detail_under_the_cursor_env_only() {
        let model = frame(vec![env("one", "box1"), env("two", "box2")]);
        let ui = PanelUi {
            open: Section::Environments,
            width: PanelWidth::Half,
            cursor: 1,
            ..Default::default()
        };
        let ctx = SectionCtx {
            model: &model,
            ui: &ui,
            cols: 50,
            rows: 20,
        };
        let rows = content(&ctx);
        assert!(!row_text(&rows[0]).contains("target"));
        assert!(
            rows[1..]
                .iter()
                .any(|row| row_text(row).contains("target") && row.hit.is_none())
        );
        assert!(
            !rows
                .iter()
                .any(|row| row_text(row).contains("placement") && row_text(row).contains("one"))
        );

        let normal = PanelUi {
            open: Section::Environments,
            width: PanelWidth::Normal,
            cursor: 1,
            ..Default::default()
        };
        let normal_ctx = SectionCtx {
            model: &model,
            ui: &normal,
            cols: 50,
            rows: 20,
        };
        assert_eq!(content(&normal_ctx).len(), 3);
    }

    #[test]
    fn full_width_lists_envs_beside_the_cursor_detail() {
        let model = frame(vec![env("one", "box1"), env("two", "box2")]);
        let ui = PanelUi {
            open: Section::Environments,
            width: PanelWidth::Full,
            cursor: 0,
            ..Default::default()
        };
        let ctx = SectionCtx {
            model: &model,
            ui: &ui,
            cols: 120,
            rows: 20,
        };
        let rows = content(&ctx);
        assert!(rows.iter().any(|row| {
            let text = row_text(row);
            text.contains("one") && text.contains("placement")
        }));
        for row in &rows {
            if let Some(PanelHit::Row(Section::Environments, index)) = row.hit {
                assert!(index < model.panel.environments.len());
            }
            if row_text(row).contains("data") {
                assert!(row.hit.is_none());
            }
        }
    }
}
