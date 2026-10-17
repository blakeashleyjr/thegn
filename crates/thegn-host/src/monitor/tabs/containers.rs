//! Containers tab renderer and tab-specific monitor actions.

use super::super::build::*;
use super::super::{
    ContainerReqKind, ContainerRequest, ContainerRowMeta, MonitorAction, MonitorOutcome,
    MonitorOverlay,
};
use crate::chrome::S;
use crate::sections::{Cell, Section, TableSection};
use crate::seg::Tok;
use thegn_core::theme::Hue;

pub(in crate::monitor) fn containers(cx: &Ctx, sel: usize) -> TabBuild {
    use thegn_core::sandbox_manage::{Health, container_health, human_bytes};
    let list = &cx.model.containers;

    // Aggregate footprint header. Owned counts are precise (ownership-filtered
    // listings); the byte total is the engine-wide `df` total, marked partial
    // when a detected engine has no `df` op.
    let owned = list.iter().filter(|c| c.ours).count();
    // The list deliberately includes containers thegn does not own (each row is
    // marked "(foreign)"), so the HEADING may not claim them — it used to read
    // "thegn containers" over a mixed list. The ownership split lives in the
    // note instead, where both counts are visible.
    let foreign = list.iter().filter(|c| !c.ours).count();
    let running = list
        .iter()
        .filter(|c| c.ours && thegn_core::sandbox_manage::container_running(&c.status))
        .count();
    let split = format!("{owned} owned · {foreign} foreign");
    let note = match &cx.model.container_footprint {
        Some(fp) => {
            let bytes = human_bytes(fp.total_bytes());
            format!(
                "{split} · {} img · {} vol · {}{} engine disk",
                fp.images,
                fp.volumes,
                if fp.partial { "≥" } else { "" },
                bytes,
            )
        }
        None => format!("{split} · {running} running"),
    };
    let mut out = vec![heading("containers", Some(note))];

    if list.is_empty() {
        out.push(heading("no containers", None));
        return plain(out);
    }

    let rows: Vec<Vec<Cell>> = list
        .iter()
        .map(|c| {
            // Ownership is the SOLE foreground rule. The cursor used to
            // overwrite it, which destroyed the ours/foreign signal the whole
            // tab is built on for exactly the row the user was acting upon; the
            // cursor is now the table's `sel` background instead.
            let name_tone = if c.ours {
                Tok::Hue(Hue::Green)
            } else {
                Tok::Slot(S::Ghost)
            };
            let (health_txt, health_tone) = match container_health(&c.status) {
                Health::Healthy => ("healthy", Tok::Hue(Hue::Green)),
                Health::Unhealthy => ("unhealthy", Tok::Hue(Hue::Red)),
                Health::Starting => ("starting", Tok::Hue(Hue::Amber)),
                Health::None => ("up", Tok::Slot(S::Text)),
                Health::Stopped => ("stopped", Tok::Slot(S::Dim)),
            };
            // Foreign containers are visibly read-only.
            let owned_mark = if c.ours { "" } else { " (foreign)" };
            vec![
                Cell::Text(format!("{}{owned_mark}", trunc(&c.name, 26)), name_tone),
                Cell::Text(trunc(&c.backend, 14), Tok::Slot(S::Ghost)),
                Cell::Text(health_txt.into(), health_tone),
                Cell::Text(
                    if c.cpu.is_empty() {
                        "—".into()
                    } else {
                        c.cpu.clone()
                    },
                    Tok::Hue(Hue::Teal),
                ),
                Cell::Text(
                    if c.mem.is_empty() {
                        "—".into()
                    } else {
                        c.mem.clone()
                    },
                    Tok::Hue(Hue::Purple),
                ),
                Cell::Text(
                    if c.net.is_empty() {
                        "—".into()
                    } else {
                        c.net.clone()
                    },
                    Tok::Slot(S::Dim),
                ),
            ]
        })
        .collect();
    let row_y = row_ys(&out, rows.len(), true);
    out.push(Section::Table(TableSection {
        header: vec![
            "container".into(),
            "backend".into(),
            "health".into(),
            "cpu".into(),
            "mem".into(),
            "net".into(),
        ],
        rows,
        sel: Some(sel),
    }));
    TabBuild {
        sections: out,
        row_y,
    }
}

pub(in crate::monitor) fn row_meta(model: &crate::chrome::FrameModel) -> Vec<ContainerRowMeta> {
    model
        .containers
        .iter()
        .map(|c| ContainerRowMeta {
            name: c.name.clone(),
            backend: c.backend.clone(),
            ours: c.ours,
            running: thegn_core::sandbox_manage::container_running(&c.status),
        })
        .collect()
}

impl MonitorOverlay {
    /// Containers-tab row actions. Reached only after every global key; offered
    /// only on an OWNED row (foreign containers are read-only, so their keys are
    /// no-ops). Records the request and hands the loop [`MonitorOutcome::Action`]
    /// — the overlay can't spawn a subprocess or open a pane itself.
    ///
    /// Keys: `t` stop, `r` restart, `x` remove, `o` logs, `Enter` shell-in.
    /// (`s`/`g`/`l` are global — scale/graph/switch — so stop is `t`, not `s`.)
    pub(in crate::monitor) fn container_key(&mut self, c: char) -> MonitorOutcome {
        let kind = match c {
            't' => ContainerReqKind::Stop,
            'r' => ContainerReqKind::Restart,
            'x' => ContainerReqKind::Remove,
            'o' => ContainerReqKind::Logs,
            '\r' | '\n' => ContainerReqKind::Shell,
            _ => return MonitorOutcome::Pending,
        };
        let Some(row) = self.container_rows.get(self.sel).cloned() else {
            return MonitorOutcome::Pending;
        };
        // Actions are offered only on OWNED rows — foreign containers are
        // read-only on every surface.
        if !row.ours {
            return MonitorOutcome::Pending;
        }
        // Remove is a two-press confirm; a running container gets a force
        // warning (the "second confirmation when running" the spec calls for).
        if kind == ContainerReqKind::Remove
            && self.remove_armed.as_deref() != Some(row.name.as_str())
        {
            self.remove_armed = Some(row.name.clone());
            self.notice = Some(if row.running {
                format!("remove RUNNING {}? press x again to force-remove", row.name)
            } else {
                format!("remove {}? press x again to confirm", row.name)
            });
            return MonitorOutcome::Pending;
        }
        self.remove_armed = None;
        self.pending_action = Some(MonitorAction::Container(ContainerRequest {
            kind,
            name: row.name,
            backend: row.backend,
            running: row.running,
        }));
        MonitorOutcome::Action
    }
}

pub(super) fn hints(
    input: &super::super::footer::FooterInput<'_>,
    out: &mut Vec<Vec<crate::seg::Seg>>,
) {
    use super::super::footer::{ghost, hint};
    if input.container_ours {
        out.push(hint("↵", " shell"));
        out.push(hint("o", " logs"));
        out.push(hint("t", " stop"));
        out.push(hint("r", " restart"));
        out.push(hint("x", " remove"));
    } else {
        out.push(vec![ghost("foreign container — read-only")]);
    }
}
