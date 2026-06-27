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
    // An open file preview takes over the whole section body (full pane).
    if let Some(fp) = &ctx.ui.file_preview {
        return file_preview_rows(fp, ctx.cols, ctx.rows);
    }
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

/// Render the inline file preview (full pane): a header (path + status + close
/// hint) then line-numbered content scrolled to `fp.scroll`, each line
/// truncated to `cols`. Pure — the host owns scroll/loading state.
fn file_preview_rows(fp: &crate::panel::FilePreview, cols: usize, rows: usize) -> Vec<PanelRow> {
    let mut out: Vec<PanelRow> = Vec::new();
    let status = if fp.loading {
        "  loading…".to_string()
    } else if fp.error.is_some() {
        String::new()
    } else {
        format!("  {} lines", fp.lines.len())
    };
    out.push(PanelRow::plain(Line::segs(vec![
        sp(1),
        seg(d(), fp.path.clone()).bold(),
        seg(g2(), status),
    ])));
    out.push(PanelRow::plain(Line::segs(vec![
        sp(1),
        seg(g2(), "esc/q close · j/k scroll · e width".to_string()),
    ])));

    if fp.loading {
        out.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(g(), "loading…".to_string()),
        ])));
        return out;
    }
    if let Some(err) = &fp.error {
        out.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(hue(Hue::Red), err.clone()),
        ])));
        return out;
    }

    // Line-numbered body window. The gutter widens to fit the largest number.
    let total = fp.lines.len();
    let gutter = total.to_string().len().max(2);
    let body = rows.saturating_sub(out.len()).max(1);
    let avail = cols.saturating_sub(gutter + 2);
    // Clamp the start so a stale scroll (e.g. after a resize shrank the
    // viewport) still shows content rather than a blank pane.
    let start = fp.scroll.min(total.saturating_sub(1));
    for (off, line) in fp.lines.iter().enumerate().skip(start).take(body) {
        let num = off + 1;
        let text: String = line.chars().take(avail).collect();
        out.push(PanelRow::plain(Line::segs(vec![
            seg(f(), format!("{num:>gutter$} ")),
            seg(d(), text),
        ])));
    }
    out
}

/// `[share]` ingress shares for the active worktree: one row per exposed port
/// with its public URL (or starting/failed state). Pure — the supervisor owns
/// lifecycle; `model.shares` is the synced snapshot.
pub(super) fn share(ctx: &SectionCtx) -> Vec<PanelRow> {
    let shares = &ctx.model.shares;
    if shares.is_empty() {
        return vec![PanelRow::plain(Line::segs(vec![seg(
            g(),
            "no shares — Alt+Shift+S to share a port".to_string(),
        )]))];
    }
    shares
        .iter()
        .map(|s| {
            let (color, status) = match &s.url {
                Some(url) => (hue(Hue::Teal), url.clone()),
                None if s.failed => (hue(Hue::Red), "failed".to_string()),
                None => (g2(), "starting…".to_string()),
            };
            PanelRow::plain(Line::segs(vec![
                seg(color, format!("\u{21c5} {} ", s.port)).bold(),
                seg(g(), status),
            ]))
        })
        .collect()
}

// ---- tests ------------------------------------------------------------------

pub(super) fn tests(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (data, deep, full) = (&ctx.model.panel, ctx.deep(), ctx.full());
    let mut rows: Vec<PanelRow> = Vec::new();

    // Full-width mode: render the live per-test node tree from TestPanelState.
    if full {
        return tests_full_tree(ctx);
    }

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

// ---- tests full tree ---------------------------------------------------------

fn tests_full_tree(ctx: &SectionCtx) -> Vec<PanelRow> {
    use crate::seg::Tok;
    use crate::testkit::model::{TestNodeKind, TestState};

    let ts = &ctx.ui.tests;
    let mut rows: Vec<PanelRow> = Vec::new();

    // Header: summary counts + running indicator.
    let s = &ts.summary;
    let mut header: Vec<crate::seg::Seg> = Vec::new();
    if s.running {
        header.push(seg(hue(Hue::Amber), "… running  "));
    }
    if s.passed > 0 {
        header.push(seg(hue(Hue::Green), format!("✓ {}  ", s.passed)));
    }
    if s.failed > 0 {
        header.push(seg(hue(Hue::Red), format!("✗ {}  ", s.failed)));
    }
    if s.skipped > 0 {
        header.push(seg(g2(), format!("○ {} skip", s.skipped)));
    }
    if header.is_empty() {
        header.push(seg(g2(), "no tests discovered"));
    }
    if s.stale {
        header.push(seg(hue(Hue::Amber), "  stale"));
    }
    rows.push(PanelRow::plain(Line::segs(header)));

    if ts.nodes.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "run tests to populate the tree",
        )])));
        rows.push(super::hint_row(&[
            ("r", "run"),
            ("R", "all"),
            ("f", "failed only"),
        ]));
        return rows;
    }

    let visible = ts.visible_indices();

    // Two-column layout: left=tree list, right=detail for selected node.
    let list_w = (ctx.cols / 2).clamp(24, 50);
    let detail_w = ctx.cols.saturating_sub(list_w + 2);
    let cursor_visible_pos = visible.iter().position(|&i| i == ts.cursor);

    // Build left column rows.
    let list_rows: Vec<Vec<crate::seg::Seg>> = visible
        .iter()
        .map(|&node_idx| {
            let node = &ts.nodes[node_idx];
            let selected = node_idx == ts.cursor;
            match node.kind {
                TestNodeKind::Group => {
                    let arrow = if selected { "▶ " } else { "  " };
                    vec![
                        seg(if selected { super::t() } else { g2() }, arrow),
                        seg(
                            if selected { super::t() } else { super::d() },
                            node.label.clone(),
                        )
                        .bold(),
                    ]
                }
                TestNodeKind::Test | TestNodeKind::Failure => {
                    let (glyph, glyph_col) = match node.state {
                        TestState::Pass => ("✓", hue(Hue::Green)),
                        TestState::Fail => ("✗", hue(Hue::Red)),
                        TestState::Running => ("…", hue(Hue::Amber)),
                        TestState::Skip => ("○", g2()),
                        TestState::Unknown => ("·", g2()),
                    };
                    let indent = if node.depth > 0 { 2 } else { 0 };
                    let name_w = list_w.saturating_sub(indent + glyph.len() + 1);
                    let label: String = node.label.chars().take(name_w).collect();
                    let mut segs = Vec::new();
                    if indent > 0 {
                        segs.push(sp(indent));
                    }
                    segs.push(seg(glyph_col, glyph));
                    segs.push(seg(g2(), " "));
                    segs.push(seg(if selected { super::t() } else { super::d() }, label));
                    segs
                }
            }
        })
        .collect();

    // Build right column: detail for the cursor's node.
    let detail_rows: Vec<Vec<crate::seg::Seg>> = if let Some(node) = ts.selected_node() {
        let mut d_rows: Vec<Vec<crate::seg::Seg>> = Vec::new();
        match node.kind {
            TestNodeKind::Group => {
                let group_tests: Vec<&crate::testkit::model::TestNode> = ts
                    .nodes
                    .iter()
                    .filter(|n| {
                        n.kind == TestNodeKind::Test
                            && crate::testkit::model::test_name_of(&n.id) != n.label
                            || {
                                // simple: check group prefix
                                n.kind != TestNodeKind::Group && n.id.starts_with(&node.label)
                            }
                    })
                    .collect();
                let p = group_tests
                    .iter()
                    .filter(|n| n.state == TestState::Pass)
                    .count();
                let f = group_tests
                    .iter()
                    .filter(|n| n.state == TestState::Fail)
                    .count();
                d_rows.push(vec![
                    seg(hue(Hue::Green), format!("✓ {p}  ")),
                    seg(hue(Hue::Red), format!("✗ {f}")),
                ]);
            }
            TestNodeKind::Test | TestNodeKind::Failure => {
                let (glyph, glyph_col) = match node.state {
                    TestState::Pass => ("✓ PASS", hue(Hue::Green)),
                    TestState::Fail => ("✗ FAIL", hue(Hue::Red)),
                    TestState::Running => ("… running", hue(Hue::Amber)),
                    TestState::Skip => ("○ skip", g2()),
                    TestState::Unknown => ("· unknown", g2()),
                };
                d_rows.push(vec![seg(glyph_col, glyph)]);
                d_rows.push(vec![seg(super::t(), node.id.clone())]);
                if let Some(loc) = &node.location {
                    let loc_str = format!("{}:{}", loc.path, loc.line);
                    let loc_str: String = loc_str.chars().take(detail_w).collect();
                    d_rows.push(vec![seg(g2(), "at  "), seg(super::d(), loc_str)]);
                }
                if let Some(msg) = &node.message {
                    d_rows.push(vec![]);
                    for chunk in msg.chars().collect::<Vec<_>>().chunks(detail_w.max(1)) {
                        let s: String = chunk.iter().collect();
                        d_rows.push(vec![seg(hue(Hue::Red), s)]);
                    }
                }
            }
        }
        d_rows
    } else {
        vec![vec![seg(g2(), "select a test")]]
    };

    // Merge list + detail into two-column rows.
    let combined = super::two_col(&list_rows, &detail_rows, list_w, 2);
    rows.extend(combined.into_iter().enumerate().map(|(i, l)| {
        let node_idx = visible.get(i).copied().unwrap_or(usize::MAX);
        let selected = node_idx == ts.cursor;
        let row = PanelRow::plain(l).with_hit(PanelHit::Row(Section::Tests, node_idx));
        if selected {
            row.with_bg(Tok::SelAccent)
        } else {
            row
        }
    }));

    let _ = cursor_visible_pos; // used for scroll tracking in future
    rows.push(super::hint_row(&[
        ("r", "run"),
        ("F", "file"),
        ("p", "pkg"),
        ("R", "all"),
        ("↵", "open"),
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
            let (bullet, bullet_label) = match &model.container_health {
                crate::chrome::ContainerHealth::Degraded(reason) => {
                    (seg(hue(Hue::Amber), "⚠ degraded"), Some(reason.clone()))
                }
                _ => (seg(hue(Hue::Green), "● running"), None),
            };
            rows.push(PanelRow::plain(Line::split(
                vec![
                    bullet,
                    seg(g(), format!(" · {} · ", c.backend)),
                    seg(d(), c.name.clone()),
                ],
                vec![seg(g(), c.status.clone())],
            )));
            if let Some(reason) = bullet_label {
                rows.push(PanelRow::plain(Line::segs(vec![
                    sp(2),
                    seg(hue(Hue::Amber), reason),
                ])));
            }
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
            if deep && !model.container_events.is_empty() {
                rows.push(PanelRow::blank());
                rows.push(PanelRow::plain(Line::segs(vec![
                    seg(g2(), "AUDIT LOG").bold(),
                ])));
                for ev in &model.container_events {
                    let kind_col = match ev.kind.as_str() {
                        "network" => hue(Hue::Amber),
                        "die" => hue(Hue::Red),
                        _ => g(),
                    };
                    let detail = ev.detail.as_deref().unwrap_or("—");
                    rows.push(PanelRow::plain(Line::segs(vec![
                        sp(2),
                        seg(kind_col, format!("{:<8}", ev.kind)),
                        seg(d(), detail.to_string()),
                    ])));
                }
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
    // Startup orphan GC notice (shown until the next model hydration clears it).
    if !model.startup_orphans_removed.is_empty() {
        rows.push(PanelRow::blank());
        rows.push(PanelRow::plain(Line::segs(vec![
            seg(hue(Hue::Amber), "⚠"),
            seg(
                g(),
                format!(
                    " removed {} orphan container(s) at startup",
                    model.startup_orphans_removed.len()
                ),
            ),
        ])));
        for name in &model.startup_orphans_removed {
            rows.push(PanelRow::plain(Line::segs(vec![
                sp(2),
                seg(d(), name.clone()),
            ])));
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
