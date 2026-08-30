//! Host edge for editor/IDE handoff.
//!
//! Core owns target validation and provider argv planning. This module is the
//! one place the compositor resolves that plan and launches it: environment
//! lookup, filesystem revalidation, CPU-cap probing, and external spawning all
//! happen on a blocking worker. Pane plans return to the event loop over the
//! normal channel and are opened through the existing pane/tab chokepoints.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc::UnboundedSender;

use thegn_core::editor::{EditorLaunch, EditorTarget, Placement};

/// How long a queued control handoff remains actionable (seconds).
///
/// The intent mailbox is asynchronous, but an editor launch must not survive a
/// compositor outage and surprise the user on a much later restart. Keep this
/// aligned with the existing `adopt_session` mailbox policy.
const MAX_EDITOR_OPEN_AGE_SECS: i64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PanePlacement {
    Tab,
    Split(u32),
}

#[derive(Debug)]
pub(crate) enum Outcome {
    Pane {
        launch: EditorLaunch,
        placement: PanePlacement,
        source: String,
        fallback: Option<String>,
    },
    Status(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentPayload {
    worktree: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<usize>,
    #[serde(default)]
    col: Option<usize>,
    source: String,
}

/// Queue one validated target for off-loop planning/launch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch(
    target: EditorTarget,
    workspace_slug: String,
    source: impl Into<String>,
    placement: PanePlacement,
    cfg: &thegn_core::config::Config,
    tx: &UnboundedSender<Outcome>,
    waker: &TerminalWaker,
) {
    let cfg = cfg.clone();
    let source = source.into();
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        let outcome = plan_and_launch(target, &workspace_slug, source, placement, &cfg);
        if tx.send(outcome).is_ok() {
            // Best-effort: the loop may already be shutting down.
            drop(waker.wake());
        }
    });
}

fn plan_and_launch(
    target: EditorTarget,
    workspace_slug: &str,
    source: String,
    placement: PanePlacement,
    cfg: &thegn_core::config::Config,
) -> Outcome {
    if let Err(message) = revalidate(&target) {
        return Outcome::Status(format!("{source}: {message}"));
    }
    let editor = thegn_core::editor::editor_for_workspace(cfg, workspace_slug);
    let (launch, fallback) = match plan_with_fallback(editor.as_ref(), &target) {
        Ok(planned) => planned,
        Err(error) => {
            return Outcome::Status(format!("{source}: IDE handoff unavailable: {error}"));
        }
    };
    match launch.placement {
        Placement::Pane => Outcome::Pane {
            launch,
            placement,
            source,
            fallback,
        },
        Placement::External => {
            let label = target_label(&target);
            if spawn_external(&launch) {
                let fallback = fallback.map_or_else(String::new, |note| format!("; {note}"));
                Outcome::Status(format!("Opened {label} in {}{fallback}", launch.provider))
            } else {
                Outcome::Status(format!(
                    "{source}: failed to launch {} for {label}",
                    launch.provider
                ))
            }
        }
    }
}

fn plan_with_fallback(
    editor: &dyn thegn_core::editor::Editor,
    target: &EditorTarget,
) -> Result<(EditorLaunch, Option<String>), thegn_core::editor::EditorError> {
    let caps = editor.caps();
    let Some(file) = target.relative_file() else {
        return editor.open_target(target).map(|launch| (launch, None));
    };

    let (planned, fallback) = if target.line().is_some() && !caps.line {
        (
            EditorTarget::file(target.worktree(), file, None, None)?,
            Some(format!(
                "{} does not support line locations; opened the file only",
                editor.id()
            )),
        )
    } else if target.col().is_some() && !caps.column {
        (
            EditorTarget::file(target.worktree(), file, target.line(), None)?,
            Some(format!(
                "{} does not support columns; opened the requested line",
                editor.id()
            )),
        )
    } else {
        (target.clone(), None)
    };
    editor
        .open_target(&planned)
        .map(|launch| (launch, fallback))
}

fn revalidate(target: &EditorTarget) -> Result<(), String> {
    let worktree = target.worktree().canonicalize().map_err(|_| {
        format!(
            "worktree is missing or unreadable: {}",
            target.worktree().display()
        )
    })?;
    if !worktree.is_dir() {
        return Err(format!(
            "worktree is missing or unreadable: {}",
            target.worktree().display()
        ));
    }
    if target.relative_file().is_some() {
        let path = target.path();
        let resolved = path
            .canonicalize()
            .map_err(|_| format!("file is missing or unreadable: {}", path.display()))?;
        if !resolved.starts_with(&worktree) {
            return Err(format!(
                "file resolves outside the worktree: {}",
                path.display()
            ));
        }
        if !resolved.is_file() {
            return Err(format!("file is missing or unreadable: {}", path.display()));
        }
    }
    Ok(())
}

fn spawn_external(launch: &EditorLaunch) -> bool {
    let argv = thegn_core::sandbox_cpucap::wrap_background_argv(launch.argv.clone());
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&launch.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::actions::spawn_detached_reaped(command)
}

fn target_label(target: &EditorTarget) -> String {
    target
        .relative_file()
        .map_or_else(|| "worktree".to_string(), |path| path.display().to_string())
}

/// Build an active-worktree target without allowing a caller to bypass the
/// core containment policy.
pub(crate) fn active_target(
    session: &crate::session::Session,
    path: Option<&str>,
    line: Option<usize>,
    col: Option<usize>,
) -> Result<(EditorTarget, String), String> {
    let group = session
        .active_group()
        .filter(|group| !group.path.trim().is_empty())
        .ok_or_else(|| "Select a worktree before opening it in an IDE".to_string())?;
    let target = match path {
        Some(path) => EditorTarget::file(&group.path, path, line, col),
        None => EditorTarget::project(&group.path),
    }
    .map_err(|error| error.to_string())?;
    // Config workspace keys use the pure path-derived slug. Do not call
    // `repo_slug` here: it opens SQLite and this request phase runs on the UI
    // loop.
    let slug = thegn_core::config::workspace_slug(Path::new(if session.id.is_empty() {
        &group.path
    } else {
        &session.id
    }));
    Ok((target, slug))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_active(
    session: &crate::session::Session,
    path: Option<&str>,
    line: Option<usize>,
    col: Option<usize>,
    source: &'static str,
    placement: PanePlacement,
    cfg: &thegn_core::config::Config,
    tx: &UnboundedSender<Outcome>,
    waker: &TerminalWaker,
) -> Result<(), String> {
    let (target, workspace_slug) = active_target(session, path, line, col)?;
    dispatch(target, workspace_slug, source, placement, cfg, tx, waker);
    Ok(())
}

/// Parse and revalidate a claimed control intent against the compositor's
/// known worktree rows. An absolute-but-unregistered path is stale, not a
/// license for a remote caller to open an arbitrary host directory.
pub(crate) fn target_from_intent(
    row: &thegn_core::store::IntentRow,
    known: impl IntoIterator<Item = (PathBuf, String)>,
    now_secs: i64,
) -> Result<(EditorTarget, String, String), String> {
    // Future timestamps from clock skew are fresh; only positive age expires.
    if now_secs.saturating_sub(row.created_at) > MAX_EDITOR_OPEN_AGE_SECS {
        return Err("stale open_editor intent dropped: request expired".into());
    }
    let payload: IntentPayload = serde_json::from_str(&row.payload)
        .map_err(|error| format!("malformed open_editor intent dropped: {error}"))?;
    let target = EditorTarget::new(
        PathBuf::from(&payload.worktree),
        payload.path.as_deref(),
        payload.line,
        payload.col,
    )
    .map_err(|error| format!("invalid open_editor intent dropped: {error}"))?;
    let slug = known
        .into_iter()
        .find_map(|(path, slug)| (path == target.worktree()).then_some(slug))
        .ok_or_else(|| {
            format!(
                "stale open_editor intent from {} dropped: unknown worktree {}",
                payload.source,
                target.worktree().display()
            )
        })?;
    Ok((target, slug, payload.source))
}

pub(crate) fn known_worktrees(
    model: &crate::chrome::FrameModel,
    session: &crate::session::Session,
) -> Vec<(PathBuf, String)> {
    let mut known: Vec<(PathBuf, String)> = model
        .sidebar_rows
        .iter()
        .filter(|row| row.kind == crate::sidebar::RowKind::Worktree)
        .filter_map(|row| {
            row.worktree_path
                .as_deref()
                .map(|path| (PathBuf::from(path), row.workspace_slug.clone()))
        })
        .collect();
    let active_slug = (!session.id.is_empty())
        .then(|| thegn_core::config::workspace_slug(Path::new(&session.id)));
    if let Some(slug) = active_slug {
        for group in &session.worktrees {
            let path = PathBuf::from(&group.path);
            if !group.path.is_empty() && !known.iter().any(|(known, _)| known == &path) {
                known.push((path, slug.clone()));
            }
        }
    }
    known
}

/// Apply a worker result on the loop. Pane launches are accepted only while
/// their worktree is still focused; this prevents a slow plan from opening in
/// a different tab after the user switches away.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply(
    outcome: Outcome,
    session: &mut crate::session::Session,
    panes: &mut crate::panes::Panes,
    focus: &mut crate::focus::FocusState,
    model: &mut crate::chrome::FrameModel,
    sb: &mut crate::run::SidebarState,
    center: crate::compositor::Rect,
) -> bool {
    match outcome {
        Outcome::Status(status) => {
            model.status = status;
            false
        }
        Outcome::Pane {
            launch,
            placement,
            source,
            fallback,
        } => {
            let focused = session.active_group().map(|group| Path::new(&group.path))
                == Some(launch.cwd.as_path());
            if !focused {
                model.status = format!(
                    "{source}: terminal editor needs {} focused; handoff cancelled",
                    launch.cwd.display()
                );
                return false;
            }
            let opened = match placement {
                PanePlacement::Tab => crate::actions::open_argv_tab(
                    session,
                    panes,
                    &launch.argv,
                    Some(&launch.cwd),
                    center,
                ),
                PanePlacement::Split(pane) => crate::actions::open_argv_pane(
                    session,
                    panes,
                    pane,
                    &launch.argv,
                    Some(&launch.cwd),
                    center,
                ),
            };
            if opened {
                focus.zone = crate::focus::Zone::Center;
                crate::run::refresh_tab_model(model, session, sb);
                let fallback = fallback.map_or_else(String::new, |note| format!("; {note}"));
                model.status = format!(
                    "Opened {} in {}{fallback}",
                    target_from_launch(&launch),
                    launch.provider,
                );
            } else {
                model.status = format!("{source}: failed to open terminal editor pane");
            }
            opened
        }
    }
}

fn target_from_launch(launch: &EditorLaunch) -> String {
    match launch.operation {
        thegn_core::editor::EditorOperation::OpenDirectory => "worktree".into(),
        thegn_core::editor::EditorOperation::OpenFile => {
            launch.argv.last().cloned().unwrap_or_else(|| "file".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_at(payload: &str, created_at: i64) -> thegn_core::store::IntentRow {
        thegn_core::store::IntentRow {
            id: 1,
            kind: "open_editor".into(),
            payload: payload.into(),
            created_at,
        }
    }

    fn row(payload: &str) -> thegn_core::store::IntentRow {
        row_at(payload, 1)
    }

    #[test]
    fn intent_reuses_core_target_policy_and_known_worktree_identity() {
        let intent = row(
            r#"{"worktree":"/repo/wt","path":"src/lib.rs","line":7,"col":2,"source":"control_api"}"#,
        );
        let (target, slug, source) = target_from_intent(
            &intent,
            [(PathBuf::from("/repo/wt"), "repo".to_string())],
            1,
        )
        .unwrap();
        assert_eq!(target.relative_file(), Some(Path::new("src/lib.rs")));
        assert_eq!(target.line(), Some(7));
        assert_eq!(slug, "repo");
        assert_eq!(source, "control_api");
    }

    #[test]
    fn malformed_escaping_and_stale_intents_are_dropped() {
        let escaping = row(r#"{"worktree":"/repo/wt","path":"../secret","source":"control_api"}"#);
        assert!(
            target_from_intent(&escaping, [], 1)
                .unwrap_err()
                .contains("invalid")
        );

        let stale = row(r#"{"worktree":"/repo/gone","source":"control_api"}"#);
        assert!(
            target_from_intent(&stale, [(PathBuf::from("/repo/wt"), "repo".into())], 1,)
                .unwrap_err()
                .contains("stale")
        );
    }

    #[test]
    fn unknown_intent_fields_fail_closed() {
        let intent = row(r#"{"worktree":"/repo/wt","source":"control_api","argv":["bad"]}"#);
        assert!(
            target_from_intent(&intent, [], 1)
                .unwrap_err()
                .contains("malformed")
        );
    }

    #[test]
    fn expired_intents_are_dropped_but_boundary_and_future_rows_are_fresh() {
        let payload = r#"{"worktree":"/repo/wt","source":"control_api"}"#;
        let known = || [(PathBuf::from("/repo/wt"), "repo".to_string())];
        assert!(
            target_from_intent(&row_at(payload, 10), known(), 311)
                .unwrap_err()
                .contains("expired")
        );
        assert!(target_from_intent(&row_at(payload, 10), known(), 310).is_ok());
        assert!(target_from_intent(&row_at(payload, 500), known(), 100).is_ok());
    }

    #[test]
    fn filesystem_revalidation_rejects_symlink_escape() {
        if !crate::platform::test_symlink_supported() {
            return;
        }

        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("worktree");
        let outside = root.path().join("outside.rs");
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(&outside, "secret").unwrap();
        crate::platform::test_symlink(&outside, &worktree.join("linked.rs")).unwrap();

        let escaped = EditorTarget::file(&worktree, "linked.rs", None, None).unwrap();
        assert!(
            revalidate(&escaped)
                .unwrap_err()
                .contains("outside the worktree")
        );

        std::fs::write(worktree.join("inside.rs"), "safe").unwrap();
        let inside = EditorTarget::file(&worktree, "inside.rs", None, None).unwrap();
        assert!(revalidate(&inside).is_ok());
    }

    #[test]
    fn unsupported_columns_fall_back_visibly_to_the_requested_line() {
        let editor = thegn_core::editor::providers::provider(
            thegn_core::editor::EditorProvider::Jetbrains,
            thegn_core::config::EditorOpenIn::Auto,
        )
        .unwrap();
        let target = EditorTarget::file("/repo/wt", "src/lib.rs", Some(7), Some(2)).unwrap();
        let (launch, fallback) = plan_with_fallback(editor.as_ref(), &target).unwrap();
        assert_eq!(
            launch.argv,
            ["idea", "--line", "7", "/repo/wt/src/lib.rs"].map(String::from)
        );
        assert!(fallback.is_some_and(|note| note.contains("does not support columns")));
    }
}
