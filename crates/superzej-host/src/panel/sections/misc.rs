//! The files, tests, sandbox, debug, and db sections. Files/tests/sandbox
//! deepen per view; debug/db are placeholders (identical at every width).

use superzej_core::theme::Hue;
use superzej_core::viz;

use crate::seg::{Line, seg, sp};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, bar_segs, compact_count, d, diffstat, f, g, g2,
    hint_row, hue, split_bar,
};

// ---- files ------------------------------------------------------------------

pub(super) fn files(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (model, deep, full) = (ctx.model, ctx.deep(), ctx.full());
    let data = &model.panel;
    let mut rows: Vec<PanelRow> = Vec::new();

    // Source: all tracked files when available; fall back to changed files only
    // (while the first hydration is still in flight).
    let source_paths: Vec<String> = if !data.all_files.is_empty() {
        data.all_files.clone()
    } else if !data.changes.is_empty() {
        data.changes.iter().map(|c| c.path.clone()).collect()
    } else {
        rows.push(PanelRow::plain(Line::segs(vec![seg(g(), "no files")])));
        return rows;
    };

    // Build an index from repo-relative path → change row for status glyphs.
    let by_path: std::collections::HashMap<&str, &super::ChangeRow> =
        data.changes.iter().map(|c| (c.path.as_str(), c)).collect();

    let tree = crate::panel::build_file_tree(&source_paths);
    let visible = crate::panel::file_tree_visible(&tree, &ctx.ui.files_collapsed);

    // Hit indices count every visible row (dirs AND files are actionable).
    // Dirs toggle collapse; files open in bat.
    for (fi, (_, e)) in visible.iter().enumerate() {
        let indent = 2 * e.depth as usize;
        let line = if e.is_dir {
            let collapsed = ctx.ui.files_collapsed.contains(&e.path);
            let chevron = if collapsed { "▸ " } else { "▾ " };
            // Count changed files under this dir for an inline badge.
            let changed_under = data
                .changes
                .iter()
                .filter(|c| c.path.starts_with(&format!("{}/", e.path)))
                .count();
            let dir_fg = if changed_under > 0 { d() } else { f() };
            let mut l = vec![
                sp(indent),
                seg(g2(), chevron.to_string()),
                seg(dir_fg, format!("{}/", e.name)),
            ];
            if changed_under > 0 && deep {
                l.push(seg(g(), format!("  {changed_under}✎")));
            }
            if full && !collapsed {
                let (a, del) = data
                    .changes
                    .iter()
                    .filter(|c| c.path.starts_with(&format!("{}/", e.path)))
                    .fold((0u32, 0u32), |(a, d), c| (a + c.added, d + c.deleted));
                if a > 0 || del > 0 {
                    l.push(seg(g(), "  Σ "));
                    l.push(seg(hue(Hue::Green), format!("+{a}")));
                    l.push(seg(g(), " "));
                    l.push(seg(hue(Hue::Red), format!("−{del}")));
                }
            }
            Line::segs(l)
        } else {
            let c = by_path.get(e.path.as_str()).copied();
            let (file_fg, st, st_tok) = if let Some(c) = c {
                let st = c.status.as_str();
                let tok = match st {
                    "A" => hue(Hue::Green),
                    "D" | "!U" => hue(Hue::Red),
                    "?" => g(),
                    _ => hue(Hue::Amber),
                };
                (d(), st.to_string(), tok)
            } else {
                // Unmodified tracked file
                (f(), String::new(), g())
            };
            let mut r = Vec::new();
            if deep && let Some(c) = c {
                r.extend(diffstat(c.added, c.deleted));
                r.push(sp(1));
            }
            if full && let Some(c) = c {
                r.extend(split_bar(c.added, c.deleted, 10));
                r.push(sp(1));
            }
            if !st.is_empty() {
                r.push(seg(st_tok, st));
            }
            Line::split(vec![sp(indent + 2), seg(file_fg, e.name.clone())], r)
        };
        let mut row = PanelRow::plain(line);
        row = row.with_hit(PanelHit::Row(Section::Files, fi));
        rows.push(row);
    }

    if deep {
        rows.push(PanelRow::blank());
        let loc = model.loc.map(compact_count).unwrap_or_else(|| "—".into());
        let count = data
            .file_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".into());
        rows.push(PanelRow::plain(Line::split(
            vec![seg(g(), format!("{count} files · {loc} loc"))],
            vec![seg(g2(), "o bat · O editor · y yazi")],
        )));
    }
    rows
}

// ---- tests ------------------------------------------------------------------

pub(super) fn tests(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (data, deep, full) = (&ctx.model.panel, ctx.deep(), ctx.full());
    let mut rows: Vec<PanelRow> = Vec::new();
    match &data.tests {
        Some(t) if t.passed + t.failed + t.skipped > 0 => {
            let dur = t
                .history
                .first()
                .map(|h| format!("{:.1}s", h.duration_ms as f64 / 1000.0));
            rows.push(PanelRow::plain(Line::split(
                vec![
                    seg(hue(Hue::Green), format!("✓ {}", t.passed)).bold(),
                    seg(hue(Hue::Red), format!("  ✗ {}", t.failed)).bold(),
                    seg(g(), format!("  ○ {} skip", t.skipped)),
                ],
                dur.map(|d_| vec![seg(g(), d_)]).unwrap_or_default(),
            )));
            let total = (t.passed + t.failed + t.skipped).max(1);
            let frac = t.passed as f32 / total as f32;
            let pct = (frac * 100.0).round() as u32;
            let mut bar = bar_segs(frac, ctx.cols.clamp(12, 28), hue(Hue::Green));
            if t.failed > 0 {
                bar.insert(1, seg(hue(Hue::Red), "█"));
            }
            bar.push(seg(g(), format!(" {pct}%")));
            rows.push(PanelRow::plain(Line::segs(bar)));
            if !t.failures.is_empty() {
                rows.push(PanelRow::blank());
                for (i, (name, at)) in t.failures.iter().enumerate() {
                    rows.push(
                        PanelRow::plain(Line::split(
                            vec![seg(hue(Hue::Red), "✗ "), seg(d(), name.clone())],
                            vec![seg(g(), at.clone())],
                        ))
                        .with_hit(PanelHit::Row(Section::Tests, i)),
                    );
                }
            }
            if let Some(err) = &t.error {
                rows.push(PanelRow::plain(Line::segs(vec![
                    sp(2),
                    seg(hue(Hue::Amber), format!("! {err}")),
                ])));
            }
            if deep && t.history.len() > 1 {
                rows.push(PanelRow::blank());
                let durations: Vec<f32> = {
                    // Oldest → newest so "now" reads at the right edge.
                    let mut v: Vec<f32> = t.history.iter().map(|h| h.duration_ms as f32).collect();
                    v.reverse();
                    let max = v.iter().copied().fold(1.0_f32, f32::max);
                    v.into_iter().map(|d| d / max).collect()
                };
                let mut head = vec![seg(g2(), "HISTORY").bold(), sp(2)];
                if full {
                    // Full: a 2-row braille curve of run durations.
                    rows.push(PanelRow::plain(Line::Segs(head)));
                    let w = durations.len().div_ceil(2).max(1);
                    for line in viz::braille_line(&durations, w, 2) {
                        rows.push(PanelRow::plain(Line::segs(vec![sp(1), seg(g(), line)])));
                    }
                } else {
                    // Half: an inline duration sparkline next to the header.
                    head.push(seg(g(), viz::sparkline(&durations)));
                    rows.push(PanelRow::plain(Line::Segs(head)));
                }
                let cap = if full { usize::MAX } else { 4 };
                for h in t.history.iter().take(cap) {
                    let mark = if h.failed > 0 {
                        seg(hue(Hue::Red), format!(" ✗{}", h.failed))
                    } else {
                        seg(hue(Hue::Green), " ✓ ")
                    };
                    rows.push(PanelRow::plain(Line::split(
                        vec![
                            mark,
                            seg(
                                g(),
                                format!("  {}✓ · {:.1}s", h.passed, h.duration_ms as f64 / 1000.0),
                            ),
                        ],
                        vec![seg(g(), h.branch.clone())],
                    )));
                }
            }
        }
        _ => rows.push(PanelRow::plain(Line::segs(vec![seg(
            g(),
            "no test runs yet",
        )]))),
    }
    rows.push(PanelRow::blank());
    rows.push(hint_row(&[
        ("r", "run"),
        ("R", "all"),
        ("f", "failed only"),
    ]));
    rows
}

// ---- debug / sandbox / db ----------------------------------------------------

pub(super) fn debug() -> Vec<PanelRow> {
    vec![
        PanelRow::plain(Line::split(
            vec![seg(g2(), "○ no session")],
            vec![seg(g(), "—")],
        )),
        PanelRow::blank(),
        PanelRow::plain(Line::segs(vec![seg(g2(), "BREAKPOINTS").bold()])),
        PanelRow::plain(Line::segs(vec![sp(2), seg(g2(), "none set")])),
        PanelRow::blank(),
        PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "debugger integration not wired yet",
        )])),
    ]
}

pub(super) fn sandbox(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (model, deep, full) = (ctx.model, ctx.deep(), ctx.full());
    let mut rows: Vec<PanelRow> = Vec::new();
    let active = model
        .containers
        .iter()
        .find(|c| c.ours && c.name == model.active_container_name);
    match active {
        Some(c) => {
            rows.push(PanelRow::plain(Line::split(
                vec![
                    seg(hue(Hue::Green), "● running"),
                    seg(g(), format!(" · {} · ", c.backend)),
                    seg(d(), c.name.clone()),
                ],
                vec![seg(g(), c.status.clone())],
            )));
            if !c.cpu.is_empty() || !c.mem.is_empty() {
                rows.push(PanelRow::plain(Line::segs(vec![
                    seg(g(), "cpu "),
                    seg(d(), c.cpu.clone()),
                    seg(g(), "  mem "),
                    seg(d(), c.mem.clone()),
                    seg(g(), "  net "),
                    seg(d(), c.net.clone()),
                ])));
            }
            if !c.containment.is_empty() {
                rows.push(PanelRow::plain(Line::segs(vec![
                    seg(g(), "policy "),
                    seg(d(), c.containment.clone()),
                ])));
            }
            rows.push(PanelRow::blank());
            rows.push(PanelRow::plain(Line::segs(vec![
                seg(g2(), "DENIALS").bold(),
            ])));
            rows.push(PanelRow::plain(Line::segs(vec![
                sp(2),
                seg(g2(), "none recorded"),
            ])));
            if deep && !c.mounts.is_empty() {
                rows.push(PanelRow::blank());
                rows.push(PanelRow::plain(Line::segs(vec![
                    seg(g2(), "MOUNTS").bold(),
                ])));
                rows.push(PanelRow::plain(Line::segs(vec![
                    sp(2),
                    seg(f(), c.mounts.clone()),
                ])));
            }
        }
        None => {
            // Non-OCI sandboxes (bwrap, systemd) don't create containers but
            // ARE active — show green if the DB confirms a non-host backend.
            let backend = model.active_sandbox_backend.as_str();
            let is_host_toolchain =
                matches!(backend, "bwrap" | "systemd") || backend.starts_with("bwrap");
            if is_host_toolchain {
                rows.push(PanelRow::plain(Line::segs(vec![
                    seg(hue(Hue::Green), "● active"),
                    seg(g(), format!("  {backend}")),
                ])));
            } else {
                rows.push(PanelRow::plain(Line::segs(vec![seg(
                    g2(),
                    "○ not sandboxed",
                )])));
            }
            if !model.containers.is_empty() {
                rows.push(PanelRow::plain(Line::segs(vec![seg(
                    g(),
                    format!("{} other container(s) running", model.containers.len()),
                )])));
            }
        }
    }
    // Full: every container on the machine, one table row each.
    if full && !model.containers.is_empty() {
        rows.push(PanelRow::blank());
        rows.push(PanelRow::plain(Line::segs(vec![
            seg(g2(), "ALL CONTAINERS").bold(),
            seg(g(), format!("  {}", model.containers.len())),
        ])));
        for c in &model.containers {
            let mark = if c.ours {
                seg(hue(Hue::Green), "● ")
            } else {
                seg(g2(), "○ ")
            };
            rows.push(PanelRow::plain(Line::split(
                vec![
                    mark,
                    seg(d(), c.name.clone()),
                    seg(g(), format!(" · {}", c.backend)),
                ],
                vec![seg(
                    g(),
                    format!("cpu {} · mem {} · net {}", c.cpu, c.mem, c.net),
                )],
            )));
        }
    }
    rows
}

pub(super) fn db() -> Vec<PanelRow> {
    vec![
        PanelRow::plain(Line::segs(vec![seg(g2(), "○ no database detected")])),
        PanelRow::blank(),
        PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "db introspection not wired yet",
        )])),
    ]
}
