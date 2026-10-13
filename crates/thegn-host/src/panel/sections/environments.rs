//! The System ▸ Environments section: one row per configured `[env.<name>]` with
//! a token/health glyph, its placement kind, and region/size. Reads
//! `model.panel.environments` (built off-loop by hydration from the config;
//! see `crate::env_ui`). Read-only today — authoring is the palette "New
//! environment…" wizard and `thegn env create`; binding is `thegn env set`.

use thegn_core::theme::Hue;

use crate::env_ui::EnvSnapshot;
use crate::seg::{Line, Seg, seg};

use super::{PanelHit, PanelRow, Section, SectionCtx, d, g, g2, hint_row, hue};

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
    }
    let selected = envs.get(ctx.ui.cursor);
    let enter_hint = selected
        .map(|env| if env.bound { "bound" } else { "rebind here" })
        .unwrap_or("rebind here");
    let mut hints = vec![("enter", enter_hint)];
    if selected.is_some_and(|env| env.placement == "provider") {
        hints.push(("t", "test"));
    }
    hints.extend([("x", "remove"), ("n", "new…")]);
    rows.push(hint_row(&hints));
    if let Some(toolchain) = &ctx.model.panel.toolchain {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            d(),
            format!("WORKTREE TOOLCHAIN  {}", toolchain_label(toolchain)),
        )])));
    }
    rows
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
                    },
                    EnvSnapshot {
                        name: "b".into(),
                        kind: "provider".into(),
                        placement: "provider".into(),
                        region: String::new(),
                        size: String::new(),
                        bound: true,
                        token: Some(false),
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
}
