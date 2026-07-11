//! Issues section — browse tracked issues from Linear / GitHub / Jira.
//!
//! Three view widths:
//!   Normal (39 cols): compact one-liners, status glyph + number + title.
//!   Half   (75 cols): two-line rows with priority/labels/assignee.
//!   Full   (150 cols): left list + right detail panel side-by-side.

use thegn_core::issue::{Issue, IssuePriority, IssueStatus};
use thegn_core::theme::Hue;

use crate::seg::{Line, Seg, seg, sp};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, d, f, g, g2, g3, hint_row, hue, rule, t, two_col,
};

// ---- status colour mapping --------------------------------------------------

fn status_hue(s: IssueStatus) -> crate::seg::Tok {
    match s {
        IssueStatus::InProgress => hue(Hue::Amber),
        IssueStatus::Todo => hue(Hue::Blue),
        IssueStatus::Backlog => g2(),
        IssueStatus::Done => hue(Hue::Green),
        IssueStatus::Cancelled => g3(),
    }
}

fn priority_hue(p: IssuePriority) -> crate::seg::Tok {
    match p {
        IssuePriority::Urgent => hue(Hue::Red),
        IssuePriority::High => hue(Hue::Amber),
        IssuePriority::Medium => hue(Hue::Blue),
        IssuePriority::Low => g(),
        IssuePriority::None => g3(),
    }
}

fn provider_sigil(provider: &str) -> &'static str {
    match provider {
        "linear" => "⬡",
        "github" => "⊙",
        "jira" => "⬢",
        _ => "○",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

// ---- sorted + filtered issue list ------------------------------------------

/// Returns issues sorted by priority (Urgent first, Done/Cancelled last) and
/// filtered by the active text query and project filter.
fn sorted_issues<'a>(ctx: &'a SectionCtx) -> Vec<&'a Issue> {
    let data = &ctx.model.panel;
    let query = ctx.ui.issues_filter.to_lowercase();
    let project = ctx.ui.issues_project_filter.as_deref();

    let mut list: Vec<&Issue> = data
        .tracker_issues
        .iter()
        .filter(|i| {
            if !query.is_empty() {
                let haystack = format!("{} {}", i.number, i.title).to_lowercase();
                if !haystack.contains(&query) {
                    return false;
                }
            }
            if let Some(pid) = project
                && !i.project_ids.iter().any(|p| p == pid)
            {
                return false;
            }
            true
        })
        .collect();

    list.sort_by_key(|i| {
        // Active issues sort by priority descending; done/cancelled float to bottom.
        let terminal = matches!(i.status, IssueStatus::Done | IssueStatus::Cancelled);
        (terminal, i.priority) // (bool, IssuePriority) — IssuePriority derives Ord
    });
    list
}

// ---- main entry point -------------------------------------------------------

pub fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.full() {
        full_view(ctx)
    } else if ctx.deep() {
        half_view(ctx)
    } else {
        normal_view(ctx)
    }
}

// ---- Normal view (39 cols) --------------------------------------------------

fn normal_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let cols = ctx.cols;
    let issues = sorted_issues(ctx);

    if issues.is_empty() {
        return empty_rows(ctx.model.panel.issues_configured);
    }

    let mut rows = Vec::new();
    let cursor = ctx.ui.issues_cursor;
    let data = &ctx.model.panel;

    for (i, issue) in issues.iter().enumerate() {
        let linked = data.tracker_links.contains(&issue.id);
        let selected = i == cursor;

        // Compact: `● ABC-123 Title…` (⊘ when blocked)
        let blocked = !issue.blocked_by.is_empty();
        let glyph_str: String = if blocked {
            "⊘".into()
        } else {
            issue.status.glyph().to_string()
        };
        let link_mark = if linked { "◈ " } else { "" };
        // Budget: 1 glyph + 1 space + number + 1 space + title
        let number_part = &issue.number;
        let budget = cols
            .saturating_sub(1) // glyph
            .saturating_sub(1) // space
            .saturating_sub(number_part.len())
            .saturating_sub(1) // space
            .saturating_sub(link_mark.len());
        let title = truncate(&issue.title, budget);

        let row = PanelRow::plain(Line::segs(vec![
            seg(
                if blocked {
                    hue(thegn_core::theme::Hue::Red)
                } else {
                    status_hue(issue.status)
                },
                glyph_str,
            ),
            seg(g(), " "),
            seg(g2(), link_mark.to_string()),
            seg(d(), number_part.clone()),
            seg(g(), " "),
            seg(t(), title),
        ]))
        .with_hit(PanelHit::Row(Section::Issues, i));

        let row = if selected {
            row.with_bg(crate::seg::Tok::SelAccent)
        } else {
            row
        };
        rows.push(row);

        if rows.len() >= ctx.rows.saturating_sub(1) {
            break;
        }
    }
    rows.push(hint_row(&[
        ("↵", "link"),
        ("o", "open"),
        ("n", "new"),
        ("e", "edit"),
    ]));
    rows
}

// ---- Half view (75 cols) ----------------------------------------------------

fn half_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let cols = ctx.cols;
    let issues = sorted_issues(ctx);

    if issues.is_empty() {
        return empty_rows(ctx.model.panel.issues_configured);
    }

    let mut rows = Vec::new();
    let cursor = ctx.ui.issues_cursor;
    let data = &ctx.model.panel;

    for (i, issue) in issues.iter().enumerate() {
        let linked = data.tracker_links.contains(&issue.id);
        let selected = i == cursor;

        // Line 1: status glyph (⊘ if blocked) + number + title
        let blocked = !issue.blocked_by.is_empty();
        let title_budget = cols.saturating_sub(issue.number.len() + 3);
        let title = truncate(&issue.title, title_budget);
        let linked_str = if linked { " ◈" } else { "" };

        let line1 = Line::segs(vec![
            seg(
                if blocked {
                    hue(thegn_core::theme::Hue::Red)
                } else {
                    status_hue(issue.status)
                },
                if blocked {
                    "⊘".to_string()
                } else {
                    issue.status.glyph().to_string()
                },
            ),
            seg(g(), " "),
            seg(d(), issue.number.clone()),
            seg(g2(), linked_str.to_string()),
            seg(g(), " "),
            seg(t(), title),
        ]);

        // Line 2: priority + labels + assignee (muted)
        let mut meta: Vec<Seg> = vec![
            sp(2),
            seg(
                priority_hue(issue.priority),
                issue.priority.label().to_string(),
            ),
        ];
        if !issue.labels.is_empty() {
            meta.push(seg(g2(), "  ".to_string()));
            let labels_str = issue
                .labels
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(" · ");
            meta.push(seg(f(), labels_str));
        }
        if let Some(assignee) = issue.assignees.first() {
            let a_str = format!("  @{}", truncate(assignee, 16));
            meta.push(seg(g2(), a_str));
        }
        let line2 = Line::segs(meta);

        let bg = if selected {
            Some(crate::seg::Tok::SelAccent)
        } else {
            None
        };

        rows.push(PanelRow {
            line: line1,
            bg,
            hit: Some(PanelHit::Row(Section::Issues, i)),
        });
        rows.push(PanelRow {
            line: line2,
            bg: None,
            hit: None,
        });

        if rows.len() + 2 > ctx.rows.saturating_sub(2) {
            break;
        }
    }
    rows.push(hint_row(&[
        ("↵", "link"),
        ("o", "open"),
        ("n", "new"),
        ("e", "edit"),
        ("/", "filter"),
    ]));
    rows
}

// ---- Full view (150 cols) ---------------------------------------------------

fn full_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let data = &ctx.model.panel;
    let cols = ctx.cols;
    let mut rows = Vec::new();
    let issues = sorted_issues(ctx);

    // Header bar
    let provider_label = if !data.issues_configured {
        "no issue tracker configured".to_string()
    } else if data.tracker_issues.is_empty() {
        "no open issues".to_string()
    } else {
        // Distinct providers, in first-seen order, so an aggregated Linear+Jira
        // list reads as "⬡ ⬢ linear+jira" rather than just the first one.
        let mut providers: Vec<&str> = Vec::new();
        for i in &data.tracker_issues {
            if !providers.contains(&i.provider.as_str()) {
                providers.push(i.provider.as_str());
            }
        }
        let sigils: String = providers
            .iter()
            .map(|p| provider_sigil(p))
            .collect::<Vec<_>>()
            .join(" ");
        let names = providers.join("+");
        let open = issues.iter().filter(|i| i.status.is_active()).count();
        let total = issues.len();
        let filter_tag = ctx
            .ui
            .issues_project_filter
            .as_deref()
            .map(|p| format!(" [{p}]"))
            .unwrap_or_default();
        format!("{sigils} {names}  {open} open / {total}{filter_tag}")
    };

    rows.push(PanelRow::plain(Line::segs(vec![
        seg(d(), "ISSUES".to_string()),
        seg(g2(), format!("  {provider_label}")),
    ])));
    rows.push(rule());

    if issues.is_empty() {
        rows.extend(empty_rows(data.issues_configured));
        return rows;
    }

    let cursor = ctx.ui.issues_cursor;
    let list_w = (cols / 3).max(30);
    let detail_w = cols.saturating_sub(list_w + 2);

    // Build left column (issue list).
    let list_rows: Vec<Vec<Seg>> = issues
        .iter()
        .enumerate()
        .map(|(i, issue)| {
            let linked = data.tracker_links.contains(&issue.id);
            let sel_mark = if i == cursor { "▶ " } else { "  " };
            let link_mark = if linked { "◈" } else { " " };
            let num_w = 10.min(issue.number.len() + 1);
            let title_w = list_w.saturating_sub(2 + num_w + 1 + 1);
            vec![
                seg(if i == cursor { t() } else { g() }, sel_mark.to_string()),
                seg(status_hue(issue.status), issue.status.glyph().to_string()),
                seg(g(), " "),
                seg(d(), issue.number.clone()),
                seg(g(), " "),
                seg(
                    if i == cursor { t() } else { g2() },
                    truncate(&issue.title, title_w),
                ),
                seg(g2(), format!(" {link_mark}")),
            ]
        })
        .collect();

    // Build right column (detail for cursor issue).
    let detail_rows: Vec<Vec<Seg>> = if let Some(issue) = issues.get(cursor) {
        issue_detail_segs(issue, &data.tracker_links, detail_w)
    } else {
        vec![vec![seg(g2(), "select an issue".to_string())]]
    };

    let combined = two_col(&list_rows, &detail_rows, list_w, 2);
    let header_offset = rows.len();
    let combined_rows: Vec<PanelRow> = combined
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            PanelRow::plain(l).with_hit(PanelHit::Row(Section::Issues, header_offset + i))
        })
        .collect();
    rows.extend(combined_rows);

    rows.push(rule());
    rows.push(hint_row(&[
        ("↵", "link"),
        ("o", "open"),
        ("n", "new"),
        ("e", "edit status"),
        ("a", "self-assign"),
    ]));
    rows
}

fn issue_detail_segs(issue: &Issue, links: &[String], w: usize) -> Vec<Vec<Seg>> {
    let mut out: Vec<Vec<Seg>> = Vec::new();

    // Title (bold)
    out.push(vec![seg(t(), truncate(&issue.title, w)).bold()]);

    // Status + priority row
    out.push(vec![
        seg(status_hue(issue.status), issue.status.glyph().to_string()),
        seg(g(), format!(" {}  ", issue.status.label())),
        seg(
            priority_hue(issue.priority),
            issue.priority.label().to_string(),
        ),
    ]);

    // Assignees
    if !issue.assignees.is_empty() {
        out.push(vec![
            seg(g2(), "by ".to_string()),
            seg(d(), issue.assignees.join(", ")),
        ]);
    }

    // Labels
    if !issue.labels.is_empty() {
        out.push(vec![seg(
            f(),
            issue
                .labels
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join("  "),
        )]);
    }

    // Branch hint
    if let Some(branch) = &issue.branch_hint {
        out.push(vec![
            seg(g2(), "branch  ".to_string()),
            seg(f(), truncate(branch, w.saturating_sub(8))),
        ]);
    }

    // Linked marker
    if links.contains(&issue.id) {
        out.push(vec![seg(
            hue(Hue::Green),
            "◈ linked to this worktree".to_string(),
        )]);
    }

    // Body (first 3 lines)
    if let Some(body) = &issue.body {
        out.push(vec![seg(g3(), "─".repeat(w.min(40)))]);
        for line in body.lines().take(3) {
            out.push(vec![seg(g(), truncate(line, w))]);
        }
        let remaining = body.lines().count().saturating_sub(3);
        if remaining > 0 {
            out.push(vec![seg(g2(), format!("… +{remaining} more lines"))]);
        }
    }

    out
}

// ---- empty state ------------------------------------------------------------

fn empty_rows(configured: bool) -> Vec<PanelRow> {
    if configured {
        // A provider is set up; the queue is simply empty right now.
        return vec![PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "no open issues".to_string(),
        )]))];
    }
    // No tracker configured — say so, and point at how to enable it.
    vec![
        PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "no issue tracker configured".to_string(),
        )])),
        PanelRow::plain(Line::segs(vec![
            seg(g3(), "set ".to_string()),
            seg(f(), "[issues] provider".to_string()),
            seg(g3(), " to enable".to_string()),
        ])),
    ]
}
