//! The project/worktree Info popup.
//!
//! The first frame is a cheap loading shell.  Gathering the report is blocking
//! work and happens on a worker; the result is only applied while the same
//! titled popup is still open.

use std::path::Path;
use std::time::Instant;

use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc::UnboundedSender;

use super::{DaemonSessions, DetailOverlay, Placement, Section, TableSection};
use crate::chrome::{FrameModel, S};
use crate::hydrate::RefreshKind;
use crate::seg::Tok;
use crate::sidebar::{ActivityState, RowKind, SidebarRow};
use thegn_core::config::Config;
use thegn_core::info_model::{self, GitFacts, InfoReport, InfoTarget, LiveFacts, Source, Tone};
use thegn_core::theme::Hue;

#[derive(Debug, Clone)]
pub(crate) struct InfoPayload {
    pub title: String,
    pub report: Result<InfoReport, String>,
}

/// Resolve the row under the cursor (or the active worktree fallback) into
/// the target and the cheap facts already present in the sidebar/cache.
pub(crate) fn target_for(
    row: Option<&SidebarRow>,
    model: &FrameModel,
    active: Option<String>,
    sessions: &DaemonSessions,
    sessions_at: Option<Instant>,
) -> Option<(InfoTarget, LiveFacts, String)> {
    let row = row.or_else(|| {
        active.as_deref().and_then(|path| {
            model.sidebar_rows.iter().find(|candidate| {
                candidate.kind.is_worktree() && candidate.worktree_path.as_deref() == Some(path)
            })
        })
    })?;

    let (target, label) = match row.kind {
        RowKind::Workspace => (
            InfoTarget {
                repo_root: row.worktree_path.clone()?,
                worktree: None,
            },
            row.label.clone(),
        ),
        RowKind::Worktree | RowKind::PipelineWorktree => {
            let worktree = row.worktree_path.clone()?;
            (
                InfoTarget {
                    repo_root: String::new(),
                    worktree: Some(worktree),
                },
                format!(
                    "{}/{}",
                    row.workspace_slug,
                    row.branch.clone().unwrap_or_else(|| row.label.clone())
                ),
            )
        }
        _ => return None,
    };

    let workspace_ahead = (row.kind == RowKind::Workspace).then(|| {
        let mut ahead = 0;
        let mut known = 0;
        for sibling in model.sidebar_rows.iter().filter(|candidate| {
            candidate.visible
                && candidate.kind.is_worktree()
                && candidate.workspace_slug == row.workspace_slug
        }) {
            if let Some(meta) = sibling.branch_meta.as_ref() {
                known += 1;
                if meta.ahead_base.is_some_and(|n| n > 0) {
                    ahead += 1;
                }
            }
        }
        (ahead, known)
    });

    let git = (row.branch_meta.is_some() || row.git.is_some()).then(|| GitFacts {
        dirty_files: row.branch_meta.as_ref().and_then(|meta| meta.dirty_files),
        ahead_base: row.branch_meta.as_ref().and_then(|meta| meta.ahead_base),
        behind_base: row.branch_meta.as_ref().and_then(|meta| meta.behind_base),
        ahead_upstream: None,
        behind_upstream: None,
        upstream: None,
    });

    let (sessions, sessions_source) = match sessions {
        DaemonSessions::Live(v) => (
            Some(
                v.iter()
                    .filter(|session| session.exited_at_ms.is_none())
                    .map(crate::cmd::info::session_row)
                    .collect(),
            ),
            Some(Source::Cache {
                name: "daemon sessions".into(),
                at: sessions_at.map(|timestamp| {
                    thegn_core::util::now() - timestamp.elapsed().as_secs() as i64
                }),
            }),
        ),
        _ => (None, None),
    };

    let facts = LiveFacts {
        read_git: false,
        git,
        git_source: Some(Source::Sidebar),
        activity: (row.activity != ActivityState::None)
            .then(|| format!("{:?}", row.activity).to_lowercase()),
        pipeline_stage: row.pipeline_stage.clone(),
        devcontainer: row.devcontainer_status.clone(),
        sessions,
        sessions_source,
        workspace_ahead,
    };
    Some((target, facts, format!("Info: {label}")))
}

pub(crate) fn loading(title: &str, screen: crate::compositor::Rect) -> DetailOverlay {
    super::sections(
        title,
        100,
        vec![Section::Heading {
            label: "reading…".into(),
            note: None,
        }],
        Placement::Center,
        screen,
    )
}

pub(crate) fn spawn(
    tx: UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
    cfg: Config,
    target: InfoTarget,
    facts: LiveFacts,
    title: String,
) {
    tokio::task::spawn_blocking(move || {
        let report = build_report(&cfg, target, facts).map_err(|error| format!("{error:#}"));

        if let Err(error) = tx.send(RefreshKind::Info(Box::new(InfoPayload { title, report }))) {
            tracing::debug!(target: "thegn::info", %error, "info result receiver is gone");
        }
        if let Err(error) = waker.wake() {
            tracing::debug!(target: "thegn::info", %error, "info result wake failed");
        }
    });
}

fn build_report(
    cfg: &Config,
    mut target: InfoTarget,
    facts: LiveFacts,
) -> anyhow::Result<InfoReport> {
    if target.repo_root.is_empty() {
        let worktree = target
            .worktree
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Info target has no worktree path"))?;
        let db = thegn_core::db::Db::open()?;
        target = thegn_core::info_facts::resolve_target(
            &db,
            None,
            Some(&worktree),
            Path::new(&worktree),
        )?;
    }
    crate::cmd::info::gather(
        cfg,
        Some(crate::hydrate::config_source_path().unwrap_or_else(thegn_core::config::Config::path)),
        &target,
        facts,
    )
}

pub(crate) fn apply(
    slot: &mut Option<DetailOverlay>,
    payload: InfoPayload,
    screen: crate::compositor::Rect,
) -> bool {
    let Some(open) = slot.as_ref() else {
        return false;
    };
    if open.title != payload.title {
        return false;
    }

    let sections = match payload.report {
        Ok(report) => {
            let mut sections = Vec::new();
            for section in info_model::sections(&report) {
                let rows = section
                    .lines
                    .into_iter()
                    .map(|line| {
                        vec![
                            super::Cell::Text(line.label, Tok::Slot(S::Dim)),
                            super::Cell::Text(line.value, line_tone(line.tone)),
                            super::Cell::Text(line.source, Tok::Slot(S::Ghost)),
                        ]
                    })
                    .collect();
                sections.push(Section::Heading {
                    label: section.title,
                    note: None,
                });
                sections.push(Section::Table(TableSection {
                    header: vec![],
                    rows,
                    sel: None,
                }));
            }
            sections
        }
        Err(error) => vec![
            Section::Heading {
                label: "could not build the Info screen".into(),
                note: None,
            },
            Section::Table(TableSection {
                header: vec![],
                rows: vec![vec![super::Cell::Text(
                    info_model::clean(&error, 200),
                    Tok::Slot(S::Text),
                )]],
                sel: None,
            }),
        ],
    };
    *slot = Some(super::sections(
        &payload.title,
        100,
        sections,
        Placement::Center,
        screen,
    ));
    true
}

fn line_tone(tone: Tone) -> Tok {
    match tone {
        Tone::Normal => Tok::Slot(S::Text),
        Tone::Muted => Tok::Slot(S::Ghost),
        Tone::Warn => Tok::Hue(Hue::Amber),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detail::{DetailContent, SectionsDetail};
    use crate::sidebar::SidebarRow;
    use thegn_core::info_model::{Field, InfoReport};
    use thegn_svc::git::BranchMeta;

    fn report() -> InfoReport {
        InfoReport {
            schema: 1,
            generated_at: 0,
            target: InfoTarget {
                repo_root: "/repo".into(),
                worktree: None,
            },
            workspace: vec![Field::value("name", "name", "app", Source::Sidebar)],
            workspace_stats: vec![],
            worktree: vec![],
            sessions: None,
            settings_scope: "workspace".into(),
            settings: vec![],
            backends: vec![],
            overrides: vec![],
            notes: vec![],
        }
    }

    #[test]
    fn apply_ignores_other_title_and_replaces_matching_overlay() {
        let screen = crate::compositor::Rect::full(120, 40);
        let mut slot = Some(loading("Info: app", screen));
        assert!(!apply(
            &mut slot,
            InfoPayload {
                title: "Info: other".into(),
                report: Ok(report()),
            },
            screen,
        ));
        assert!(matches!(
            slot.as_ref().unwrap().content,
            DetailContent::Sections(_)
        ));
        assert!(apply(
            &mut slot,
            InfoPayload {
                title: "Info: app".into(),
                report: Ok(report()),
            },
            screen,
        ));
        let overlay = slot.unwrap();
        let DetailContent::Sections(SectionsDetail { sections, .. }) = &overlay.content else {
            panic!("expected sections");
        };
        assert_eq!(sections.len(), 2);
        assert!(matches!(sections[0], Section::Heading { .. }));
        assert!(matches!(sections[1], Section::Table(_)));
    }

    #[test]
    fn apply_renders_error_message() {
        let screen = crate::compositor::Rect::full(120, 40);
        let mut slot = Some(loading("Info: app", screen));
        assert!(apply(
            &mut slot,
            InfoPayload {
                title: "Info: app".into(),
                report: Err("bad config".into()),
            },
            screen,
        ));
        let overlay = slot.unwrap();
        let DetailContent::Sections(SectionsDetail { sections, .. }) = &overlay.content else {
            panic!("expected sections");
        };
        let Section::Table(table) = &sections[1] else {
            panic!("expected error table");
        };
        assert!(
            matches!(&table.rows[0][0], super::super::Cell::Text(text, _) if text == "bad config")
        );
    }

    #[test]
    fn target_for_workspace_counts_visible_siblings() {
        let mut model = FrameModel::default();
        let mut workspace = SidebarRow::base(RowKind::Workspace, 0, "app", "app");
        workspace.worktree_path = Some("/repo".into());
        let mut ahead = SidebarRow::base(RowKind::Worktree, 1, "feat", "app");
        ahead.branch_meta = Some(BranchMeta {
            ahead_base: Some(2),
            ..Default::default()
        });
        let mut clean = SidebarRow::base(RowKind::Worktree, 1, "main", "app");
        clean.branch_meta = Some(BranchMeta {
            ahead_base: Some(0),
            ..Default::default()
        });
        model.sidebar_rows = vec![workspace, ahead, clean];
        let (target, facts, title) = target_for(
            model.sidebar_rows.first(),
            &model,
            None,
            &DaemonSessions::Unknown,
            None,
        )
        .expect("workspace target");
        assert_eq!(target.repo_root, "/repo");
        assert_eq!(facts.workspace_ahead, Some((1, 2)));
        assert_eq!(title, "Info: app");
    }

    #[test]
    fn target_for_folder_is_none() {
        let row = SidebarRow::base(RowKind::Folder, 1, "folder", "app");
        assert!(
            target_for(
                Some(&row),
                &FrameModel::default(),
                None,
                &DaemonSessions::Unknown,
                None,
            )
            .is_none()
        );
    }
}
