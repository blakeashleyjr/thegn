//! Issues section — browse tracked issues from Linear / GitHub / Jira.
//!
//! Three view widths:
//!   Normal (39 cols): compact one-liners, status glyph + number + title.
//!   Half   (75 cols): two-line rows with priority/labels/assignee.
//!   Full   (150 cols): left list + right detail panel side-by-side.

use thegn_core::issue::{Issue, IssuePriority, IssueStatus};
use thegn_core::pipeline_report::{display_cell, display_clip};
use thegn_core::theme::Hue;
use unicode_width::UnicodeWidthStr;

use crate::panel::gitui::DocLoad;
use crate::seg::{Line, Seg, seg, sp};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, d, f, g, g2, g3, hint_row, hue, list_detail, rule, t,
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

// ---- sorted + filtered issue list ------------------------------------------

/// Returns issues sorted by priority (Urgent first, Done/Cancelled last) and
/// filtered by the active text query and project filter. Shared by the render
/// path and the Issues action-key handlers (`handlers::tracker`) so a cursor
/// index means the same row in both — indexing the raw, unsorted
/// `tracker_issues` from the handlers used to act on a *different* issue than
/// the one highlighted.
pub(crate) fn sorted_filtered_issues<'a>(
    data: &'a crate::panel::PanelData,
    query: &str,
    project: Option<&str>,
) -> Vec<&'a Issue> {
    let query = query.to_lowercase();
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

pub(crate) fn issue_at_cursor<'a>(
    panel: &'a crate::panel::PanelData,
    ui: &crate::panel::PanelUi,
) -> Option<&'a Issue> {
    sorted_filtered_issues(
        panel,
        &ui.issues_filter,
        ui.issues_project_filter.as_deref(),
    )
    .into_iter()
    .nth(ui.cursor)
}

pub(super) fn sorted_issues<'a>(ctx: &'a SectionCtx) -> Vec<&'a Issue> {
    sorted_filtered_issues(
        &ctx.model.panel,
        &ctx.ui.issues_filter,
        ctx.ui.issues_project_filter.as_deref(),
    )
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
        return empty_rows(
            ctx.model.panel.issues_configured,
            &ctx.model.panel.tracker_load,
            !ctx.model.panel.tracker_issues.is_empty(),
        );
    }

    let mut rows = Vec::new();
    if !ctx.model.panel.tracker_partial.is_empty() {
        let accounts = ctx
            .model
            .panel
            .tracker_partial
            .iter()
            .map(|partial| display_cell(&partial.account))
            .collect::<Vec<_>>()
            .join(", ");
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            hue(Hue::Amber),
            display_clip(
                &format!("partial results: {accounts} — showing the last complete list"),
                cols,
            ),
        )])));
    }
    let cursor = ctx.ui.cursor;
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
        let number_part = display_cell(&issue.number);
        let number_width = UnicodeWidthStr::width(number_part.as_str());
        let budget = cols
            .saturating_sub(1) // glyph
            .saturating_sub(1) // space
            .saturating_sub(number_width)
            .saturating_sub(1) // space
            .saturating_sub(link_mark.len());
        let title = display_clip(&issue.title, budget);

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
    }
    rows.push(hint_row(&[
        ("↵", "link"),
        ("o", "browser"),
        ("a", "assign me"),
        ("b", "branch"),
    ]));
    rows
}

// ---- Half view (75 cols) ----------------------------------------------------

fn half_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let cols = ctx.cols;
    let issues = sorted_issues(ctx);

    if issues.is_empty() {
        return empty_rows(
            ctx.model.panel.issues_configured,
            &ctx.model.panel.tracker_load,
            !ctx.model.panel.tracker_issues.is_empty(),
        );
    }

    let mut rows = Vec::new();
    if !ctx.model.panel.tracker_partial.is_empty() {
        let accounts = ctx
            .model
            .panel
            .tracker_partial
            .iter()
            .map(|partial| display_cell(&partial.account))
            .collect::<Vec<_>>()
            .join(", ");
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            hue(Hue::Amber),
            display_clip(
                &format!("partial results: {accounts} — showing the last complete list"),
                cols,
            ),
        )])));
    }
    let cursor = ctx.ui.cursor;
    let data = &ctx.model.panel;

    for (i, issue) in issues.iter().enumerate() {
        let linked = data.tracker_links.contains(&issue.id);
        let selected = i == cursor;

        // Line 1: status glyph (⊘ if blocked) + number + title
        let blocked = !issue.blocked_by.is_empty();
        let number = display_clip(&issue.number, cols.saturating_sub(3));
        let title_budget = cols.saturating_sub(UnicodeWidthStr::width(number.as_str()) + 3);
        let title = display_clip(&issue.title, title_budget);
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
            seg(d(), number),
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
                .map(|label| display_cell(label))
                .collect::<Vec<_>>()
                .join(" · ");
            meta.push(seg(f(), labels_str));
        }
        if let Some(assignee) = issue.assignees.first() {
            let a_str = format!("  @{}", display_clip(assignee, 16));
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
    }
    rows.push(hint_row(&[
        ("↵", "link"),
        ("o", "browser"),
        ("a", "assign me"),
        ("b", "branch"),
        ("D", "dispatch"),
        ("r", "refresh"),
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
        match &data.tracker_load {
            DocLoad::Pending => "loading issues…".to_string(),
            DocLoad::Ready => "no open issues".to_string(),
            DocLoad::Failed(error) => format!("issue tracker unavailable: {error}"),
        }
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
    let provider_label = if data.tracker_partial.is_empty() {
        provider_label
    } else {
        let accounts = data
            .tracker_partial
            .iter()
            .map(|partial| display_cell(&partial.account))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{provider_label}  partial: {accounts}")
    };

    rows.push(PanelRow::plain(Line::segs(vec![
        seg(d(), "ISSUES".to_string()),
        seg(g2(), format!("  {}", display_cell(&provider_label))),
    ])));
    rows.push(rule());

    if issues.is_empty() {
        rows.extend(empty_rows(
            data.issues_configured,
            &data.tracker_load,
            !data.tracker_issues.is_empty(),
        ));
        return rows;
    }

    let cursor = ctx.ui.cursor;
    let list_w = (cols / 3).max(30);
    let detail_w = cols.saturating_sub(list_w + 2);

    // Build left column (issue list).
    let list_rows: Vec<(Vec<Seg>, Option<usize>)> = issues
        .iter()
        .enumerate()
        .map(|(i, issue)| {
            let linked = data.tracker_links.contains(&issue.id);
            let sel_mark = if i == cursor { "▶ " } else { "  " };
            let link_mark = if linked { "◈" } else { " " };
            let number = display_clip(&issue.number, 10);
            let num_w = UnicodeWidthStr::width(number.as_str()) + 1;
            let title_w = list_w.saturating_sub(2 + num_w + 1 + 1);
            (
                vec![
                    seg(if i == cursor { t() } else { g() }, sel_mark.to_string()),
                    seg(status_hue(issue.status), issue.status.glyph().to_string()),
                    seg(g(), " "),
                    seg(d(), number),
                    seg(g(), " "),
                    seg(
                        if i == cursor { t() } else { g2() },
                        display_clip(&issue.title, title_w),
                    ),
                    seg(g2(), format!(" {link_mark}")),
                ],
                Some(i),
            )
        })
        .collect();

    // Build right column (detail for cursor issue).
    let detail_rows: Vec<Vec<Seg>> = if let Some(issue) = issues.get(cursor) {
        issue_detail_segs(issue, &data.tracker_links, detail_w)
    } else {
        vec![vec![seg(g2(), "select an issue".to_string())]]
    };

    rows.extend(list_detail(
        Section::Issues,
        &list_rows,
        &detail_rows,
        cursor,
        list_w,
        ctx.rows.saturating_sub(rows.len() + 2),
    ));

    rows.push(rule());
    rows.push(hint_row(&[
        ("↵", "link"),
        ("o", "browser"),
        ("a", "assign me"),
        ("b", "branch"),
        ("D", "dispatch"),
        ("r", "refresh"),
    ]));
    rows
}

fn issue_detail_segs(issue: &Issue, links: &[String], w: usize) -> Vec<Vec<Seg>> {
    let mut out: Vec<Vec<Seg>> = Vec::new();

    // Title (bold)
    out.push(vec![seg(t(), display_clip(&issue.title, w)).bold()]);

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
            seg(d(), display_cell(&issue.assignees.join(", "))),
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
                .map(|label| display_cell(label))
                .collect::<Vec<_>>()
                .join("  "),
        )]);
    }

    // Branch hint
    if let Some(branch) = &issue.branch_hint {
        out.push(vec![
            seg(g2(), "branch  ".to_string()),
            seg(f(), display_clip(branch, w.saturating_sub(8))),
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
            out.push(vec![seg(g(), display_clip(line, w))]);
        }
        let remaining = body.lines().count().saturating_sub(3);
        if remaining > 0 {
            out.push(vec![seg(g2(), format!("… +{remaining} more lines"))]);
        }
    }

    out
}

// ---- empty state ------------------------------------------------------------

fn empty_rows(configured: bool, load: &DocLoad, has_rows: bool) -> Vec<PanelRow> {
    if configured {
        let message = if has_rows {
            "no open issues".to_string()
        } else {
            match load {
                DocLoad::Pending => "loading issues…".to_string(),
                DocLoad::Ready => "no open issues".to_string(),
                DocLoad::Failed(error) => format!("issue tracker unavailable: {error}"),
            }
        };
        return vec![PanelRow::plain(Line::segs(vec![seg(g2(), message)]))];
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

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::issue::{Issue, IssuePriority, IssueStatus};

    fn assert_safe_rows(rows: &[PanelRow], cols: usize) {
        fn check(segs: &[Seg], cols: usize) {
            for segment in segs {
                assert!(
                    segment.text.chars().all(|c| {
                        !c.is_control() && !thegn_core::pipeline_report::is_format_control(c)
                    }),
                    "{:?}",
                    segment.text
                );
                if segment.text.contains('…') {
                    assert!(
                        UnicodeWidthStr::width(segment.text.as_str()) <= cols,
                        "clipped segment exceeds {cols} columns: {:?}",
                        segment.text
                    );
                }
            }
        }

        for row in rows {
            match &row.line {
                Line::Segs(segs) => check(segs, cols),
                Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => {
                    check(l, cols);
                    check(r, cols);
                }
                Line::Blank | Line::Fill { .. } => {}
            }
        }
    }

    fn issue(id: &str, prio: IssuePriority, status: IssueStatus, project: &str) -> Issue {
        Issue {
            id: id.to_string(),
            number: id.to_string(),
            title: format!("title {id}"),
            priority: prio,
            status,
            project_ids: if project.is_empty() {
                vec![]
            } else {
                vec![project.to_string()]
            },
            ..Default::default()
        }
    }

    fn panel_with(issues: Vec<Issue>) -> crate::panel::PanelData {
        crate::panel::PanelData {
            tracker_issues: issues,
            ..Default::default()
        }
    }

    #[test]
    fn sorted_filtered_issues_orders_by_priority_and_floats_terminal() {
        // Insertion order deliberately scrambled vs. the intended display order.
        let data = panel_with(vec![
            issue("low", IssuePriority::Low, IssueStatus::Todo, ""),
            issue("done", IssuePriority::Urgent, IssueStatus::Done, ""),
            issue("urgent", IssuePriority::Urgent, IssueStatus::Todo, ""),
            issue("high", IssuePriority::High, IssueStatus::InProgress, ""),
        ]);
        let ordered: Vec<&str> = sorted_filtered_issues(&data, "", None)
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        // Active by priority, then terminal (Done) last — regardless of its
        // priority. Cursor N must map to THIS order, not insertion order.
        assert_eq!(ordered, vec!["urgent", "high", "low", "done"]);
    }

    #[test]
    fn sorted_filtered_issues_applies_query_and_project_filters() {
        let data = panel_with(vec![
            issue("APP-1", IssuePriority::High, IssueStatus::Todo, "proj-a"),
            issue("APP-2", IssuePriority::High, IssueStatus::Todo, "proj-b"),
        ]);
        // Project filter narrows to a single issue.
        let by_project = sorted_filtered_issues(&data, "", Some("proj-b"));
        assert_eq!(by_project.len(), 1);
        assert_eq!(by_project[0].id, "APP-2");
        // Text query matches on number/title.
        let by_query = sorted_filtered_issues(&data, "app-1", None);
        assert_eq!(by_query.len(), 1);
        assert_eq!(by_query[0].id, "APP-1");
    }

    #[test]
    fn issue_at_cursor_uses_sorted_filtered_visible_order() {
        let data = panel_with(vec![
            issue("low", IssuePriority::Low, IssueStatus::Todo, ""),
            issue("urgent", IssuePriority::Urgent, IssueStatus::Todo, ""),
            issue("high", IssuePriority::High, IssueStatus::Todo, ""),
            issue("medium", IssuePriority::Medium, IssueStatus::Todo, ""),
        ]);
        let ui = crate::panel::PanelUi {
            open: Section::Issues,
            cursor: 2,
            ..Default::default()
        };
        assert_eq!(
            issue_at_cursor(&data, &ui).map(|issue| issue.id.as_str()),
            Some("medium")
        );

        let filtered_ui = crate::panel::PanelUi {
            open: Section::Issues,
            cursor: 0,
            issues_filter: "high".into(),
            ..Default::default()
        };
        assert_eq!(
            issue_at_cursor(&data, &filtered_ui).map(|issue| issue.id.as_str()),
            Some("high")
        );
    }

    #[test]
    fn issues_render_selection_and_full_hits_use_list_ordinals() {
        let data = panel_with(vec![
            issue("low", IssuePriority::Low, IssueStatus::Todo, ""),
            issue("urgent", IssuePriority::Urgent, IssueStatus::Todo, ""),
            issue("high", IssuePriority::High, IssueStatus::Todo, ""),
            issue("medium", IssuePriority::Medium, IssueStatus::Todo, ""),
        ]);
        let full_ui = crate::panel::PanelUi {
            open: Section::Issues,
            width: crate::layout::PanelWidth::Full,
            cursor: 1,
            ..Default::default()
        };
        let full_ctx = SectionCtx {
            model: &crate::chrome::FrameModel {
                panel: data.clone(),
                ..Default::default()
            },
            ui: &full_ui,
            cols: 150,
            rows: 40,
        };
        let full_rows = content(&full_ctx);
        let hits: Vec<_> = full_rows.iter().filter_map(|row| row.hit).collect();
        assert_eq!(
            hits,
            (0..4)
                .map(|i| PanelHit::Row(Section::Issues, i))
                .collect::<Vec<_>>()
        );

        let normal_ui = crate::panel::PanelUi {
            open: Section::Issues,
            cursor: 1,
            ..Default::default()
        };
        let normal_ctx = SectionCtx {
            model: &crate::chrome::FrameModel {
                panel: data,
                ..Default::default()
            },
            ui: &normal_ui,
            cols: 60,
            rows: 40,
        };
        let selected: Vec<_> = content(&normal_ctx)
            .into_iter()
            .filter(|row| row.bg == Some(crate::seg::Tok::SelAccent))
            .collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].hit, Some(PanelHit::Row(Section::Issues, 1)));
    }

    #[test]
    fn partial_notice_names_the_account() {
        let issue = issue("ABC-1", IssuePriority::High, IssueStatus::Todo, "");
        let partial = thegn_core::issue::IssueCachePartial {
            account: "work".into(),
            reason: "page cap".into(),
            checked_at_ms: 7,
        };
        let render = |tracker_partial| {
            let model = crate::chrome::FrameModel {
                panel: crate::panel::PanelData {
                    tracker_issues: vec![issue.clone()],
                    tracker_partial,
                    ..Default::default()
                },
                ..Default::default()
            };
            let ui = crate::panel::PanelUi {
                open: Section::Issues,
                ..Default::default()
            };
            let ctx = SectionCtx {
                model: &model,
                ui: &ui,
                cols: 60,
                rows: 20,
            };
            content(&ctx)
        };
        let partial_rows = render(vec![partial]);
        let text = partial_rows
            .iter()
            .flat_map(|row| match &row.line {
                Line::Segs(segs) => segs.iter().map(|seg| seg.text.as_str()).collect::<Vec<_>>(),
                Line::Blank
                | Line::Fill { .. }
                | Line::Split { .. }
                | Line::SplitMinLeft { .. } => Vec::new(),
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("partial results"));
        assert!(text.contains("work"));
        assert_eq!(
            partial_rows
                .iter()
                .filter_map(|row| row.hit)
                .collect::<Vec<_>>(),
            vec![PanelHit::Row(Section::Issues, 0)]
        );

        let complete_rows = render(Vec::new());
        assert!(!complete_rows.iter().any(|row| match &row.line {
            Line::Segs(segs) => segs.iter().any(|seg| seg.text.contains("partial results")),
            _ => false,
        }));
    }

    #[test]
    fn issues_empty_states_distinguish_pending_ready_failed() {
        use crate::panel::gitui::DocLoad;

        for (width, cols) in [
            (crate::layout::PanelWidth::Normal, 39),
            (crate::layout::PanelWidth::Half, 75),
            (crate::layout::PanelWidth::Full, 150),
        ] {
            let render = |load| {
                let model = crate::chrome::FrameModel {
                    panel: crate::panel::PanelData {
                        issues_configured: true,
                        tracker_load: load,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let ui = crate::panel::PanelUi {
                    open: Section::Issues,
                    width,
                    ..Default::default()
                };
                let ctx = SectionCtx {
                    model: &model,
                    ui: &ui,
                    cols,
                    rows: 20,
                };
                content(&ctx)
                    .iter()
                    .flat_map(|row| match &row.line {
                        Line::Segs(segs) => {
                            segs.iter().map(|s| s.text.as_str()).collect::<Vec<_>>()
                        }
                        Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => l
                            .iter()
                            .chain(r)
                            .map(|s| s.text.as_str())
                            .collect::<Vec<_>>(),
                        Line::Blank | Line::Fill { .. } => Vec::new(),
                    })
                    .collect::<String>()
            };
            let pending = render(DocLoad::Pending);
            let ready = render(DocLoad::Ready);
            let failed = render(DocLoad::Failed("offline".into()));
            assert_ne!(pending, ready, "{width:?}");
            assert_ne!(pending, failed, "{width:?}");
            assert_ne!(ready, failed, "{width:?}");
            assert!(pending.contains("loading issues"), "{pending}");
            assert!(ready.contains("no open issues"), "{ready}");
            assert!(failed.contains("offline"), "{failed}");
        }
    }

    #[test]
    fn issues_summary_distinguishes_pending_failed_and_clear() {
        use crate::panel::gitui::DocLoad;

        let render = |tracker_load| {
            let model = crate::chrome::FrameModel {
                panel: crate::panel::PanelData {
                    issues_configured: true,
                    tracker_load,
                    ..Default::default()
                },
                ..Default::default()
            };
            crate::panel::sections::summary(Section::Issues, &model)
                .iter()
                .map(|seg| seg.text.as_str())
                .collect::<String>()
        };
        assert_eq!(render(DocLoad::Pending), "…");
        assert_eq!(render(DocLoad::Failed("offline".into())), "failed");
        assert_eq!(render(DocLoad::Ready), "clear");
    }

    #[test]
    fn hostile_issue_fields_render_without_terminal_controls() {
        let mut hostile = issue(
            "APP-\u{1b}[31m1",
            IssuePriority::High,
            IssueStatus::Todo,
            "",
        );
        hostile.title = "\u{1b}]0;pwn\u{7}日本語".repeat(12);
        hostile.labels = vec!["label\u{202e}".into()];
        hostile.assignees = vec!["user\u{200b}".into()];
        let model = crate::chrome::FrameModel {
            panel: panel_with(vec![hostile]),
            ..Default::default()
        };
        for (width, cols) in [
            (crate::layout::PanelWidth::Normal, 39),
            (crate::layout::PanelWidth::Half, 75),
            (crate::layout::PanelWidth::Full, 150),
        ] {
            let ui = crate::panel::PanelUi {
                open: Section::Issues,
                width,
                cursor: 0,
                ..Default::default()
            };
            let rows = content(&SectionCtx {
                model: &model,
                ui: &ui,
                cols,
                rows: 20,
            });
            assert_safe_rows(&rows, cols);
            let text = format!("{rows:?}");
            assert!(
                text.contains("�"),
                "hostile issue fields were not rendered: {text}"
            );
            if matches!(
                width,
                crate::layout::PanelWidth::Half | crate::layout::PanelWidth::Full
            ) {
                assert!(
                    text.contains("label�"),
                    "issue label was not rendered: {text}"
                );
                assert!(
                    text.contains("user�"),
                    "issue assignee was not rendered: {text}"
                );
            }
        }
    }
}
