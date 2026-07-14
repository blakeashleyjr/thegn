//! Tracker (Issues/Mine) section glue: the issue↔worktree link toggle,
//! branch-from-issue, the Issues action keys (`o/r/a/D`), and the
//! `pending_issue_link` resolution that runs when a branched-from-issue
//! worktree finishes creating. Extracted from the ratchet-pinned `run.rs`.
//!
//! Threading contract: every tracker mutation (issue router calls, worktree
//! creation, DB writes) runs on `spawn_blocking`; the functions here execute
//! ON the loop and only clone what the off-thread closure needs.

use std::path::Path;

use termwiz::terminal::TerminalWaker;
use thegn_core::store::{NotificationStore, WorkspaceStore, WorktreeAuxStore};
use tokio::sync::mpsc as tokio_mpsc;

use crate::actions::open_url_detached;
use crate::chrome::FrameModel;
use crate::hydrate::active_tab_path;
use crate::naming::issue_branch_tail;

/// The loop locals the tracker keys need, borrowed for one keypress.
pub(crate) struct TrackerCtx<'a> {
    pub model: &'a mut FrameModel,
    pub panel_ui: &'a mut crate::panel::PanelUi,
    pub session: &'a crate::session::Session,
    /// The keymap's (workspace-resolved) config — worktree-create presets and
    /// agent dispatch read this, matching the wizard paths.
    pub cfg: &'a thegn_core::config::Config,
    /// The live reloadable config — issue-router calls read this.
    pub live_cfg: &'a thegn_core::config::Config,
    pub waker: &'a TerminalWaker,
    // Worktree-creation plumbing (branch-from-issue / agent dispatch).
    pub create_gen: &'a mut u64,
    pub create_tx: &'a tokio_mpsc::UnboundedSender<crate::wizard::CreateEvent>,
    pub inflight: &'a mut crate::handlers::creating::InFlight,
    pub wizard_ui: &'a mut Option<crate::wizard::NewWorktreeWizard>,
    pub pending_issue_link: &'a mut Option<(u64, String)>,
    // Model re-hydration after a link toggle.
    pub model_tx: &'a tokio_mpsc::UnboundedSender<(u64, FrameModel)>,
    pub hydration_gen: &'a mut u64,
}

/// Enter on the Issues section: toggle the worktree↔issue link for the
/// cursor row, then re-hydrate so the badge updates.
pub(crate) fn toggle_link(ctx: &mut TrackerCtx) {
    let wt = active_tab_path(ctx.session);
    let wt_str = wt.to_string_lossy().to_string();
    if let Some(issue) = ctx
        .model
        .panel
        .tracker_issues
        .get(ctx.panel_ui.issues_cursor)
    {
        let id = issue.id.clone();
        let already_linked = ctx.model.panel.tracker_links.contains(&id);
        if let Ok(db) = thegn_core::db::Db::open() {
            if already_linked {
                let _ = db.unlink_issue(&wt_str, &id);
                ctx.model.status = format!("Unlinked {id}");
            } else {
                let _ = db.link_issue(&wt_str, &id);
                ctx.model.status = format!("Linked {id}");
            }
        }
        // Refresh model so the badge updates.
        *ctx.hydration_gen += 1;
        crate::hydrate::spawn_model_hydration(
            ctx.model_tx.clone(),
            *ctx.hydration_gen,
            ctx.session.clone(),
            Some(ctx.waker.clone()),
            crate::hydrate::HydrateHints {
                open: ctx.panel_ui.open,
                expanded: ctx.panel_ui.width.is_expanded(),
                ..Default::default()
            },
        );
    }
}

/// `b` on Mine or Issues: branch-a-worktree-from-this-issue — the keystone
/// that turns the dashboard into a launchpad. Reuses the headless
/// worktree-create preset and links the new worktree to the issue on
/// completion (via `pending_issue_link` → [`on_worktree_created`]).
pub(crate) fn branch_from_issue(ctx: &mut TrackerCtx) {
    let fields: Option<(String, String, String, Option<String>)> =
        if ctx.panel_ui.open == crate::panel::Section::Mine {
            let rows = crate::panel::sections::my_work::ordered_rows(&ctx.model.panel);
            rows.get(ctx.panel_ui.cursor).and_then(|r| {
                r.issue_id
                    .clone()
                    .map(|id| (id, r.number.clone(), r.title.clone(), r.branch_hint.clone()))
            })
        } else {
            ctx.model
                .panel
                .tracker_issues
                .get(ctx.panel_ui.issues_cursor)
                .map(|i| {
                    (
                        i.id.clone(),
                        i.number.clone(),
                        i.title.clone(),
                        i.branch_hint.clone(),
                    )
                })
        };
    if let Some((issue_id, number, title, hint)) = fields {
        let root = ctx
            .session
            .active_group()
            .map(|g| g.path.clone())
            .filter(|p| !p.is_empty())
            .and_then(|p| thegn_core::repo::main_worktree(Path::new(&p)))
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|c| thegn_core::repo::main_worktree(&c))
            });
        if let Some(root) = root {
            let tail = issue_branch_tail(&number, &title, hint.as_deref());
            crate::run::begin_worktree_preset(
                root,
                crate::keymap::NameSpec::Fixed(tail),
                None,
                None,
                None,
                ctx.cfg,
                ctx.create_gen,
                ctx.create_tx,
                ctx.waker,
                ctx.inflight,
                ctx.wizard_ui,
                ctx.model,
            );
            *ctx.pending_issue_link = Some((*ctx.create_gen, issue_id));
            ctx.model.status = format!("Branching worktree for {number}…");
        } else {
            ctx.model.status = "branch-from-issue: not inside a git repository".into();
        }
    }
}

/// The Issues section's plain action keys (`o/r/a/D`). Returns whether the
/// key was consumed.
pub(crate) fn issues_key(key: char, ctx: &mut TrackerCtx) -> bool {
    match key {
        'o' => {
            if let Some(issue) = ctx
                .model
                .panel
                .tracker_issues
                .get(ctx.panel_ui.issues_cursor)
            {
                open_url_detached(&issue.url.clone());
                ctx.model.status = format!("Opened {} in browser", issue.number);
            }
            true
        }
        'r' => {
            crate::hydrate_tracker::spawn_issue_cache_refresh(
                active_tab_path(ctx.session),
                ctx.live_cfg.issues.clone(),
                Some(ctx.waker.clone()),
            );
            ctx.model.status = "Refreshing issues…".into();
            true
        }
        'a' => {
            // Self-assign the cursor issue.
            if let Some(issue) = ctx
                .model
                .panel
                .tracker_issues
                .get(ctx.panel_ui.issues_cursor)
                .cloned()
            {
                let cfg = ctx.live_cfg.issues.clone();
                let waker2 = ctx.waker.clone();
                let cwd2 = active_tab_path(ctx.session);
                tokio::task::spawn_blocking(move || {
                    use thegn_core::issue::IssuePatch;
                    use thegn_svc::issue::IssueRouter;
                    let router = IssueRouter::from_config(&cfg);
                    if !router.is_configured() {
                        return;
                    }
                    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        return;
                    };
                    let patch = IssuePatch {
                        assignee_me: Some(true),
                        ..Default::default()
                    };
                    let _ = rt.block_on(router.update_issue(&issue.id, &patch));
                    crate::hydrate_tracker::spawn_issue_cache_refresh(cwd2, cfg, Some(waker2));
                });
                ctx.model.status = format!("Assigning {} to you…", issue.number);
            }
            true
        }
        'D' => {
            dispatch_agent(ctx);
            true
        }
        _ => false,
    }
}

/// `D` on Issues: dispatch a Claude Code agent to a new worktree for the
/// selected issue. Reuses the wizard's `Done` path.
fn dispatch_agent(ctx: &mut TrackerCtx) {
    if let Some(issue) = ctx
        .model
        .panel
        .tracker_issues
        .get(ctx.panel_ui.issues_cursor)
        .cloned()
    {
        // Fresh generation for this headless dispatch: it sends `Done`
        // directly (no worker channel / progress entry) and leaves concurrent
        // creations undisturbed.
        *ctx.create_gen += 1;
        let dispatch_gen = *ctx.create_gen;

        let cfg2 = ctx.cfg.clone();
        let tx2 = ctx.create_tx.clone();
        let wk2 = ctx.waker.clone();
        let src_path = ctx
            .session
            .active_group()
            .map(|g| g.path.clone())
            .unwrap_or_default();
        let issue_id = issue.id.clone();
        let issue_number = issue.number.clone();
        let issue_title = issue.title.clone();
        let issue_body = issue.body.clone().unwrap_or_default();
        let issue_url = issue.url.clone();

        tokio::task::spawn_blocking(move || {
            use thegn_core::{repo, worktree as wt};

            // Find the repo root from the active worktree path.
            let root_opt = (!src_path.is_empty())
                .then(|| repo::main_worktree(std::path::Path::new(&src_path)))
                .flatten()
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|c| repo::main_worktree(&c))
                });
            let Some(root) = root_opt else {
                return;
            };

            let base = wt::resolve_base(&root, &cfg2);
            let taken = wt::BranchSet::load(&root);
            let raw_branch = issue
                .branch_hint
                .as_deref()
                .map(str::to_owned)
                .unwrap_or_else(|| thegn_core::util::slugify(&issue_number));
            let branch = wt::dedupe(&raw_branch, &taken);
            let path = wt::worktree_path(&root, &branch, &cfg2);

            if let Err(e) = wt::add_checked(&root, &branch, &base, &path, &cfg2) {
                thegn_core::msg::warn(&format!("agent dispatch: {e}"));
                return;
            }

            let wt_str = path.to_string_lossy().into_owned();
            let Ok(mut spec) =
                crate::direnv_warm::launch_spec_synced(&cfg2, &wt_str, Some(&branch), "claude")
            else {
                return;
            };

            // Inject issue context for the agent.
            spec.env.push(("THEGN_ISSUE_ID".into(), issue_id.clone()));
            spec.env
                .push(("THEGN_ISSUE_TITLE".into(), issue_title.clone()));
            spec.env.push(("THEGN_ISSUE_BODY".into(), issue_body));
            spec.env.push(("THEGN_ISSUE_URL".into(), issue_url));

            let slug = repo::repo_slug(&root);
            let tab = repo::branch_tab(&slug, &branch);

            // Register the dispatch in the DB.
            if let Ok(db) = thegn_core::db::Db::open() {
                let root_s = root.to_string_lossy();
                let _ = db.put_worktree(&tab, &root_s, &wt_str, &branch, None, None);
                let _ = db.put_agent_dispatch(&issue_id, &wt_str, "claude");
                let _ = db.link_issue(&wt_str, &issue_id);
            }
            let payload = crate::wizard::CreatedWorktree {
                tab: tab.clone(),
                branch: branch.clone(),
                path: wt_str,
                agent: "claude".into(),
                spec,
            };
            let _ = tx2.send(crate::wizard::CreateEvent::Done {
                generation: dispatch_gen,
                payload: Box::new(payload),
            });
            let _ = wk2.wake();
        });
        ctx.model.status = format!("Dispatching agent to {}…", issue.number);
    }
}

/// Branch-from-issue completion: link the freshly created worktree to the
/// issue it was branched from, so its tab carries the issue badge. Called
/// from the wizard `CreateEvent::Done` arm; leaves a non-matching pending
/// entry in place for its own `Done` event.
pub(crate) fn on_worktree_created(
    pending_issue_link: &mut Option<(u64, String)>,
    generation: u64,
    path: &str,
) {
    if let Some((g, issue_id)) = pending_issue_link.take() {
        if g == generation {
            if let Ok(db) = thegn_core::db::Db::open() {
                let _ = db.link_issue(path, &issue_id);
            }
        } else {
            // Not ours — put it back for the matching Done event.
            *pending_issue_link = Some((g, issue_id));
        }
    }
}
