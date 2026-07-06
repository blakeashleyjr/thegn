//! The System ▸ Environments section: one row per configured `[env.<name>]` with
//! a token/health glyph, its placement kind, and region/size. Reads
//! `model.panel.environments` (built off-loop by hydration from the config;
//! see `crate::env_ui`). Read-only today — authoring is the palette "New
//! environment…" wizard and `superzej env create`; binding is `superzej env set`.

use superzej_core::theme::Hue;

use crate::env_ui::EnvSnapshot;
use crate::seg::{Line, Seg, seg};

use super::{PanelRow, SectionCtx, d, g, g2, hue};

/// ● token present (green) · ✗ token missing (red) · ● no token needed (dim).
fn glyph(e: &EnvSnapshot) -> Seg {
    match e.token {
        Some(true) => seg(hue(Hue::Green), "●"),
        Some(false) => seg(hue(Hue::Red), "✗"),
        None => seg(g(), "●"),
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
        return rows;
    }
    for e in envs {
        let mut left = vec![
            glyph(e),
            seg(d(), format!(" {}", e.name)),
            seg(g(), format!("  {}", e.kind)),
        ];
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
        rows.push(PanelRow::plain(Line::split(left, vec![right])));
    }
    // Authoring/binding pointers (read-only section; the wizard + CLI do the rest).
    rows.push(PanelRow::plain(Line::segs(vec![seg(
        g(),
        "palette → ＋ New environment…  ·  bind: superzej env set <name>",
    )])));
    rows
}
