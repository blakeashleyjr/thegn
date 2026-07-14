//! The Alt+W "new workspace" flow's non-loop halves: input classification
//! (path / URL / new-project), clone-by-URL, and the create+switch completion
//! shared by the typed prompt, the fuzzy picker, and the clone-finished drain
//! arm. Extracted from `run.rs` (god-file ratchet).

use anyhow::{Context, Result};

use thegn_core::store::WorkspaceStore;

use crate::chrome::FrameModel;
use crate::compositor::Rect;
use crate::menu::{self, MenuOverlay};
use crate::panes::Panes;
use crate::run::{
    DrawerPool, ResidentWorkspace, SidebarState, WorkspacePool, persist_session_layout,
    refresh_tab_model, remap_cold_workspace_ids, sync_drawer_persistence,
};

fn looks_like_git_url(input: &str) -> bool {
    input.starts_with("http://")
        || input.starts_with("https://")
        || input.starts_with("ssh://")
        || input.starts_with("git://")
        || input.starts_with("git@")
}

fn workspace_repo_name_from_url(input: &str) -> String {
    let trimmed = input.trim_end_matches('/');
    let tail = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    let name = tail.strip_suffix(".git").unwrap_or(tail);
    let slug = thegn_core::util::slugify(name);
    if slug.is_empty() {
        "workspace".into()
    } else {
        name.to_string()
    }
}

pub(crate) enum WorkspaceResolution {
    Repo(std::path::PathBuf),
    NotARepo(std::path::PathBuf),
}

/// Where a cloned-by-URL workspace lands (`[ui].workspaces_dir` / repo name);
/// `None` for local-path inputs.
pub(crate) fn workspace_clone_dest(
    input: &str,
    cfg: &thegn_core::config::Config,
) -> Option<std::path::PathBuf> {
    looks_like_git_url(input).then(|| {
        std::path::PathBuf::from(thegn_core::util::expand_tilde(&cfg.workspaces_dir))
            .join(workspace_repo_name_from_url(input))
    })
}

/// Clone `url` into `dest`, forwarding each `git clone --progress` line to
/// `on_progress`. BLOCKING (a clone can take minutes) — only call inside
/// `spawn_blocking`; the NewWorkspace flow completes over `workspace_clone_rx`.
pub(crate) fn clone_workspace_repo(
    url: &str,
    dest: &std::path::Path,
    on_progress: impl FnMut(String),
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    // Via the scrubbed git_cmd so a stray GIT_DIR can't redirect the
    // clone; dest is absolute, so the `-C` only sets the working dir.
    let cwd = dest.parent().unwrap_or(std::path::Path::new("."));
    // `--progress` forces git to emit its transfer counters even though stderr
    // is a pipe, not a TTY; we stream them to the picker's cloning panel.
    let mut child = thegn_core::util::git_cmd(cwd)
        .arg("clone")
        .arg("--progress")
        .arg(url)
        .arg(dest)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("git clone {url} {}", dest.display()))?;
    if let Some(stderr) = child.stderr.take() {
        stream_progress_lines(std::io::BufReader::new(stderr), on_progress);
    }
    // off-loop: this blocks until the clone finishes, but the whole helper only
    // runs inside spawn_blocking (see spawn_workspace_clone).
    #[expect(clippy::disallowed_methods)]
    let status = child
        .wait()
        .with_context(|| format!("git clone {url} {}", dest.display()))?;
    anyhow::ensure!(status.success(), "git clone failed for {url}");
    Ok(())
}

/// Read `git`'s progress stream, calling `on_line` with each finished segment.
/// git rewrites its progress counter in place with a carriage return, so we
/// break on `\r` *and* `\n` (trimming, dropping empties) rather than by line.
pub(crate) fn stream_progress_lines<R: std::io::BufRead>(
    reader: R,
    mut on_line: impl FnMut(String),
) {
    let mut buf: Vec<u8> = Vec::new();
    let flush = |buf: &mut Vec<u8>, on_line: &mut dyn FnMut(String)| {
        let s = String::from_utf8_lossy(buf).trim().to_string();
        buf.clear();
        if !s.is_empty() {
            on_line(s);
        }
    };
    for byte in reader.bytes() {
        let Ok(b) = byte else { break };
        if b == b'\r' || b == b'\n' {
            flush(&mut buf, &mut on_line);
        } else {
            buf.push(b);
        }
    }
    flush(&mut buf, &mut on_line);
}

/// Result of a background workspace clone, delivered to the loop's
/// `workspace_clone_rx` drain arm.
pub(crate) struct WorkspaceCloneOutcome {
    pub(crate) url: String,
    pub(crate) dest: std::path::PathBuf,
    pub(crate) result: Result<()>,
}

/// What the background clone sends over `workspace_clone_rx`: streamed progress
/// lines while it runs, then a terminal `Done`.
pub(crate) enum CloneEvent {
    Progress { url: String, line: String },
    Done(WorkspaceCloneOutcome),
}

/// What a manual new-workspace input resolves to. Pure classification — the
/// loop maps each variant onto the matching flow (off-loop clone, create+
/// switch, or a create-project confirm).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SubmitPlan {
    /// A git URL whose clone dest isn't on disk yet → clone OFF the loop.
    Clone {
        url: String,
        dest: std::path::PathBuf,
    },
    /// An existing local path (or an already-cloned URL dest): repo →
    /// create+switch; non-repo dir → the git-init offer.
    Local(String),
    /// A path that doesn't exist but whose PARENT does — a new project one
    /// level below the present tree → confirm, then mkdir + git init.
    CreateNew {
        leaf: std::path::PathBuf,
    },
    Invalid(String),
}

/// Classify a typed new-workspace input (path / URL / new-project leaf).
pub(crate) fn plan_new_workspace_input(
    input: &str,
    cfg: &thegn_core::config::Config,
) -> SubmitPlan {
    let input = input.trim();
    if input.is_empty() {
        return SubmitPlan::Invalid("no workspace path or URL given".into());
    }
    if let Some(dest) = workspace_clone_dest(input, cfg) {
        // An already-materialized clone dest is just a local open.
        if dest.exists() {
            return SubmitPlan::Local(input.to_string());
        }
        return SubmitPlan::Clone {
            url: input.to_string(),
            dest,
        };
    }
    let expanded = thegn_core::util::expand_tilde(input);
    let path = std::path::PathBuf::from(expanded);
    let path = if path.is_absolute() {
        path
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        return SubmitPlan::Invalid("cannot resolve relative path".into());
    };
    if path.is_dir() {
        SubmitPlan::Local(input.to_string())
    } else if !path.exists() && path.parent().is_some_and(|p| p.is_dir()) {
        SubmitPlan::CreateNew { leaf: path }
    } else {
        SubmitPlan::Invalid(format!("path does not exist: {}", path.display()))
    }
}

/// mkdir the leaf (its parent is verified to exist — new projects only nest
/// one level below the present tree) + `git init`. Runs on an explicit user
/// confirm; ms-scale like the ConfirmInitGit arm.
pub(crate) fn init_new_project(leaf: &std::path::Path) -> Result<()> {
    anyhow::ensure!(
        leaf.parent().is_some_and(|p| p.is_dir()),
        "parent directory of {} does not exist",
        leaf.display()
    );
    std::fs::create_dir(leaf).with_context(|| format!("create {}", leaf.display()))?;
    // Accepted on-loop subprocess: `git init` on a local path is ms-scale and
    // runs on an explicit user confirm (same stance as the ConfirmInitGit arm).
    #[expect(clippy::disallowed_methods)]
    let status = thegn_core::util::git_cmd(leaf)
        .arg("init")
        .status()
        .with_context(|| format!("git init {}", leaf.display()))?;
    anyhow::ensure!(status.success(), "git init failed for {}", leaf.display());
    Ok(())
}

/// Run the (minutes-scale) clone on `spawn_blocking`; the loop completes the
/// create+switch in its `workspace_clone_rx` drain arm.
pub(crate) fn spawn_workspace_clone(
    url: String,
    dest: std::path::PathBuf,
    tx: tokio::sync::mpsc::UnboundedSender<CloneEvent>,
    waker: termwiz::terminal::TerminalWaker,
) {
    tokio::task::spawn_blocking(move || {
        let progress_url = url.clone();
        let progress_tx = tx.clone();
        let progress_waker = waker.clone();
        let result = clone_workspace_repo(&url, &dest, |line| {
            // best-effort: the picker may have been cancelled
            let _ = progress_tx.send(CloneEvent::Progress {
                url: progress_url.clone(),
                line,
            });
            let _ = progress_waker.wake();
        });
        let _ = tx.send(CloneEvent::Done(WorkspaceCloneOutcome {
            url,
            dest,
            result,
        }));
        let _ = waker.wake();
    });
}

pub(crate) fn create_workspace_from_input_with_config(
    input: &str,
    session: &mut crate::session::Session,
    db: &thegn_core::db::Db,
    cfg: &thegn_core::config::Config,
) -> Result<WorkspaceResolution> {
    let input = input.trim();
    anyhow::ensure!(!input.is_empty(), "no workspace path or URL given");

    let root = if let Some(dest) = workspace_clone_dest(input, cfg) {
        // URL inputs are cloned OFF the loop before this runs (NewWorkspace
        // handler → spawn_blocking → workspace_clone_rx drain); by the time
        // this is called the clone must already be on disk.
        anyhow::ensure!(
            dest.exists(),
            "clone of {input} not materialized at {} (the clone runs off-loop first)",
            dest.display()
        );
        std::fs::canonicalize(&dest).unwrap_or(dest)
    } else {
        let expanded = thegn_core::util::expand_tilde(input);
        let path = std::path::PathBuf::from(expanded);
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        anyhow::ensure!(path.is_dir(), "path does not exist: {}", path.display());
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        thegn_core::repo::main_worktree(&canonical).unwrap_or(canonical)
    };

    let root_s = root.to_string_lossy().into_owned();
    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".into());
    let kind = if thegn_core::repo::main_worktree(&root).is_some() {
        "repo"
    } else {
        return Ok(WorkspaceResolution::NotARepo(root));
    };
    db.put_workspace(&root_s, &name, kind)?;
    db.touch_repo(&root_s, &name)?;
    session.switch_to_workspace(&root_s, db)?;
    Ok(WorkspaceResolution::Repo(root))
}

/// Create + switch to a workspace for a LOCAL `input` (a path, or the dest of
/// a finished URL clone): park the outgoing workspace in the pool, resolve
/// and persist the new one, and sync chrome state. Never clones — URL inputs
/// go through the off-loop clone first (see `WorkspaceCloneOutcome`).
/// Returns whether a relayout is needed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn complete_workspace_create(
    input: &str,
    session: &mut crate::session::Session,
    panes: &mut Panes,
    workspace_pool: &mut WorkspacePool,
    active_menu: &mut Option<MenuOverlay>,
    focus: &mut crate::focus::FocusState,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    drawer: &mut Option<u32>,
    drawer_pool: &mut DrawerPool,
    drawer_home: &mut Option<std::path::PathBuf>,
    cfg: &thegn_core::config::Config,
    center: Rect,
) -> bool {
    persist_session_layout(session, panes);
    let prev_id = session.id.clone();
    let snapshot = ResidentWorkspace {
        worktrees: session.worktrees.clone(),
        active: session.active,
    };
    match thegn_core::db::Db::open()
        .context("open thegn db")
        .and_then(|db| create_workspace_from_input_with_config(input, session, &db, cfg))
    {
        Ok(WorkspaceResolution::Repo(path)) => {
            workspace_pool.stash(prev_id, snapshot, panes);
            remap_cold_workspace_ids(session, panes);
            focus.zone = crate::focus::Zone::Center;
            refresh_tab_model(model, session, sb);
            sync_drawer_persistence(
                session,
                panes,
                drawer,
                drawer_pool,
                drawer_home,
                cfg,
                center,
            );
            model.status = format!("workspace created: {}", path.display());
            true
        }
        Ok(WorkspaceResolution::NotARepo(path)) => {
            *active_menu = Some(menu::init_git_menu(path.to_string_lossy().into_owned()));
            false
        }
        Err(e) => {
            model.status = format!("workspace create failed: {e}");
            false
        }
    }
}

/// Apply one `CloneEvent` from the off-loop clone to the picker: stream a
/// progress line into its cloning panel, or on `Done` complete+switch (closing
/// the picker) / park it in a visible error. Ignores results for a clone the
/// user has since cancelled or moved past. Returns whether the loop should
/// relayout + re-hydrate (a workspace was created).
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_clone_event(
    ev: CloneEvent,
    picker: &mut Option<crate::workspace_picker::WorkspacePicker>,
    session: &mut crate::session::Session,
    panes: &mut Panes,
    workspace_pool: &mut WorkspacePool,
    active_menu: &mut Option<MenuOverlay>,
    focus: &mut crate::focus::FocusState,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    drawer: &mut Option<u32>,
    drawer_pool: &mut DrawerPool,
    drawer_home: &mut Option<std::path::PathBuf>,
    cfg: &thegn_core::config::Config,
    center: Rect,
) -> bool {
    match ev {
        CloneEvent::Progress { url, line } => {
            if let Some(p) = picker.as_mut()
                && p.cloning_url() == Some(url.as_str())
            {
                p.set_clone_progress(line);
            }
            false
        }
        CloneEvent::Done(o) => {
            // Only honor a result the picker is still awaiting — a cancel (or a
            // move to another clone) abandons it rather than yanking the user
            // into a workspace they dismissed.
            let awaited = picker
                .as_ref()
                .is_some_and(|p| p.cloning_url() == Some(o.url.as_str()));
            if !awaited {
                return false;
            }
            match o.result {
                Ok(()) => {
                    *picker = None;
                    let dest = o.dest.to_string_lossy().into_owned();
                    complete_workspace_create(
                        &dest,
                        session,
                        panes,
                        workspace_pool,
                        active_menu,
                        focus,
                        model,
                        sb,
                        drawer,
                        drawer_pool,
                        drawer_home,
                        cfg,
                        center,
                    )
                }
                Err(e) => {
                    if let Some(p) = picker.as_mut() {
                        p.set_error(format!("clone failed: {e}"));
                    }
                    false
                }
            }
        }
    }
}

/// Map a [`crate::workspace_picker::PickerOutcome`] onto the shared workspace
/// flows: cancel, open a browsed repo, or classify+dispatch a manual input
/// (off-loop clone / local create+switch / new-project confirm / inline error).
/// Owns the picker's lifecycle so it stays on screen through a clone or a bad
/// input instead of vanishing. Returns whether the loop should relayout +
/// re-hydrate (a workspace was created).
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_picker_outcome(
    outcome: crate::workspace_picker::PickerOutcome,
    picker: &mut Option<crate::workspace_picker::WorkspacePicker>,
    clone_tx: &tokio::sync::mpsc::UnboundedSender<CloneEvent>,
    waker: &termwiz::terminal::TerminalWaker,
    session: &mut crate::session::Session,
    panes: &mut Panes,
    workspace_pool: &mut WorkspacePool,
    active_menu: &mut Option<MenuOverlay>,
    focus: &mut crate::focus::FocusState,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    drawer: &mut Option<u32>,
    drawer_pool: &mut DrawerPool,
    drawer_home: &mut Option<std::path::PathBuf>,
    cfg: &thegn_core::config::Config,
    center: Rect,
) -> bool {
    use crate::workspace_picker::PickerOutcome;
    let create = |input: &str,
                  picker: &mut Option<crate::workspace_picker::WorkspacePicker>,
                  session: &mut crate::session::Session,
                  panes: &mut Panes,
                  workspace_pool: &mut WorkspacePool,
                  active_menu: &mut Option<MenuOverlay>,
                  focus: &mut crate::focus::FocusState,
                  model: &mut FrameModel,
                  sb: &mut SidebarState,
                  drawer: &mut Option<u32>,
                  drawer_pool: &mut DrawerPool,
                  drawer_home: &mut Option<std::path::PathBuf>| {
        *picker = None;
        complete_workspace_create(
            input,
            session,
            panes,
            workspace_pool,
            active_menu,
            focus,
            model,
            sb,
            drawer,
            drawer_pool,
            drawer_home,
            cfg,
            center,
        )
    };
    match outcome {
        PickerOutcome::Pending => false,
        PickerOutcome::Cancel => {
            *picker = None;
            model.status = "workspace creation cancelled".into();
            false
        }
        PickerOutcome::OpenRepo(path) => create(
            &path,
            picker,
            session,
            panes,
            workspace_pool,
            active_menu,
            focus,
            model,
            sb,
            drawer,
            drawer_pool,
            drawer_home,
        ),
        PickerOutcome::Manual(input) => match plan_new_workspace_input(&input, cfg) {
            SubmitPlan::Clone { url, dest } => {
                // Keep the picker on screen in its cloning phase — apply_clone_event
                // streams progress and closes it (or shows an error) on done.
                model.status = format!("Cloning {url}…");
                if let Some(p) = picker.as_mut() {
                    p.begin_clone(url.clone());
                }
                spawn_workspace_clone(url, dest, clone_tx.clone(), waker.clone());
                false
            }
            SubmitPlan::Local(input) => create(
                &input,
                picker,
                session,
                panes,
                workspace_pool,
                active_menu,
                focus,
                model,
                sb,
                drawer,
                drawer_pool,
                drawer_home,
            ),
            SubmitPlan::CreateNew { leaf } => {
                *picker = None;
                *active_menu = Some(menu::create_project_menu(
                    leaf.to_string_lossy().into_owned(),
                ));
                false
            }
            SubmitPlan::Invalid(msg) => {
                // Stay open with the error inline so the input can be fixed.
                if let Some(p) = picker.as_mut() {
                    p.set_error(msg);
                }
                false
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tg-wscreate-{}-{}", std::process::id(), name))
    }

    #[test]
    fn progress_lines_split_on_cr_and_lf() {
        // git rewrites the transfer counter in place with '\r', then ends the
        // final line with '\n'. Each rewrite is a distinct segment; blanks and
        // whitespace are dropped/trimmed.
        let data = b"Cloning into 'x'...\nReceiving objects:  10% (1/10)\r\
Receiving objects: 100% (10/10)\rdone.\n";
        let mut lines = Vec::new();
        stream_progress_lines(std::io::Cursor::new(&data[..]), |l| lines.push(l));
        assert_eq!(
            lines,
            vec![
                "Cloning into 'x'...",
                "Receiving objects:  10% (1/10)",
                "Receiving objects: 100% (10/10)",
                "done.",
            ]
        );
    }

    #[test]
    fn progress_lines_flush_trailing_without_terminator() {
        let mut lines = Vec::new();
        stream_progress_lines(std::io::Cursor::new(b"no newline".as_slice()), |l| {
            lines.push(l)
        });
        assert_eq!(lines, vec!["no newline"]);
    }

    #[test]
    fn plan_classifies_git_urls_as_clone() {
        let cfg = thegn_core::config::Config::default();
        match plan_new_workspace_input("https://github.com/acme/widget.git", &cfg) {
            SubmitPlan::Clone { url, dest } => {
                assert_eq!(url, "https://github.com/acme/widget.git");
                assert_eq!(dest.file_name().unwrap().to_string_lossy(), "widget");
            }
            other => panic!("expected Clone, got {other:?}"),
        }
        // ssh remotes too
        assert!(matches!(
            plan_new_workspace_input("git@github.com:acme/widget.git", &cfg),
            SubmitPlan::Clone { .. }
        ));
    }

    #[test]
    fn plan_classifies_existing_dir_as_local() {
        let cfg = thegn_core::config::Config::default();
        let dir = tmp("local");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.to_string_lossy().into_owned();
        assert_eq!(
            plan_new_workspace_input(&input, &cfg),
            SubmitPlan::Local(input.clone())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_classifies_missing_leaf_with_existing_parent_as_create_new() {
        let cfg = thegn_core::config::Config::default();
        let parent = tmp("createnew");
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        let leaf = parent.join("fresh-project");
        match plan_new_workspace_input(&leaf.to_string_lossy(), &cfg) {
            SubmitPlan::CreateNew { leaf: got } => assert_eq!(got, leaf),
            other => panic!("expected CreateNew, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn plan_rejects_missing_parent_and_empty_input() {
        let cfg = thegn_core::config::Config::default();
        // Two missing levels: new projects may only nest one level below the
        // present tree.
        let leaf = tmp("no-such-parent").join("deeper/leaf");
        assert!(matches!(
            plan_new_workspace_input(&leaf.to_string_lossy(), &cfg),
            SubmitPlan::Invalid(_)
        ));
        assert!(matches!(
            plan_new_workspace_input("   ", &cfg),
            SubmitPlan::Invalid(_)
        ));
    }

    #[test]
    fn init_new_project_creates_dir_and_git_repo() {
        let parent = tmp("initproj");
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        let leaf = parent.join("proj");
        init_new_project(&leaf).unwrap();
        assert!(leaf.join(".git").is_dir(), "git init produced a .git dir");
        // A leaf whose parent is missing is refused (and nothing is created).
        let bad = parent.join("missing/two-deep");
        assert!(init_new_project(&bad).is_err());
        assert!(!bad.exists());
        let _ = std::fs::remove_dir_all(&parent);
    }
}
