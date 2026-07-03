//! "My Work" section — the unified, cross-repo, cross-tool actionable feed.
//!
//! Rows are grouped (Review requested · Needs attention · Assigned to me) and
//! within a group sorted by urgency. Each work row carries a `Row` hit; group
//! headers do not, so the live `ui.cursor` walks only actionable rows and the
//! Enter handler can index the same `work::sort_rows` order.

use superzej_core::work::{WorkGroup, WorkKind, WorkRow};

use crate::seg::{Line, seg, sp};

use super::{PanelHit, PanelRow, Section, SectionCtx, d, f, g, g2, hint_row, hue, rule, t};

fn provider_sigil(provider: &str) -> &'static str {
    match provider {
        "linear" => "⬡",
        "github" => "⊙",
        "jira" => "⬢",
        _ => "○",
    }
}

fn kind_hue(kind: WorkKind) -> crate::seg::Tok {
    match kind {
        WorkKind::Pr => hue(superzej_core::theme::Hue::Green),
        WorkKind::Issue => hue(superzej_core::theme::Hue::Blue),
        WorkKind::Notification => hue(superzej_core::theme::Hue::Amber),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// The feed in display order — the single source of truth shared by the
/// renderer and the Enter handler so cursor indices line up.
pub fn ordered_rows(panel: &crate::panel::PanelData) -> Vec<WorkRow> {
    let mut rows = panel.my_work.clone();
    superzej_core::work::sort_rows(&mut rows);
    rows
}

pub fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let cols = ctx.cols;
    let rows_data = ordered_rows(&ctx.model.panel);

    if rows_data.is_empty() {
        return empty_rows(ctx);
    }

    let all = crate::panel::scope::mine_all();
    let mut out = Vec::new();
    // The full view leads with a count banner (also makes it visually distinct
    // from the half view, which is otherwise identical at wide column counts).
    if ctx.full() {
        let scope = if all {
            " · all repos"
        } else {
            " · this repo"
        };
        out.push(PanelRow::plain(Line::segs(vec![
            seg(d(), format!("MY WORK — {} items", rows_data.len())),
            seg(g2(), scope.to_string()),
        ])));
        out.push(rule());
    }
    let mut last_group: Option<WorkGroup> = None;

    // The enumerate index doubles as the actionable-row index: every `WorkRow`
    // emits exactly one `Row` hit (group headers carry none), and `ui.cursor`
    // counts only `Row` hits — so `ai` lines up with the cursor.
    for (ai, row) in rows_data.iter().enumerate() {
        // Group header (no hit — skipped by the cursor).
        if last_group != Some(row.group) {
            if last_group.is_some() {
                out.push(PanelRow::plain(Line::Blank));
            }
            out.push(PanelRow::plain(Line::segs(vec![seg(
                d(),
                row.group.label().to_uppercase(),
            )])));
            out.push(rule());
            last_group = Some(row.group);
        }

        // `⊙ acme/widget #42 Title…` — sigil + repo + number + title, plus a
        // ◈ marker when a worktree is already linked.
        let sigil = provider_sigil(&row.provider).to_string();
        let link_mark = if row.is_linked() { "◈ " } else { "" };
        let repo_part = if row.repo.is_empty() {
            String::new()
        } else {
            format!("{} ", short_repo(&row.repo))
        };
        let prefix_len =
            2 + link_mark.len() + repo_part.chars().count() + row.number.chars().count() + 1;
        let title = truncate(&row.title, cols.saturating_sub(prefix_len));

        let mut segs = vec![seg(kind_hue(row.kind), sigil), seg(g(), " ".to_string())];
        if !link_mark.is_empty() {
            segs.push(seg(g2(), link_mark.to_string()));
        }
        if !repo_part.is_empty() {
            segs.push(seg(g2(), repo_part));
        }
        segs.push(seg(d(), row.number.clone()));
        segs.push(seg(g(), " ".to_string()));
        segs.push(seg(t(), title));

        out.push(PanelRow::plain(Line::segs(segs)).with_hit(PanelHit::Row(Section::Mine, ai)));

        // In the deeper views, add a muted second line with the URL host/repo.
        if ctx.deep() && !row.url.is_empty() {
            out.push(PanelRow::plain(Line::segs(vec![
                sp(2),
                seg(f(), truncate(&row.url, cols.saturating_sub(2))),
            ])));
        }

        if out.len() >= ctx.rows.saturating_sub(1) {
            break;
        }
    }

    out.push(hint_row(&[
        ("↵", "open"),
        ("b", "branch"),
        ("o", "browser"),
        ("a", if all { "this repo" } else { "all repos" }),
        ("R", "refresh"),
    ]));
    out
}

/// Drop the leading `owner/` of `owner/repo` in tight widths.
fn short_repo(repo: &str) -> String {
    repo.rsplit('/').next().unwrap_or(repo).to_string()
}

fn empty_rows(ctx: &SectionCtx) -> Vec<PanelRow> {
    let mut rows = vec![
        PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "Nothing waiting on you.".to_string(),
        )])),
        PanelRow::plain(Line::Blank),
    ];
    if ctx.deep() {
        let where_ = if crate::panel::scope::mine_all() {
            "PRs across every repo show up here."
        } else {
            "PRs for this repo show up here (a = all repos)."
        };
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "Assigned issues, review requests, and your open ".to_string(),
        )])));
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g2(),
            where_.to_string(),
        )])));
        rows.push(PanelRow::plain(Line::Blank));
    }
    if ctx.full() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "Press R to refresh the feed.".to_string(),
        )])));
    }
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(g2(), "Configure ".to_string()),
        seg(f(), "[issues] providers".to_string()),
        seg(g2(), " and authenticate ".to_string()),
        seg(f(), "gh".to_string()),
        seg(g2(), ".".to_string()),
    ])));
    rows
}

#[cfg(test)]
mod spec {
    use super::*;
    use crate::panel::PanelData;

    fn row(group: WorkGroup, number: &str, urgency: u8) -> WorkRow {
        WorkRow {
            group,
            number: number.into(),
            urgency,
            title: "t".into(),
            provider: "github".into(),
            ..Default::default()
        }
    }

    #[test]
    fn ordered_rows_sorts_by_group_then_urgency() {
        let panel = PanelData {
            my_work: vec![
                row(WorkGroup::Assigned, "ABC-2", 1),
                row(WorkGroup::ReviewRequested, "#9", 5),
                row(WorkGroup::Assigned, "ABC-1", 9),
            ],
            ..Default::default()
        };
        let ordered = ordered_rows(&panel);
        assert_eq!(ordered[0].group, WorkGroup::ReviewRequested);
        assert_eq!(ordered[1].number, "ABC-1"); // higher urgency first within group
    }
}
