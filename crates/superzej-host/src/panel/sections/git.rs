//! The git section: PR/checks/review/log (Normal), plus issues and a
//! velocity strip (Half), and the full-width heat calendar · velocity · log
//! layout (Full — the former git overlay).

use superzej_core::theme::Hue;
use superzej_core::viz;

use crate::panel::docs::GitDocs;
use crate::seg::{Line, Seg, Tok, seg, sp};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, ac, d, f, g, g2, g3, hint_row, hue, pr_state_hue,
    spinner_row, two_col, visible_threads,
};

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.full() {
        return full(ctx);
    }
    list(ctx)
}

/// Normal/Half: PR header, checks, review threads, log; Half adds issues and
/// a 12-week velocity strip from the fetched git docs.
fn list(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (data, deep) = (&ctx.model.panel, ctx.deep());
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

            // REVIEW — decision + unresolved threads.
            let unresolved = data.threads.iter().filter(|t| !t.resolved).count();
            if !data.threads.is_empty() || pr.review_decision.is_some() {
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
                    l.push(seg(g(), " · "));
                    l.push(seg(hue(Hue::Amber), format!("⊘{unresolved} unresolved")));
                }
                rows.push(PanelRow::plain(Line::segs(l)));
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
                        .with_hit(PanelHit::Row(Section::Git, i)),
                    );
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

    rows.push(PanelRow::blank());
    rows.push(if data.pr.is_some() {
        hint_row(&[
            ("M", "merge"),
            ("A", "approve"),
            ("r", "rerun"),
            ("o", "browser"),
        ])
    } else {
        hint_row(&[("c", "create PR")])
    });
    rows
}

fn log_row(row: &superzej_svc::git::LogRow) -> PanelRow {
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

// ---- Full: heat calendar · velocity · log (the former git overlay) ---------

fn full(ctx: &SectionCtx) -> Vec<PanelRow> {
    let footer = hint_row(&[("j/k", "scroll log"), ("y", "copy sha")]);
    let Some(docs) = &ctx.ui.docs.git else {
        return vec![
            spinner_row(ctx.ui.docs.tick, "commit data"),
            PanelRow::blank(),
            footer,
        ];
    };

    let left = calendar_column(docs);
    let body = ctx.rows.saturating_sub(2); // blank + footer
    let right = log_column(docs, ctx.ui.scroll, body);

    // Two columns when the band is wide enough for calendar + readable log.
    const LEFT_W: usize = 46;
    const GAP: usize = 2;
    let mut rows: Vec<PanelRow> = if ctx.cols >= LEFT_W + GAP + 40 {
        two_col(&left, &right, LEFT_W, GAP)
            .into_iter()
            .take(body)
            .map(PanelRow::plain)
            .collect()
    } else {
        // Narrow fallback: stack the calendar above the log.
        let mut stacked: Vec<Vec<Seg>> = left;
        stacked.push(Vec::new());
        stacked.extend(right);
        stacked
            .into_iter()
            .take(body)
            .map(|segs| {
                if segs.is_empty() {
                    PanelRow::blank()
                } else {
                    PanelRow::plain(Line::Segs(segs))
                }
            })
            .collect()
    };
    rows.push(PanelRow::blank());
    rows.push(footer);
    rows
}

/// COMMITS heat calendar + legend + VELOCITY braille, as seg rows.
fn calendar_column(docs: &GitDocs) -> Vec<Vec<Seg>> {
    let weeks = docs.heat.len();
    let mut left: Vec<Vec<Seg>> = Vec::new();
    let mut head = vec![
        seg(g2(), "COMMITS").bold(),
        seg(g3(), format!(" · {weeks} weeks  ")),
        seg(g3(), "less "),
    ];
    for l in 0..=4u8 {
        head.push(seg(Tok::Heat(l), "■"));
    }
    head.push(seg(g3(), " more"));
    left.push(head);
    left.push(Vec::new());
    let gutter = ["mon", "   ", "wed", "   ", "fri", "   ", "sun"];
    for d in 0..7 {
        let mut segs = vec![seg(g3(), format!("{} ", gutter[d]))];
        for week in &docs.heat {
            segs.push(seg(Tok::Heat(week[d]), "■"));
        }
        left.push(segs);
    }
    left.push(Vec::new());
    left.push(vec![
        seg(g2(), "VELOCITY").bold(),
        seg(g3(), " · commits/wk  "),
        seg(d(), docs.total.to_string()),
        seg(g(), " commits"),
    ]);
    let max = docs.weekly.iter().copied().max().unwrap_or(0).max(1) as f32;
    let vals: Vec<f32> = docs.weekly.iter().map(|&c| c as f32 / max).collect();
    let vw = vals.len().div_ceil(2).max(1);
    let vel = viz::braille_line(&vals, vw, 2);
    left.push(vec![sp(1), seg(ac(), vel[0].clone())]);
    left.push(vec![sp(1), seg(g(), vel[1].clone())]);
    left
}

/// The LOG column: header + scrolled graph rows, as seg rows.
fn log_column(docs: &GitDocs, scroll: usize, body: usize) -> Vec<Vec<Seg>> {
    let mut right: Vec<Vec<Seg>> = Vec::new();
    right.push(vec![
        seg(g2(), "LOG").bold(),
        sp(2),
        seg(hue(Hue::Amber), docs.head_branch.clone()),
    ]);
    right.push(Vec::new());
    let visible = body.saturating_sub(2);
    for row in docs.log.iter().skip(scroll).take(visible) {
        let mut segs = vec![seg(g(), row.graph.clone()), sp(1)];
        if !row.sha.is_empty() {
            segs.push(seg(f(), row.sha.clone()));
            segs.push(sp(1));
            segs.push(seg(d(), row.subject.clone()));
            if row.is_head() {
                segs.push(sp(1));
                segs.push(Seg::chip(ac(), " HEAD "));
            }
        }
        right.push(segs);
    }
    right
}
