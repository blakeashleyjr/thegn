//! `thegn dispatch <action>` — the durable agent-dispatch roster (THE-57).
//!
//! The roster (`agent_dispatches`) is the ledger a supervisor agent resumes
//! from after a crash: which issue is being worked in which worktree by which
//! agent, and each row's status. These verbs read and advance it directly
//! against the local SQLite cache (no daemon needed), the same way `thegn wt
//! list` / `thegn merge` read their tables. The status column is a **closed,
//! parseable set** ([`thegn_core::issue::AgentDispatchStatus`]); `set-status`
//! writes it through the typed value, and `list` coerces any legacy/unknown
//! stored string to `unknown` rather than failing the read.
//!
//! `put` appends a row, including the v56 pipeline columns
//! (`stage`/`parent`/`session`/`artifact`). Those are **structure, not
//! judgment**: thegn stores, groups and renders them, and no code path here
//! advances a stage — that is the supervising agent's call.
//!
//! The run-completion contract (THE-76) adds the mechanism around that
//! judgment: `verify` asks git whether a row's handoff artifact is real, the
//! `set-status done` gate refuses an unverifiable completion unless
//! `--force` is given, and `wait` blocks on an active row's daemon session —
//! all reads of local state, never stage transitions.

use anyhow::Result;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::issue::{AgentDispatch, AgentDispatchStatus, NewDispatch};
use thegn_core::outln;
use thegn_core::pipeline_run::{self, WaitTarget};
use thegn_core::store::NotificationStore;
use thegn_core::util::{git_ok, git_out};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List the dispatch roster (newest first).
    List {
        /// Only rows that occupy a slot (queued / spawning / running /
        /// waiting_human / pr_open) — what a supervisor resumes from.
        #[arg(long)]
        active: bool,
        /// Emit JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Append a row to the roster: this agent is working this issue in this
    /// worktree. The pipeline columns (`--stage`/`--parent`/`--session`/
    /// `--artifact`) are the supervising agent's own bookkeeping — thegn stores
    /// and renders them and never advances a stage itself.
    Put {
        /// Tracker issue id (`"<provider>:<key>"`, e.g. `linear:THE-57`).
        issue_id: String,
        /// The worktree the agent works in (path).
        worktree_path: String,
        /// An `[[agents]]`/`[[tools]]` name (or a provider id).
        agent_name: String,
        /// The `[[pipeline.stages]]` step this row is (e.g. `architect`).
        #[arg(long)]
        stage: Option<String>,
        /// The roster row this one was chunked out of (see `dispatch list`).
        #[arg(long)]
        parent: Option<i64>,
        /// The daemon session running it (see `thegn session open`), so a pane
        /// exit stamps THIS row and not a sibling stage's.
        #[arg(long)]
        session: Option<String>,
        /// Path to the handoff artifact committed in the worktree.
        #[arg(long)]
        artifact: Option<String>,
        /// Emit the created row as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Advance one dispatch's status. Marking a row `done` is gated: when the
    /// row carries an artifact, it must exist in the worktree and be tracked by
    /// git (a session exiting is not a handoff — see `dispatch verify`).
    /// `failed`/`abandoned`/`merged` are never gated: recording a bad outcome
    /// must always be possible.
    SetStatus {
        /// The dispatch row id (see `dispatch list`).
        id: i64,
        /// A member of the closed set: queued | spawning | running |
        /// waiting_human | pr_open | merged | abandoned | done | failed.
        status: String,
        /// Record `done` even when the artifact gate refuses it. A forced
        /// completion is printed as such, in both output modes.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Report whether a row's handoff artifact is real — present in the row's
    /// worktree AND tracked by git (an uncommitted artifact is not a handoff).
    /// Exit 0 when it is; exit 2 (retryable) with the reasons when not. Reads
    /// only — the verdict is the supervisor's judgment.
    Verify {
        /// The dispatch row id (see `dispatch list`).
        id: i64,
        /// Emit the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Block until an active roster row's daemon session exits (the supervisor
    /// wake primitive). A session that already exited answers immediately from
    /// the daemon's tombstone; one reaped past the tombstone TTL reports as
    /// gone. Exit 0 on the wake, 2 (retryable) on timeout.
    Wait {
        /// The roster row to wait on (see `dispatch list`).
        #[arg(long)]
        row: Option<i64>,
        /// Wait on every spawning/running row that carries a session — the
        /// default when neither flag is given; the first exit wakes.
        #[arg(long, conflicts_with = "row")]
        any: bool,
        /// Milliseconds before giving up (exit 2). Omit to wait forever.
        #[arg(long)]
        timeout: Option<i64>,
        /// Emit the wake (or the timeout) as JSON.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List { active, json } => list(active, json),
        Action::Put {
            issue_id,
            worktree_path,
            agent_name,
            stage,
            parent,
            session,
            artifact,
            json,
        } => {
            let db = Db::open()?;
            let row = put(
                &db,
                NewDispatch {
                    issue_id: &issue_id,
                    worktree_path: &worktree_path,
                    agent_name: &agent_name,
                    stage: stage.as_deref(),
                    parent_id: parent,
                    session_id: session.as_deref(),
                    artifact_path: artifact.as_deref(),
                },
            )?;
            if json {
                return super::emit_json(&row);
            }
            outln!("dispatch {} → {}", row.id, row.status.as_str());
            Ok(())
        }
        Action::SetStatus {
            id,
            status,
            force,
            json,
        } => set_status(id, &status, force, json),
        Action::Verify { id, json } => verify(id, json),
        Action::Wait {
            row, timeout, json, ..
        } => wait(cfg, row, timeout, json),
    }
}

/// Insert one roster row and read it back. Split from the clap arm (and taking
/// an explicit `&Db`) so the insert is testable against an isolated database —
/// `Db::open()` would hit the developer's live state.
fn put(db: &Db, new: NewDispatch<'_>) -> Result<AgentDispatch> {
    // A parent must exist. Nothing enforces it in SQL (the roster is a
    // cache-side ledger, not a foreign-key graph), so a typo would otherwise
    // produce a chunk row silently orphaned from the board.
    if let Some(parent) = new.parent_id
        && db.get_dispatch(parent)?.is_none()
    {
        anyhow::bail!("no dispatch with id {parent} to parent this row on");
    }
    let id = db.put_agent_dispatch(new)?;
    db.get_dispatch(id)?
        .ok_or_else(|| anyhow::anyhow!("dispatch {id} vanished after insert"))
}

fn list(active: bool, json: bool) -> Result<()> {
    let db = Db::open()?;
    let mut rows = db.list_dispatches()?;
    if active {
        rows.retain(|d| d.status.is_active());
    }
    if json {
        return super::emit_json(&rows);
    }
    if rows.is_empty() {
        outln!("no dispatches");
        return Ok(());
    }
    for d in &rows {
        // The pipeline columns print as `-` when absent rather than collapsing
        // the row's shape, so the table stays column-aligned for a roster that
        // mixes pipeline and plain dispatches.
        outln!(
            "{}  {}  {}  {}  {}  {}  {}",
            d.id,
            d.status.as_str(),
            d.stage.as_deref().unwrap_or("-"),
            d.parent_id
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
            d.issue_id,
            d.agent_name,
            d.worktree_path,
        );
    }
    Ok(())
}

fn set_status(id: i64, status: &str, force: bool, json: bool) -> Result<()> {
    // Reject an unparseable status up front — writing `Unknown` back would
    // corrupt the roster the fix exists to protect. `Unknown` is a read-only
    // coercion, never a target a caller may set.
    let parsed = AgentDispatchStatus::parse(status);
    if parsed == AgentDispatchStatus::Unknown {
        anyhow::bail!(
            "unknown dispatch status {status:?} (expected one of: queued, spawning, running, \
             waiting_human, pr_open, merged, abandoned, done, failed)"
        );
    }
    let db = Db::open()?;
    let row = db
        .get_dispatch(id)?
        .ok_or_else(|| anyhow::anyhow!("no dispatch with id {id}"))?;
    // Only `done` is gated, and only for rows that carry an artifact — see
    // `done_gate`. Every other status stays unconditional: a supervisor must
    // always be able to record a bad outcome. This comment is the rule a
    // future edit will erode; keep it.
    if parsed == AgentDispatchStatus::Done && !force {
        done_gate(&row)?;
    }
    db.update_dispatch_status(id, parsed)?;
    if json {
        let mut v = serde_json::json!({ "id": id, "status": parsed.as_str() });
        if force {
            // A forced completion is never invisible.
            v["forced"] = serde_json::json!(true);
        }
        return super::emit_json(&v);
    }
    if force {
        outln!("dispatch {id} → {} (forced)", parsed.as_str());
    } else {
        outln!("dispatch {id} → {}", parsed.as_str());
    }
    Ok(())
}

/// The `set-status done` gate (THE-76): a row that carries an artifact may
/// only be recorded done when that artifact is real — present in the row's
/// worktree **and** tracked by git. A session exiting is not a handoff; git is
/// the source of truth, so an uncommitted artifact is not finished work.
/// Split from `set_status` so the decision is testable without a live DB.
fn done_gate(row: &AgentDispatch) -> Result<()> {
    let report = pipeline_run::verify_report(&verify_facts(row));
    if report.ok {
        return Ok(());
    }
    anyhow::bail!(
        "dispatch {} is not verifiably finished:\n  - {}\nrun `thegn dispatch verify {}` for detail, or `--force` to record it anyway",
        row.id,
        report.reasons.join("\n  - "),
        row.id
    );
}

/// Gather the filesystem/git facts for one roster row — the single
/// implementation shared by `dispatch verify` and the `done` gate. A row with
/// no artifact is never gated, so no git subprocess is spent on it; the facts
/// read as all-false and `verify_report`'s first rule turns that into `ok`.
fn verify_facts(row: &AgentDispatch) -> pipeline_run::VerifyFacts {
    let Some(artifact) = row
        .artifact_path
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
    else {
        return pipeline_run::VerifyFacts {
            artifact: None,
            exists: false,
            tracked: false,
            dirty: false,
        };
    };
    let wt = std::path::Path::new(&row.worktree_path);
    // `symlink_metadata`, not `is_file`/`exists`: the final component must be
    // a regular file, not a symlink to one. A committed symlink at the artifact
    // path is `tracked` (the link itself is in the index) but redirects the
    // Lead's artifact read at whatever the worker pointed it at — the gate
    // must not bless that as a handoff. (It reads as `exists=false`, the same
    // refusal as a missing file; `--force` remains the deliberate override.
    // A symlink on an *intermediate* component is held by `tracked`: `git
    // ls-files` matches index paths, and paths under a linked directory are
    // not in the index.)
    let exists = wt
        .join(&artifact)
        .symlink_metadata()
        .is_ok_and(|m| m.is_file());
    let tracked = git_ok(
        wt,
        &["ls-files", "--error-unmatch", "--", artifact.as_str()],
    );
    // `git_out` returns `None` for empty output, so `Some` ⇔ the worktree
    // has uncommitted changes.
    let dirty = git_out(wt, &["status", "--porcelain"]).is_some();
    pipeline_run::VerifyFacts {
        artifact: Some(artifact),
        exists,
        tracked,
        dirty,
    }
}

fn verify(id: i64, json: bool) -> Result<()> {
    let db = Db::open()?;
    let row = db
        .get_dispatch(id)?
        .ok_or_else(|| anyhow::anyhow!("no dispatch with id {id}"))?;
    let report = pipeline_run::verify_report(&verify_facts(&row));
    if json {
        // One flat document — the report plus the row id — so a supervisor can
        // grep `"ok":false` / `"id":12` from a single line.
        let mut v = serde_json::to_value(&report)?;
        v["id"] = serde_json::json!(id);
        super::emit_json(&v)?;
    } else {
        let a = report.artifact.as_deref().unwrap_or("-");
        outln!(
            "dispatch {id}  artifact {a}  exists={} tracked={} dirty={}  ok={}",
            yes_no(report.exists),
            yes_no(report.tracked),
            yes_no(report.dirty),
            yes_no(report.ok)
        );
        for r in &report.reasons {
            outln!("  - {r}");
        }
    }
    // Not-yet is retryable, not an error — the same convention `session wait`
    // uses for a timeout: a supervisor may simply poll again later.
    if !report.ok {
        std::process::exit(crate::cmd::EXIT_RETRYABLE);
    }
    Ok(())
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// `dispatch wait` — the one dispatch verb that needs the daemon. The roster
/// read and the candidate selection stay synchronous and BEFORE the connect:
/// a selection error (unknown row, nothing active) is answerable from the
/// local roster alone and must not require a running daemon.
fn wait(cfg: &Config, row: Option<i64>, timeout: Option<i64>, json: bool) -> Result<()> {
    let db = Db::open()?;
    let rows = db.list_dispatches()?;
    let targets = pipeline_run::wait_candidates(&rows, row).map_err(|e| anyhow::anyhow!("{e}"))?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(wait_wake(cfg, targets, timeout, json))
}

async fn wait_wake(
    cfg: &Config,
    targets: Vec<WaitTarget>,
    timeout: Option<i64>,
    json: bool,
) -> Result<()> {
    let client = match crate::cmd::session::connect(cfg).await {
        Ok(c) => c,
        Err(e) => {
            if json {
                outln!("{}", serde_json::json!({ "error": "no_daemon" }));
            }
            return Err(e);
        }
    };
    // One wait per target; the client is shared behind an Arc.
    let client = std::sync::Arc::new(client);
    let mut set = tokio::task::JoinSet::new();
    for t in targets {
        let client = client.clone();
        set.spawn(async move {
            let out = client
                .wait(
                    &t.session_id,
                    serde_json::json!({ "kind": "exited" }),
                    timeout,
                )
                .await;
            (t, out)
        });
    }
    // The first wake wins and dropping the set cancels the remaining waits
    // (JoinSet's cancel-on-drop). A `matched: false` is a timeout answer, not a
    // wake: keep listening for the others until every target has answered, so
    // one target timing out a beat before another exits still wakes us.
    while let Some(joined) = set.join_next().await {
        let (t, outcome) = joined.map_err(|e| anyhow::anyhow!("wait task failed: {e}"))?;
        // A daemon error for one target (its session aged past the tombstone
        // TTL and was reaped) is a wake with `"gone": true` — never a failure
        // of the whole call: one reaped session must not make `--any` unusable.
        let (matched, exit_code, gone) = match outcome {
            Ok(v) => (
                v.get("matched").and_then(|m| m.as_bool()).unwrap_or(false),
                v.get("exit_code").and_then(|c| c.as_i64()),
                false,
            ),
            Err(_) => (true, None, true),
        };
        if !matched {
            continue;
        }
        if json {
            let mut v = serde_json::json!({
                "row": t.id,
                "session": t.session_id,
                "stage": t.stage,
                "issue": t.issue_id,
                "exit_code": exit_code,
                "matched": true,
            });
            if gone {
                v["gone"] = serde_json::json!(true);
            }
            return super::emit_json(&v);
        }
        let stage = t.stage.as_deref().unwrap_or("-");
        match (gone, exit_code) {
            (true, _) => outln!("dispatch {} ({}) session is gone", t.id, stage),
            (false, Some(code)) => outln!("dispatch {} ({}) exited {code}", t.id, stage),
            // Matched but unreapable — the same `?` `session list` prints.
            (false, None) => outln!("dispatch {} ({}) exited ?", t.id, stage),
        }
        return Ok(());
    }
    // Every target answered `matched: false` — the daemon-side timeout fired.
    if json {
        super::emit_json(&serde_json::json!({ "matched": false }))?;
    } else {
        outln!("timeout waiting on the dispatch wake");
    }
    std::process::exit(crate::cmd::EXIT_RETRYABLE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::store::NotificationStore;

    fn db(name: &str) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join(format!("{name}.db"))).unwrap();
        (dir, db)
    }

    #[test]
    fn put_records_the_pipeline_columns_and_reads_the_row_back() {
        let (_d, db) = db("put-cols");
        let lead = put(&db, NewDispatch::new("linear:A-1", "/wt/a", "claude")).unwrap();
        assert_eq!(lead.status, AgentDispatchStatus::Queued);
        assert_eq!(lead.stage, None, "a plain dispatch carries no stage");

        let chunk = put(
            &db,
            NewDispatch {
                issue_id: "linear:A-1",
                worktree_path: "/wt/a",
                agent_name: "coder",
                stage: Some("code"),
                parent_id: Some(lead.id),
                session_id: Some("sess-7"),
                artifact_path: Some(".thegn/pipeline/architect/1.md"),
            },
        )
        .unwrap();
        assert_eq!(chunk.stage.as_deref(), Some("code"));
        assert_eq!(chunk.parent_id, Some(lead.id));
        assert_eq!(chunk.session_id.as_deref(), Some("sess-7"));
        assert_eq!(
            chunk.artifact_path.as_deref(),
            Some(".thegn/pipeline/architect/1.md")
        );
        // And the roster read carries them (the columns move together).
        let listed = db.list_dispatches().unwrap();
        assert_eq!(listed[0], chunk);
    }

    #[test]
    fn put_rejects_a_parent_that_does_not_exist() {
        let (_d, db) = db("put-parent");
        let mut new = NewDispatch::new("linear:A-1", "/wt/a", "coder");
        new.parent_id = Some(4242);
        let err = put(&db, new).unwrap_err().to_string();
        assert!(err.contains("4242"), "{err}");
        assert!(
            db.list_dispatches().unwrap().is_empty(),
            "a rejected parent must not leave an orphan row"
        );
    }

    // --- run-completion contract (THE-76) -----------------------------------

    /// A throwaway git repo with an initial commit, for the fact-gathering and
    /// gate tests (which need a real worktree for `git ls-files`/`git status`).
    fn git_repo(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        assert!(git_ok(&root, &["init", "-q"]), "git init");
        assert!(git_ok(&root, &["config", "user.email", "t@t"]));
        assert!(git_ok(&root, &["config", "user.name", "t"]));
        // The ambient global config may sign commits — a test repo must not
        // block on a passphrase prompt (the same isolation every other git
        // harness in the repo applies).
        assert!(git_ok(&root, &["config", "commit.gpgsign", "false"]));
        assert!(git_ok(
            &root,
            &["commit", "--allow-empty", "-q", "-m", "init"]
        ));
        (dir, root)
    }

    fn row_in(root: &std::path::Path, artifact: Option<&str>) -> AgentDispatch {
        AgentDispatch {
            id: 7,
            issue_id: "linear:A-1".into(),
            worktree_path: root.to_string_lossy().into_owned(),
            agent_name: "coder".into(),
            dispatched_at_ms: 0,
            status: AgentDispatchStatus::Running,
            stage: Some("code".into()),
            parent_id: None,
            session_id: None,
            artifact_path: artifact.map(str::to_string),
        }
    }

    const ARTIFACT: &str = ".thegn/pipeline/THE-76/code/7.md";

    fn commit_artifact(root: &std::path::Path) {
        let path = root.join(ARTIFACT);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# handoff").unwrap();
        assert!(git_ok(root, &["add", "--", ARTIFACT]));
        assert!(git_ok(root, &["commit", "-q", "-m", "artifact"]));
    }

    #[test]
    fn facts_for_a_row_without_artifact_are_all_false_and_skip_git() {
        let (_d, root) = git_repo("no-artifact");
        let f = verify_facts(&row_in(&root, None));
        assert_eq!(f.artifact, None);
        assert!(!f.exists && !f.tracked && !f.dirty);
        // A blank artifact is the same as none — never a path join of "".
        let f = verify_facts(&row_in(&root, Some("   ")));
        assert_eq!(f.artifact, None);
    }

    #[test]
    fn facts_distinguish_missing_untracked_and_committed_artifacts() {
        let (_d, root) = git_repo("facts");
        // Missing: no file, so nothing is tracked either.
        let f = verify_facts(&row_in(&root, Some(ARTIFACT)));
        assert_eq!(f.artifact.as_deref(), Some(ARTIFACT));
        assert!(!f.exists && !f.tracked);
        assert!(!f.dirty, "a fresh repo with one commit is clean");

        // Written but never committed: exists, git does not track it — the
        // exact pilot failure the gate exists to catch.
        let path = root.join(ARTIFACT);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "wip").unwrap();
        let f = verify_facts(&row_in(&root, Some(ARTIFACT)));
        assert!(f.exists && !f.tracked);
        // …and writing it left the worktree dirty (whole-tree read).
        assert!(f.dirty);

        // Committed: tracked, and the tree is clean again.
        commit_artifact(&root);
        let f = verify_facts(&row_in(&root, Some(ARTIFACT)));
        assert!(f.exists && f.tracked && !f.dirty);
    }

    // Symlink-at-the-artifact-path is a POSIX shape; windows runners only
    // cross-compile here.
    #[cfg(unix)]
    #[test]
    fn the_done_gate_refuses_a_symlinked_artifact_even_when_committed() {
        // A committed *symlink* at the artifact path is `tracked` (the link
        // itself is in the index) but redirects the Lead's artifact read at
        // whatever the worker pointed it at. `exists` must follow the link's
        // metadata, not its target: the gate reads it as missing and refuses.
        let (_d, root) = git_repo("symlink");
        let path = root.join(ARTIFACT);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
        assert!(std::path::Path::new(&path).exists(), "the target is real");
        // Commit the link as-is — NOT via `commit_artifact`, whose `fs::write`
        // would follow the symlink and write through it.
        assert!(git_ok(&root, &["add", "--", ARTIFACT]));
        assert!(git_ok(&root, &["commit", "-q", "-m", "link"]));
        let f = verify_facts(&row_in(&root, Some(ARTIFACT)));
        assert!(f.tracked, "the link itself is in the index");
        assert!(!f.exists, "a symlink is not the artifact");
        let err = done_gate(&row_in(&root, Some(ARTIFACT)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn the_done_gate_refuses_a_missing_artifact_and_passes_a_committed_one() {
        let (_d, root) = git_repo("gate");
        // Missing artifact: refused, naming the artifact and the way out.
        let err = done_gate(&row_in(&root, Some(ARTIFACT)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not verifiably finished"), "{err}");
        assert!(err.contains(ARTIFACT), "{err}");
        assert!(err.contains("dispatch verify 7"), "{err}");
        assert!(err.contains("--force"), "{err}");

        // Untracked artifact: still refused — a session exiting is not a
        // handoff.
        let path = root.join(ARTIFACT);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "wip").unwrap();
        let err = done_gate(&row_in(&root, Some(ARTIFACT)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not track"), "{err}");

        // Committed: the gate passes.
        commit_artifact(&root);
        done_gate(&row_in(&root, Some(ARTIFACT))).unwrap();

        // A dirty tree never blocks: reported, never gating (the tracked check
        // already holds the line).
        std::fs::write(root.join(ARTIFACT), "post-commit edit").unwrap();
        done_gate(&row_in(&root, Some(ARTIFACT))).unwrap();
    }

    #[test]
    fn the_done_gate_passes_by_construction_for_a_row_without_artifact() {
        let (_d, root) = git_repo("gate-none");
        done_gate(&row_in(&root, None)).unwrap();
    }
}
