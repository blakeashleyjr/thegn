//! Disk tab renderer and tab-specific monitor actions.

use super::super::build::*;
use super::super::{Confirm, MonitorOutcome, MonitorOverlay};
use crate::chrome::S;
use crate::sections::{Cell, Section, TableSection, spacer};
use crate::seg::Tok;
use crate::telemetry::Metric;
use thegn_core::disk_fill::DiskFillEta;
use thegn_core::theme::Hue;
use thegn_core::viz::Unit;

pub(in crate::monitor) mod view;

#[cfg(test)]
#[path = "disk/disk_wire_tests.rs"]
mod wire_tests;

pub(in crate::monitor) fn disk(
    cx: &Ctx,
    wt_rows: &[DiskWtRow],
    sel: usize,
    eta: Option<DiskFillEta>,
) -> TabBuild {
    let s = &cx.model.stats;
    let mut out = vec![cx.graph(Metric::DiskIo, MAIN_H, Tok::Hue(Hue::Blue))];

    out.push(spacer());
    out.push(heading("volumes", Some(format!("{}", s.disks.len()))));
    let rows: Vec<Vec<Cell>> = s
        .disks
        .iter()
        .map(|d| {
            let tone = free_tone(d.free_pct);
            let mount = if d.extra_mounts > 0 {
                let suffix = format!(" +{}", d.extra_mounts);
                format!("{}{suffix}", trunc(&d.mount, 20 - suffix.chars().count()))
            } else {
                trunc(&d.mount, 20)
            };
            vec![
                Cell::Text(mount, Tok::Slot(S::Text)),
                Cell::Text(kind_str(d.kind).into(), Tok::Slot(S::Ghost)),
                Cell::Bar(f32::from(d.free_pct) / 100.0, 8, tone),
                Cell::Text(format!("{}% free", d.free_pct), tone),
                Cell::Text(
                    format!("↓{}", Unit::BytesPerSec.fmt(d.read_bps as f32)),
                    Tok::Slot(S::Dim),
                ),
                Cell::Text(
                    format!("↑{}", Unit::BytesPerSec.fmt(d.write_bps as f32)),
                    Tok::Slot(S::Dim),
                ),
            ]
        })
        .collect();
    out.push(Section::Table(TableSection {
        header: vec![
            "mount".into(),
            "kind".into(),
            "free".into(),
            "".into(),
            "read".into(),
            "write".into(),
        ],
        rows,
        sel: None,
    }));

    if let Some((total, avail)) = s.disk_bytes {
        out.push(spacer());
        // The fill projection rides the worktrees-filesystem heading, so it sits
        // right beside the free-space number it extrapolates from.
        out.push(heading("worktrees filesystem", eta.map(fmt_fill_eta)));
        out.push(Section::Grid {
            cols: 2,
            cells: vec![
                kv("total", Unit::Bytes.fmt(total as f32), Tok::Slot(S::Dim)),
                kv(
                    "available",
                    Unit::Bytes.fmt(avail as f32),
                    Tok::Slot(S::Text),
                ),
                kv(
                    "used",
                    Unit::Bytes.fmt(total.saturating_sub(avail) as f32),
                    Tok::Slot(S::Dim),
                ),
            ],
        });
    }

    // The worktree lane: per-worktree usage from the `[disk]` scanner cache, so
    // "where did the disk go?" has an IDE-shaped answer without a du walk.
    out.push(spacer());
    out.push(worktrees_heading(wt_rows));
    let mut row_y = Vec::new();
    if !wt_rows.is_empty() {
        let rows: Vec<Vec<Cell>> = wt_rows
            .iter()
            .map(|w| {
                vec![
                    Cell::Text(trunc(&w.name, 22), Tok::Slot(S::Text)),
                    Cell::Text(Unit::Bytes.fmt(w.total_bytes as f32), Tok::Slot(S::Text)),
                    Cell::Text(
                        format!("target {}", Unit::Bytes.fmt(w.target_bytes as f32)),
                        Tok::Hue(Hue::Amber),
                    ),
                    Cell::Text(fmt_age(w.age_secs), Tok::Slot(S::Ghost)),
                ]
            })
            .collect();
        row_y = row_ys(&out, rows.len(), true);
        out.push(Section::Table(TableSection {
            header: vec![
                "worktree".into(),
                "size".into(),
                "reclaimable".into(),
                "measured".into(),
            ],
            rows,
            sel: Some(sel),
        }));
    }
    TabBuild {
        sections: out,
        row_y,
    }
}

/// The worktrees-lane heading: count plus the grand `target/` reclaimable total,
/// which is the number that answers "how much can I get back".
fn worktrees_heading(rows: &[DiskWtRow]) -> Section {
    if rows.is_empty() {
        return heading(
            "worktrees",
            Some("no cached sizes yet (or [disk] show_sizes = false)".into()),
        );
    }
    let reclaimable: u64 = rows.iter().map(|w| w.target_bytes).sum();
    heading(
        "worktrees",
        Some(format!(
            "{} · {} reclaimable",
            rows.len(),
            Unit::Bytes.fmt(reclaimable as f32)
        )),
    )
}

/// "full in ~2d" / "full in ~5h" for the fill projection. The `~` is honest: it
/// is an extrapolation of a short trend, not a guarantee.
fn fmt_fill_eta(eta: DiskFillEta) -> String {
    let hours = eta.hours;
    let when = if hours >= 48.0 {
        format!("~{}d", (hours / 24.0).round() as u64)
    } else if hours >= 1.0 {
        format!("~{}h", hours.round() as u64)
    } else {
        format!("~{}m", (hours * 60.0).round().max(1.0) as u64)
    };
    format!("filling · full in {when}")
}

/// A cached measurement's age, e.g. `2m ago` / `just now`. `None` reads `—`.
fn fmt_age(age_secs: Option<u64>) -> String {
    match age_secs {
        None => "—".into(),
        Some(s) if s < 5 => "just now".into(),
        Some(s) if s < 60 => format!("{s}s ago"),
        Some(s) if s < 3600 => format!("{}m ago", s / 60),
        Some(s) if s < 86_400 => format!("{}h ago", s / 3600),
        Some(s) => format!("{}d ago", s / 86_400),
    }
}

fn free_tone(pct: u8) -> Tok {
    if pct <= 5 {
        Tok::Hue(Hue::Red)
    } else if pct <= 15 {
        Tok::Hue(Hue::Amber)
    } else {
        Tok::Slot(S::Text)
    }
}

fn kind_str(k: thegn_metrics::DiskKind) -> &'static str {
    match k {
        thegn_metrics::DiskKind::Ssd => "ssd",
        thegn_metrics::DiskKind::Hdd => "hdd",
        thegn_metrics::DiskKind::Unknown => "",
    }
}

impl MonitorOverlay {
    /// Open a clean confirmation for the selected worktree row.
    pub(in crate::monitor) fn begin_clean(&mut self) -> MonitorOutcome {
        let Some(path) = self.disk_rows.get(self.sel).map(|r| r.path.clone()) else {
            return MonitorOutcome::Pending;
        };
        let label = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.confirm = Some(Confirm::Clean { path, label });
        self.status = None;
        MonitorOutcome::Pending
    }
}

pub(super) fn hints(
    input: &super::super::footer::FooterInput<'_>,
    out: &mut Vec<Vec<crate::seg::Seg>>,
) {
    if input.disk_rows > 0 {
        out.push(super::super::footer::hint("x", " clean"));
    }
}
