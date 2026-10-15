//! "My Work" section — the unified, cross-repo, cross-tool actionable feed.
//!
//! Rows are grouped (Review requested · Needs attention · Assigned to me) and
//! within a group sorted by urgency. Each work row carries a `Row` hit; group
//! headers do not, so the live `ui.cursor` walks only actionable rows and the
//! Enter handler can index the same `work::sort_rows` order.

use thegn_core::pipeline_report::{display_cell, display_clip};
use thegn_core::work::{WorkGroup, WorkKind, WorkRow};

use crate::seg::{Line, seg, sp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnterTarget {
    Jump(crate::sidebar::RowTarget),
    PrView {
        owner: String,
        repo: String,
        number: u64,
    },
    Browser(String),
    Nothing,
}

pub(crate) fn enter_target(
    row: &WorkRow,
    groups: &[(String, usize)],
    db: &[crate::sidebar::DbWorktree],
    active_repo_path: Option<&str>,
) -> EnterTarget {
    if let Some(path) = row.worktree_path.as_deref()
        && let Some(target) = crate::nav::worktree_row_target(path, groups, db)
    {
        return EnterTarget::Jump(target);
    }
    if row.kind == WorkKind::Pr
        && row
            .repo_root
            .as_deref()
            .is_some_and(|root| Some(root) == active_repo_path)
        && let Some((owner, repo)) = thegn_core::forge::model::owner_repo_from_url(&row.url)
        && let Ok(number) = row.number.trim_start_matches('#').parse::<u64>()
    {
        return EnterTarget::PrView {
            owner,
            repo,
            number,
        };
    }
    if !row.url.is_empty() {
        EnterTarget::Browser(row.url.clone())
    } else {
        EnterTarget::Nothing
    }
}

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
        WorkKind::Pr => hue(thegn_core::theme::Hue::Green),
        WorkKind::Issue => hue(thegn_core::theme::Hue::Blue),
        WorkKind::Notification => hue(thegn_core::theme::Hue::Amber),
    }
}

/// The feed in display order — the single source of truth shared by the
/// renderer and the Enter handler so cursor indices line up.
pub fn ordered_rows(panel: &crate::panel::PanelData) -> Vec<WorkRow> {
    let mut rows = panel.my_work.clone();
    thegn_core::work::sort_rows(&mut rows);
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
    push_note(&mut out, ctx);
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
        let repo = display_cell(&short_repo(&row.repo));
        let number = display_cell(&row.number);
        let repo_part = if row.repo.is_empty() {
            String::new()
        } else {
            format!("{repo} ")
        };
        let prefix_len =
            2 + link_mark.len() + repo_part.chars().count() + number.chars().count() + 1;
        let title = display_clip(&row.title, cols.saturating_sub(prefix_len));

        let mut segs = vec![seg(kind_hue(row.kind), sigil), seg(g(), " ".to_string())];
        if !link_mark.is_empty() {
            segs.push(seg(g2(), link_mark.to_string()));
        }
        if !repo_part.is_empty() {
            segs.push(seg(g2(), repo_part));
        }
        segs.push(seg(d(), number));
        segs.push(seg(g(), " ".to_string()));
        segs.push(seg(t(), title));

        out.push(PanelRow::plain(Line::segs(segs)).with_hit(PanelHit::Row(Section::Mine, ai)));

        // In the deeper views, add a muted second line with the URL host/repo.
        if ctx.deep() && !row.url.is_empty() {
            out.push(PanelRow::plain(Line::segs(vec![
                sp(2),
                seg(f(), display_clip(&row.url, cols.saturating_sub(2))),
            ])));
        }
    }

    out.push(hint_row(&[
        ("↵", "go"),
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

/// Surface the aggregator's note (e.g. "no `origin` remote — PR search needs
/// it for repo scope") so a degraded fetch never looks like a clean feed.
fn push_note(out: &mut Vec<PanelRow>, ctx: &SectionCtx) {
    let note = &ctx.model.panel.my_work_note;
    if note.is_empty() {
        return;
    }
    out.push(PanelRow::plain(Line::segs(vec![
        seg(hue(thegn_core::theme::Hue::Amber), "⚠ ".to_string()),
        seg(g2(), display_clip(note, ctx.cols.saturating_sub(2))),
    ])));
}

fn empty_rows(ctx: &SectionCtx) -> Vec<PanelRow> {
    let mut rows = Vec::new();
    push_note(&mut rows, ctx);
    rows.push(PanelRow::plain(Line::segs(vec![seg(
        g2(),
        "Nothing waiting on you.".to_string(),
    )])));
    rows.push(PanelRow::plain(Line::Blank));
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

    #[test]
    fn truncate_replaces_terminal_and_format_controls_before_cutting() {
        let rendered = display_clip("ok\r\nforged\u{202e}\u{200b}", 40);
        assert!(
            !rendered
                .chars()
                .any(|ch| matches!(ch, '\r' | '\n' | '\u{202e}' | '\u{200b}'))
        );
        assert!(rendered.contains('\u{fffd}'));
    }

    #[test]
    fn content_replaces_hostile_work_row_cells() {
        let mut work = row(WorkGroup::Assigned, "#1\n", 1);
        work.title = "a\r\nb\x1b]52;c;QQ==\x07".into();
        work.repo = "o/r\r".into();
        work.url = "https://example.test/a\n".into();
        let model = crate::chrome::FrameModel {
            panel: PanelData {
                my_work: vec![work],
                ..Default::default()
            },
            ..Default::default()
        };
        let ui = crate::panel::PanelUi {
            open: Section::Mine,
            width: crate::layout::PanelWidth::Half,
            ..Default::default()
        };
        let ctx = SectionCtx {
            model: &model,
            ui: &ui,
            cols: 75,
            rows: 32,
        };
        let text = content(&ctx)
            .iter()
            .filter_map(|row| match &row.line {
                Line::Segs(segs) => {
                    Some(segs.iter().map(|seg| seg.text.clone()).collect::<String>())
                }
                _ => None,
            })
            .collect::<String>();
        assert!(
            !text
                .chars()
                .any(|ch| matches!(ch, '\r' | '\n' | '\x1b' | '\x07')),
            "{text:?}"
        );
        assert!(
            text.contains('\u{fffd}'),
            "hostile cells are visibly replaced: {text:?}"
        );
    }

    #[test]
    fn mine_enter_on_a_linked_row_jumps() {
        let row = WorkRow {
            worktree_path: Some("/wt".into()),
            ..Default::default()
        };
        assert_eq!(
            enter_target(&row, &[("/wt".into(), 1)], &[], None),
            EnterTarget::Jump(crate::sidebar::RowTarget::Tab(0, 1))
        );
    }

    #[test]
    fn mine_enter_on_a_same_repo_pr_opens_the_pr_view() {
        let row = WorkRow {
            kind: WorkKind::Pr,
            repo_root: Some("/repo".into()),
            number: "#12".into(),
            url: "https://github.com/acme/widget/pull/12".into(),
            ..Default::default()
        };
        assert_eq!(
            enter_target(&row, &[], &[], Some("/repo")),
            EnterTarget::PrView {
                owner: "acme".into(),
                repo: "widget".into(),
                number: 12
            }
        );
    }

    #[test]
    fn mine_enter_on_a_foreign_pr_opens_the_browser() {
        let row = WorkRow {
            kind: WorkKind::Pr,
            repo_root: Some("/other".into()),
            number: "#12".into(),
            url: "https://github.com/acme/widget/pull/12".into(),
            ..Default::default()
        };
        assert_eq!(
            enter_target(&row, &[], &[], Some("/repo")),
            EnterTarget::Browser(row.url)
        );
    }
}
