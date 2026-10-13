//! The git section: PR/checks/review/log (Normal), plus issues and a
//! velocity strip (Half), and a two-column PR page (Full).

use crate::seg::{Line, Seg, seg, sp};
use thegn_core::theme::Hue;
use thegn_core::viz;

use super::{
    PanelHit, PanelRow, Section, SectionCtx, ac, d, f, g, g2, g3, hint_row, hue, pr_state_hue,
    visible_threads,
};

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.full() {
        return full_pr(ctx);
    }
    list(ctx)
}

/// Normal/Half: PR header, checks, review threads, log; Half adds issues and
/// a 12-week velocity strip from the fetched git docs.
fn list(ctx: &SectionCtx) -> Vec<PanelRow> {
    let mut rows = header_and_checks(ctx);
    rows.extend(threads_and_open_prs(ctx));
    rows.extend(log_issues_velocity(ctx));
    rows.push(PanelRow::blank());
    rows.push(if ctx.model.panel.pr.is_some() {
        hint_row(&[
            ("M", "merge"),
            ("A", "approve"),
            ("c", "comment"),
            ("r", "rerun"),
            ("o", "browser"),
        ])
    } else {
        hint_row(&[("c", "create PR")])
    });
    rows
}

fn header_and_checks(ctx: &SectionCtx) -> Vec<PanelRow> {
    let data = &ctx.model.panel;
    let mut rows: Vec<PanelRow> = Vec::new();
    match &data.pr {
        Some(pr) => {
            rows.push(PanelRow::plain(Line::split(
                vec![
                    seg(d(), "PR "),
                    seg(ac(), format!("#{}", pr.number)).bold(),
                    seg(d(), format!(" {}", pr.title)),
                ],
                vec![Seg::chip(
                    pr_state_hue(&pr.state, pr.is_draft),
                    format!(" {} ", pr.state),
                )],
            )));
            if !data.pr_base.is_empty() {
                rows.push(PanelRow::plain(Line::segs(vec![seg(
                    g(),
                    format!("{} → {}", data.branch, data.pr_base),
                )])));
            }
            rows.push(PanelRow::blank());

            // CHECKS — grouped: one row of passing names, one per failure,
            // one per pending with its running time.
            let total = data.checks.len();
            if total > 0 {
                let passing: Vec<&super::super::CheckLine> = data
                    .checks
                    .iter()
                    .filter(|c| c.state == super::super::CheckState::Pass)
                    .collect();
                let frac = passing.len() as f32 / total as f32;
                rows.push(PanelRow::plain(Line::split(
                    vec![
                        seg(g2(), "CHECKS").bold(),
                        seg(g(), format!("  {} of {} passing", passing.len(), total)),
                    ],
                    super::bar_segs(frac, 8, hue(Hue::Green)),
                )));
                if !passing.is_empty() {
                    let names: Vec<&str> =
                        passing.iter().take(4).map(|c| c.name.as_str()).collect();
                    let dur = passing.iter().filter_map(|c| c.duration_secs).max();
                    rows.push(PanelRow::plain(Line::split(
                        vec![seg(hue(Hue::Green), "✓ "), seg(f(), names.join(" · "))],
                        dur.map(|s| vec![seg(g(), super::fmt_secs(s))])
                            .unwrap_or_default(),
                    )));
                }
                for c in &data.checks {
                    match c.state {
                        super::super::CheckState::Fail => {
                            rows.push(PanelRow::plain(Line::split(
                                vec![seg(hue(Hue::Red), "✗ "), seg(d(), c.name.clone())],
                                c.duration_secs
                                    .map(|s| vec![seg(g(), super::fmt_secs(s))])
                                    .unwrap_or_default(),
                            )));
                        }
                        super::super::CheckState::Pending => {
                            rows.push(PanelRow::plain(Line::split(
                                vec![
                                    seg(hue(Hue::Amber), "⠼ "),
                                    seg(f(), c.name.clone()),
                                    seg(g(), " · running"),
                                ],
                                c.duration_secs
                                    .map(|s| vec![seg(g(), super::fmt_secs(s))])
                                    .unwrap_or_default(),
                            )));
                        }
                        super::super::CheckState::Pass => {}
                    }
                }
                rows.push(PanelRow::blank());
            }
        }
        None => {
            rows.push(PanelRow::plain(Line::segs(vec![seg(
                g(),
                data.pr_note
                    .clone()
                    .unwrap_or_else(|| "no pull request".into()),
            )])));
            rows.push(PanelRow::blank());
        }
    }
    rows
}

fn log_issues_velocity(ctx: &SectionCtx) -> Vec<PanelRow> {
    let data = &ctx.model.panel;
    let deep = ctx.deep();
    let mut rows = Vec::new();
    // LOG — graph rows.
    if !data.log.is_empty() {
        let mut r = vec![seg(d(), "")];
        if data.stash_count > 0 {
            r = vec![seg(g(), format!("stash {}", data.stash_count))];
        }
        rows.push(PanelRow::plain(Line::split(
            vec![seg(g2(), "LOG").bold()],
            r,
        )));
        for row in &data.log {
            rows.push(log_row(row));
        }
    }

    // ISSUES — deep mode only.
    if deep && !data.issues.is_empty() {
        rows.push(PanelRow::blank());
        rows.push(PanelRow::plain(Line::segs(vec![
            seg(g2(), "ISSUES").bold(),
        ])));
        for is in data.issues.iter().take(4) {
            let label = is.labels.first().cloned().unwrap_or_default();
            rows.push(PanelRow::plain(Line::split(
                vec![
                    seg(g2(), format!("#{} ", is.number)),
                    seg(f(), is.title.clone()),
                ],
                if label.is_empty() {
                    Vec::new()
                } else {
                    vec![seg(g(), label)]
                },
            )));
        }
    }

    // VELOCITY — deep mode strip from the fetched docs (12 weeks).
    if deep {
        rows.push(PanelRow::blank());
        match &ctx.ui.docs.git {
            Some(docs) if !docs.weekly.is_empty() => {
                let tail: Vec<u32> = docs.weekly.iter().rev().take(12).rev().copied().collect();
                rows.push(PanelRow::plain(Line::split(
                    vec![seg(g2(), "VELOCITY").bold(), seg(g3(), " · commits/wk")],
                    vec![seg(d(), docs.total.to_string()), seg(g(), " commits")],
                )));
                let max = tail.iter().copied().max().unwrap_or(0).max(1) as f32;
                let vals: Vec<f32> = tail.iter().map(|&c| c as f32 / max).collect();
                let vw = vals.len().div_ceil(2).max(1);
                let vel = viz::braille_line(&vals, vw, 2);
                rows.push(PanelRow::plain(Line::segs(vec![
                    sp(1),
                    seg(ac(), vel[0].clone()),
                ])));
                rows.push(PanelRow::plain(Line::segs(vec![
                    sp(1),
                    seg(g(), vel[1].clone()),
                ])));
            }
            _ => rows.push(PanelRow::plain(Line::segs(vec![seg(
                g2(),
                "velocity · loading…",
            )]))),
        }
    }

    rows
}

fn threads_and_open_prs(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (data, deep) = (&ctx.model.panel, ctx.deep());
    let mut rows: Vec<PanelRow> = Vec::new();
    if let Some(pr) = &data.pr {
        // REVIEW — decision + unresolved threads.
        let unresolved = data.threads.iter().filter(|t| !t.resolved).count();
        let top_level = data.review_snapshot.as_ref().map_or(0, |snapshot| {
            snapshot.conversation.comments.len()
                + snapshot
                    .conversation
                    .reviews
                    .iter()
                    .filter(|review| !review.body.trim().is_empty())
                    .count()
        });
        let mut per_file: Vec<(String, usize)> = Vec::new();
        for thread in data.threads.iter().filter(|thread| !thread.resolved) {
            if let Some((_, count)) = per_file.iter_mut().find(|(path, _)| *path == thread.path) {
                *count += 1;
            } else {
                per_file.push((thread.path.clone(), 1));
            }
        }
        if !data.threads.is_empty() || pr.review_decision.is_some() || top_level > 0 {
            let decision = pr
                .review_decision
                .as_deref()
                .unwrap_or("REVIEW_REQUIRED")
                .to_lowercase()
                .replace('_', " ");
            let mut l = vec![
                seg(g2(), "REVIEW").bold(),
                seg(g(), format!("  {decision}")),
            ];
            if unresolved > 0 {
                l.push(seg(
                    g(),
                    format!(" {} ", crate::caps::active_glyphs().middot),
                ));
                l.push(seg(hue(Hue::Amber), format!("⊘{unresolved} unresolved")));
            }
            if top_level > 0 {
                l.push(seg(g(), " · "));
                l.push(seg(g(), format!("{top_level} top-level")));
            }
            rows.push(PanelRow::plain(Line::segs(l)));
            if !per_file.is_empty() {
                rows.push(PanelRow::plain(Line::segs(vec![
                    seg(g2(), "files: "),
                    seg(
                        g(),
                        per_file
                            .iter()
                            .take(6)
                            .map(|(path, count)| format!("{path} x{count}"))
                            .collect::<Vec<_>>()
                            .join(&format!(" {} ", crate::caps::active_glyphs().middot)),
                    ),
                ])));
            }
            for (i, th) in visible_threads(data, deep).enumerate() {
                let mark = if th.resolved {
                    seg(hue(Hue::Green), "✓ ")
                } else {
                    seg(hue(Hue::Amber), "⊘ ")
                };
                let at = match th.line {
                    Some(l) => format!("{}:{l}", th.path),
                    None => th.path.clone(),
                };
                rows.push(
                    PanelRow::plain(Line::segs(vec![
                        mark,
                        seg(hue(Hue::Purple), th.author.clone()),
                        seg(g(), format!(" · {at} · ")),
                        seg(d(), format!("“{}”", th.snippet)),
                    ]))
                    .with_hit(PanelHit::Row(Section::Pr, i)),
                );
            }
            rows.push(PanelRow::blank());
        }
    }

    // OPEN PRS — the repo's other open PRs (from `gh pr list` via the
    // repo-keyed `pr_branch_cache`), so the section answers "what's open on
    // this repo", not only "what's open on this branch". The current branch's
    // own PR is the detail above, so it's skipped here. ◈ marks a PR whose
    // head branch already has a local worktree.
    {
        let others: Vec<&thegn_core::forge::model::PrHeader> = data
            .open_prs
            .iter()
            .filter(|p| p.head_ref != data.branch)
            .collect();
        if !others.is_empty() {
            let cap = if deep { 8 } else { 4 };
            let mut header = vec![
                seg(g2(), "OPEN PRS").bold(),
                seg(g(), format!("  {} open on this repo", others.len())),
            ];
            // Age tag once the cache is meaningfully old (offline / gh
            // failing): stale-but-shown beats blank, but it must SAY so.
            if let Some(at) = data.open_prs_fetched_at {
                let age = thegn_core::util::now().saturating_sub(at);
                if age > 600 {
                    header.push(seg(f(), format!("  ({} ago)", super::fmt_secs(age))));
                }
            }
            rows.push(PanelRow::plain(Line::segs(header)));
            for p in others.iter().take(cap) {
                let linked = ctx
                    .model
                    .sidebar_db_worktrees
                    .iter()
                    .any(|w| w.branch == p.head_ref);
                let mut l = vec![seg(ac(), format!("#{} ", p.number))];
                if linked {
                    l.push(seg(g2(), "◈ "));
                }
                l.push(seg(f(), p.head_ref.clone()));
                let state_chip = if p.is_draft {
                    Seg::chip(g(), " draft ")
                } else {
                    Seg::chip(pr_state_hue(&p.state, p.is_draft), format!(" {} ", p.state))
                };
                rows.push(PanelRow::plain(Line::split(l, vec![state_chip])));
            }
            if others.len() > cap {
                rows.push(PanelRow::plain(Line::segs(vec![seg(
                    g(),
                    format!("… +{} more", others.len() - cap),
                )])));
            }
            rows.push(PanelRow::blank());
        }
    }

    rows
}

fn log_row(row: &thegn_svc::git::LogRow) -> PanelRow {
    if row.sha.is_empty() {
        return PanelRow::plain(Line::segs(vec![seg(g(), row.graph.clone())]));
    }
    let graph_tok = if row.is_head() { ac() } else { g() };
    PanelRow::plain(Line::split(
        vec![
            seg(graph_tok, row.graph.clone()),
            sp(1),
            seg(f(), row.sha.clone()),
            sp(1),
            seg(if row.is_head() { d() } else { f() }, row.subject.clone()),
        ],
        if row.is_head() {
            vec![Seg::chip(ac(), " HEAD ")]
        } else {
            Vec::new()
        },
    ))
}

// ---- Full: the PR summary and review threads, with OPEN PRS ----------------

fn full_pr(ctx: &SectionCtx) -> Vec<PanelRow> {
    let left = header_and_checks(ctx);
    let right = threads_and_open_prs(ctx);
    let left_segs = left.iter().map(row_segs).collect::<Vec<_>>();
    let right_segs = right.iter().map(row_segs).collect::<Vec<_>>();
    let mut out = if ctx.cols >= 100 {
        let left_width = ctx.cols.saturating_sub(4) / 2;
        let mut combined = super::two_col(&left_segs, &right_segs, left_width, 2)
            .into_iter()
            .map(PanelRow::plain)
            .collect::<Vec<_>>();
        for (index, row) in right.iter().enumerate() {
            if let Some(hit) = row.hit.clone()
                && let Some(target) = combined.get_mut(index)
            {
                target.hit = Some(hit);
            }
        }
        combined
    } else {
        left.iter().chain(right.iter()).cloned().collect()
    };
    out.push(PanelRow::blank());
    out.push(if ctx.model.panel.pr.is_some() {
        hint_row(&[
            ("enter", "PR view"),
            ("M", "merge"),
            ("A", "approve"),
            ("c", "comment"),
            ("r", "rerun"),
            ("o", "browser"),
        ])
    } else {
        hint_row(&[("c", "create PR")])
    });
    out
}

fn row_segs(row: &PanelRow) -> Vec<Seg> {
    match &row.line {
        Line::Blank => Vec::new(),
        Line::Fill { ch, .. } => vec![seg(g(), ch.to_string())],
        Line::Segs(segs) => segs.clone(),
        Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => {
            let mut segs = l.clone();
            if !r.is_empty() {
                segs.push(sp(2));
                segs.extend(r.clone());
            }
            segs
        }
    }
}
