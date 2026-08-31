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
use std::collections::HashMap;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::issue::{AgentDispatch, AgentDispatchStatus, DispatchNote, NewDispatch};
use thegn_core::outln;
use thegn_core::pipeline_reap;
use thegn_core::pipeline_chunk;
use thegn_core::pipeline_report;
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
        /// The chunk file this row dispatches under (`.thegn/pipeline/<ISSUE>/
        /// code/chunk-N.md`), whose `files:` frontmatter is the row's scope.
        /// The gate reads it (and every active sibling's, from each sibling's
        /// own worktree) and refuses a scope collision with an active sibling
        /// or an unmet `after:` — the refusal names the paths and row ids.
        #[arg(long)]
        chunk: Option<String>,
        /// Dispatch even though the chunk-scope gate refused (a scope
        /// collision or an unmet `after:`). A forced dispatch is printed as
        /// such, in both output modes.
        #[arg(long)]
        force: bool,
        /// Emit the created row as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Reconcile active rows against reality: which ones still have a live
    /// worker, which finished, and which died without handing anything off.
    ///
    /// This is the join a supervisor otherwise does by hand across three
    /// places — the roster, the daemon's live sessions, and each row's
    /// artifact in git. Dry-run by default; `--apply` performs only the
    /// unambiguous transitions. A row whose artifact IS committed but which
    /// filed no report is never closed automatically: that one needs a person.
    Reap {
        /// Perform the unambiguous transitions instead of only reporting them.
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        json: bool,
    },
    /// Claim a slot and append the row in ONE atomic step — the safe
    /// alternative to `list` + judgment + `put`, which races with another
    /// monitor and with your own restart.
    ///
    /// Refuses when an equivalent row is already open (same issue/stage/
    /// worktree/artifact) or when the stage's `concurrency` budget is full.
    /// Rows whose worker has EXITED but which nobody closed still occupy their
    /// slot — that is deliberate: they are unreconciled work, not free capacity.
    Claim {
        /// Tracker issue id (`"<provider>:<key>"`, e.g. `linear:THE-57`).
        issue_id: String,
        /// The worktree the agent works in (path).
        worktree_path: String,
        /// An `[[agents]]`/`[[tools]]` name (or a provider id).
        agent_name: String,
        /// The `[[pipeline.stages]]` step this row is (e.g. `architect`). Its
        /// configured `concurrency` is the budget enforced here.
        #[arg(long)]
        stage: String,
        /// Path to the handoff artifact this row will produce. This is what
        /// distinguishes parallel chunks from a re-dispatch — give each
        /// concurrent worker its own.
        #[arg(long)]
        artifact: Option<String>,
        /// The roster row this one was chunked out of.
        #[arg(long)]
        parent: Option<i64>,
        /// The chunk file this row dispatches under.
        #[arg(long)]
        chunk: Option<String>,
        /// Create the row even though it duplicates an open one. Requires a
        /// reason, which is recorded as an audit note on the new row.
        #[arg(long, value_name = "REASON")]
        allow_duplicate: Option<String>,
        /// Emit the created row (or the refusal) as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Take, renew, release or inspect the pipeline monitor lease — the guard
    /// that stops two Leads driving one pipeline and filling the same slots.
    Lease {
        /// What to do: `acquire` | `release` | `show`.
        action: String,
        /// Who is asking (a stable id for this monitor process).
        #[arg(long)]
        owner: Option<String>,
        /// Seconds the lease stays valid; renew before it lapses. A crashed
        /// holder's lease expires on its own.
        #[arg(long, default_value_t = 300)]
        ttl: i64,
        /// The lease name — one pipeline per name.
        #[arg(long, default_value = "lead")]
        name: String,
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
    /// File the worker's structured handoff report on a roster row (THE-88):
    /// verdict, commits, unverified, findings, and next hints. The Lead reads
    /// this instead of re-reading the artifact. Last write wins; the report
    /// is a pointer-summary, the artifact stays in git. Empty/oversize text
    /// is refused by the core policy.
    Report {
        /// The dispatch row id (see `dispatch list`).
        id: i64,
        /// The report body. Trimmed; must be 1..=16_384 chars.
        #[arg(long)]
        text: String,
        /// Emit the write as JSON (`{ "id", "report", "bytes" }`).
        #[arg(long)]
        json: bool,
    },
    /// Append a progress note to a row's queue (THE-88): one line of context
    /// the supervisor reads on demand via `dispatch status --since`. Trimmed;
    /// must be 1..=4_096 chars.
    Note {
        /// The dispatch row id (see `dispatch list`).
        id: i64,
        /// The note body. Trimmed; must be 1..=4_096 chars.
        #[arg(long)]
        text: String,
        /// Emit the write as JSON (`{ "id", "note_id", "created_at_ms" }`).
        #[arg(long)]
        json: bool,
    },
    /// The on-demand status summary (THE-88, `/btw`). Without `row`: active
    /// rows only (or every row with `--all`); with `row`: that row's report
    /// verbatim plus the notes since `--since` (default: all, capped at the
    /// last 20).
    Status {
        /// A specific row id to read (the report verbatim + notes since
        /// `--since`). Refused when the id is unknown.
        row: Option<i64>,
        /// Only notes with `created_at_ms > since` count (epoch-ms; matches
        /// `dispatched_at_ms` and the note stamp). The row's report is always
        /// shown in full.
        #[arg(long)]
        since: Option<i64>,
        /// Include every row (terminals too), not just active ones.
        #[arg(long)]
        all: bool,
        /// Emit as JSON (one digest array; row mode adds a `notes` array).
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List { active, json } => list(active, json),
        Action::Reap { apply, json } => reap(cfg, apply, json),
        Action::Claim {
            issue_id,
            worktree_path,
            agent_name,
            stage,
            artifact,
            parent,
            chunk,
            allow_duplicate,
            json,
        } => claim(
            cfg,
            &issue_id,
            &worktree_path,
            &agent_name,
            &stage,
            artifact.as_deref(),
            parent,
            chunk.as_deref(),
            allow_duplicate.as_deref(),
            json,
        ),
        Action::Lease {
            action,
            owner,
            ttl,
            name,
            json,
        } => lease(&action, owner.as_deref(), ttl, &name, json),
        Action::Put {
            issue_id,
            worktree_path,
            agent_name,
            stage,
            parent,
            session,
            artifact,
            chunk,
            force,
            json,
        } => {
            let db = Db::open()?;
            if let Some(chunk_path) = chunk.as_deref() {
                // The chunk-scope gate runs BEFORE the insert: a refused put
                // must leave no row behind (a refused scope is not a
                // dispatch, and a row stuck queued would read as un-driven).
                chunk_gate(&db, &worktree_path, &issue_id, chunk_path, force)?;
            }
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
                    chunk_path: chunk.as_deref(),
                },
            )?;
            if json {
                let mut v = serde_json::to_value(&row)?;
                if force {
                    // A forced dispatch is never invisible (the set-status
                    // done --force idiom).
                    v["forced"] = serde_json::json!(true);
                }
                return super::emit_json(&v);
            }
            if force {
                outln!("dispatch {} → {} (forced)", row.id, row.status.as_str());
            } else {
                outln!("dispatch {} → {}", row.id, row.status.as_str());
            }
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
            row,
            any,
            timeout,
            json,
        } => wait(cfg, row, any, timeout, json),
        Action::Report { id, text, json } => report(id, &text, json),
        Action::Note { id, text, json } => note(id, &text, json),
        Action::Status {
            row,
            since,
            all,
            json,
        } => status(row, since, all, json),
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

/// The chunk-scope gate (THE-86): before a row carrying `--chunk` is
/// inserted, its chunk file's `files:` frontmatter is checked against every
/// ACTIVE sibling's scope — rows of the same issue in the same worktree,
/// non-terminal, each with its own `chunk_path`. A scope collision is a
/// refusal naming the colliding paths and the sibling row ids; an `after:`
/// chunk that is not `done` is a refusal naming the chunk and its row
/// status. Shared by `dispatch put --chunk` and `session open --chunk`
/// (two callers, one refusal — two implementations would drift).
///
/// `--force` is the way out, exactly like the `set-status done --force`
/// idiom: it overrides a refusal AND an unparseable/unreadable chunk file,
/// and every caller reports the forced dispatch in its output. Without
/// `--force` the gate is strict: a chunk file that cannot be read or parsed
/// is a refusal (naming the line), because a typo'd scope must not silently
/// opt the row out of the gate.
///
/// Sibling chunk files are read from each sibling's OWN recorded worktree,
/// best-effort: an unreadable sibling contributes an empty scope (which
/// never conflicts) rather than an error — one broken file must not wedge
/// the whole roster's gate.
pub(crate) fn chunk_gate(
    db: &Db,
    worktree: &str,
    issue_id: &str,
    chunk_path: &str,
    force: bool,
) -> Result<()> {
    if force {
        // The explicit override: no read, no parse, no verdict. The caller's
        // output says the dispatch was forced.
        return Ok(());
    }
    let body = read_chunk_file(worktree, chunk_path).map_err(|e| {
        anyhow::anyhow!(
            "chunk file {chunk_path} is not readable in {worktree}: {e} — fix the --chunk path, \
             or drop the flag to dispatch without a scope"
        )
    })?;
    let scope = pipeline_chunk::parse_frontmatter(&body).map_err(|e| {
        anyhow::anyhow!(
            "chunk file {chunk_path} is not a valid scope ({e}) — fix the frontmatter, \
             or --force to dispatch anyway"
        )
    })?;

    // Siblings of the new row: same issue, same worktree, carrying a chunk
    // path. Terminal rows leave the gate's picture entirely — `done` feeds
    // the after-set, the other terminals are simply history.
    let rows = db.list_dispatches()?;
    let mut active_scopes: Vec<pipeline_chunk::ActiveScope> = Vec::new();
    let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &rows {
        let Some(sibling_path) = r
            .chunk_path
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        else {
            continue;
        };
        if r.issue_id != issue_id || r.worktree_path != worktree {
            continue;
        }
        let name = chunk_name(sibling_path).to_string();
        if r.status == AgentDispatchStatus::Done {
            done.insert(name);
            continue;
        }
        if r.status.is_terminal() {
            continue;
        }
        // Read the sibling's scope from ITS worktree — best-effort (see the
        // doc comment): unreadable means empty scope, never an error.
        let files = read_chunk_file(&r.worktree_path, sibling_path)
            .ok()
            .and_then(|body| pipeline_chunk::parse_frontmatter(&body).ok())
            .map(|s| s.files)
            .unwrap_or_default();
        active_scopes.push(pipeline_chunk::ActiveScope {
            row: r.id,
            name,
            files,
        });
    }

    // Both axes are computed even when the verdict short-circuits, so a
    // mixed problem (a collision AND an unmet after) is refused in one
    // message that names everything.
    let conflicts = match pipeline_chunk::verdict(&scope, &active_scopes, &done) {
        pipeline_chunk::ScopeVerdict::Ok => Vec::new(),
        pipeline_chunk::ScopeVerdict::Conflict { overlaps } => overlaps,
        pipeline_chunk::ScopeVerdict::UnmetAfter(_) => Vec::new(),
    };
    let unmet = pipeline_chunk::after_unmet(&scope.after, &done);

    let mut lines: Vec<String> = Vec::new();
    let new_name = chunk_name(chunk_path);
    for (idx, pairs) in conflicts {
        let sib = &active_scopes[idx];
        for (mine, theirs) in pairs {
            lines.push(format!(
                "{new_name} vs {}: {mine} collides with {theirs} (active row {})",
                sib.name, sib.row
            ));
        }
    }
    for name in &unmet {
        // The refusal names the chunk AND its row status: the row holding
        // that chunk name for this issue+worktree, whatever its state.
        let holder = rows.iter().find(|r| {
            r.chunk_path
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .is_some_and(|c| chunk_name(c) == name)
                && r.issue_id == issue_id
                && r.worktree_path == worktree
        });
        let detail = match holder {
            Some(r) => format!("row {}: {}", r.id, r.status.as_str()),
            None => "no dispatch row for it".to_string(),
        };
        lines.push(format!("after {name} is not done ({detail})"));
    }
    if lines.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "chunk scope gate refused {chunk_path}:\n  - {}\nresolve the overlap in the chunk's \
         files:/overlaps:/after: frontmatter, or --force to dispatch anyway",
        lines.join("\n  - ")
    );
}

/// Resolve `chunk_path` against `worktree` (absolute paths as-is, everything
/// else joined) and read it. The caller decides what a miss means — for the
/// NEW row's file it is a refusal; for a sibling's it degrades to an empty
/// scope.
fn read_chunk_file(worktree: &str, chunk_path: &str) -> std::io::Result<String> {
    let p = std::path::Path::new(chunk_path);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::path::Path::new(worktree).join(p)
    };
    std::fs::read_to_string(p)
}

/// The chunk NAME a `files:`/`overlaps:`/`after:` reference resolves to: the
/// chunk file's basename without extension (`…/code/chunk-2.md` →
/// `chunk-2`). The architect's frontmatter names siblings by this stem.
fn chunk_name(chunk_path: &str) -> &str {
    std::path::Path::new(chunk_path)
        .file_stem()
        .map(|s| s.to_str().unwrap_or(""))
        .unwrap_or("")
}

/// `dispatch claim` — the atomic slot check plus insert.
///
/// The stage's budget comes from `[[pipeline.stages]]`, so the number enforced
/// is the one the operator configured; an unknown stage name has no budget and
/// only the duplicate rule applies (naming a stage thegn does not know is a
/// supervisor bug, but refusing every dispatch for it would be a worse one).
#[allow(clippy::too_many_arguments)]
fn claim(
    cfg: &Config,
    issue_id: &str,
    worktree_path: &str,
    agent_name: &str,
    stage: &str,
    artifact: Option<&str>,
    parent: Option<i64>,
    chunk: Option<&str>,
    allow_duplicate: Option<&str>,
    json: bool,
) -> Result<()> {
    let db = Db::open()?;
    let limit = cfg
        .pipeline
        .stage(stage)
        .map(|s| s.concurrency)
        .unwrap_or(0);
    let outcome = db.claim_dispatch(
        NewDispatch {
            issue_id,
            worktree_path,
            agent_name,
            stage: Some(stage),
            parent_id: parent,
            session_id: None,
            artifact_path: artifact,
            chunk_path: chunk,
        },
        limit,
        allow_duplicate,
    )?;
    match outcome {
        Ok(id) => {
            let row = db.get_dispatch(id)?;
            if json {
                let mut v = serde_json::json!({ "granted": true, "id": id });
                if let Some(r) = &row {
                    v["row"] = serde_json::to_value(r)?;
                }
                if let Some(why) = allow_duplicate {
                    v["allowed_duplicate"] = serde_json::json!(why);
                }
                return super::emit_json(&v);
            }
            if allow_duplicate.is_some() {
                outln!("dispatch {id} claimed (duplicate explicitly authorized)");
            } else {
                outln!("dispatch {id} claimed for stage {stage}");
            }
            Ok(())
        }
        Err(decision) => {
            if json {
                super::emit_json(&serde_json::json!({
                    "granted": false,
                    "reason": decision.reason(),
                }))?;
            }
            // Exit 2 = retryable, matching `verify`/`wait`: the supervisor's
            // correct response is to reconcile and try again, not to abort.
            anyhow::bail!(
                "dispatch refused: {}\n(exit 2 — reconcile and retry)",
                decision.reason()
            )
        }
    }
}

/// `dispatch lease` — monitor ownership.
fn lease(action: &str, owner: Option<&str>, ttl: i64, name: &str, json: bool) -> Result<()> {
    let db = Db::open()?;
    match action {
        "show" => {
            let held = db.pipeline_lease_holder(name)?;
            if json {
                return super::emit_json(&serde_json::json!({
                    "name": name,
                    "owner": held.as_ref().map(|(o, _)| o.clone()),
                    "expires_in_ms": held.as_ref().map(|(_, ms)| *ms),
                }));
            }
            match held {
                Some((o, ms)) => outln!("lease {name}: held by {o} ({}s left)", ms / 1000),
                None => outln!("lease {name}: free"),
            }
            Ok(())
        }
        "acquire" | "release" => {
            let owner = owner.ok_or_else(|| {
                anyhow::anyhow!("--owner is required for `{action}` (a stable id for this monitor)")
            })?;
            if action == "release" {
                let dropped = db.release_pipeline_lease(name, owner)?;
                if json {
                    return super::emit_json(&serde_json::json!({ "released": dropped }));
                }
                outln!(
                    "lease {name}: {}",
                    if dropped {
                        "released"
                    } else {
                        "not held by this owner — nothing released"
                    }
                );
                return Ok(());
            }
            match db.acquire_pipeline_lease(name, owner, ttl)? {
                Ok(()) => {
                    if json {
                        return super::emit_json(
                            &serde_json::json!({ "acquired": true, "owner": owner, "ttl": ttl }),
                        );
                    }
                    outln!("lease {name}: held by {owner} for {ttl}s");
                    Ok(())
                }
                Err(holder) => {
                    if json {
                        super::emit_json(
                            &serde_json::json!({ "acquired": false, "holder": holder }),
                        )?;
                    }
                    anyhow::bail!(
                        "lease {name} is held by {holder} — another monitor is already driving \
                         this pipeline. Stop it, or wait for its lease to lapse, before starting \
                         a second one.\n(exit 2 — retryable)"
                    )
                }
            }
        }
        other => anyhow::bail!("unknown lease action {other:?} (expected: acquire, release, show)"),
    }
}

fn list(active: bool, json: bool) -> Result<()> {
    let db = Db::open()?;
    let mut rows = db.list_dispatches()?;
    if active {
        rows.retain(|d| d.status.is_active());
    }
    if json {
        // One value per row: the stored fields plus the parsed scope. `chunk_files`
        // (the chunk file's `files:` list) is a best-effort read at list time —
        // the file lives in the worktree and may be gone; the key is then
        // omitted rather than emitted empty (an empty list would read as "this
        // chunk touches nothing", the opt-out, which is a different claim).
        let mut vals: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
        let now_ms = thegn_core::util::now_ms();
        for d in &rows {
            let mut v = serde_json::to_value(d)?;
            // The derived liveness, so a supervisor need not re-implement the
            // exited-vs-running distinction (and get it wrong, as one did).
            v["liveness"] = serde_json::json!(pipeline_run::row_liveness(d, now_ms).token());
            if let Some(scope) = d
                .chunk_path
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .and_then(|chunk_path| read_chunk_file(&d.worktree_path, chunk_path).ok())
                .and_then(|body| pipeline_chunk::parse_frontmatter(&body).ok())
            {
                v["chunk_files"] = serde_json::json!(scope.files);
            }
            vals.push(v);
        }
        return super::emit_json(&vals);
    }
    if rows.is_empty() {
        outln!("no dispatches");
        return Ok(());
    }
    let now_ms = thegn_core::util::now_ms();
    // How many active rows have an exited worker: the number that turns "121
    // running" from a workload into a backlog. Printed once, after the table.
    let mut stale = 0usize;
    for d in &rows {
        // A row whose worker exited but which nobody closed prints its status
        // with the exit made explicit — `running!exited` rather than a bare
        // `running` that reads identically to a live worker.
        let status_cell = match pipeline_run::row_liveness(d, now_ms) {
            pipeline_run::RowLiveness::ExitedUnverified { .. } => {
                stale += 1;
                format!("{}!exited", d.status.as_str())
            }
            _ => d.status.as_str().to_string(),
        };
        // The pipeline columns print as `-` when absent rather than collapsing
        // the row's shape, so the table stays column-aligned for a roster that
        // mixes pipeline and plain dispatches.
        outln!(
            "{}  {}  {}  {}  {}  {}  {}  {}  {}",
            d.id,
            status_cell,
            d.stage.as_deref().unwrap_or("-"),
            d.parent_id
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
            d.issue_id,
            d.agent_name,
            d.worktree_path,
            note_cell(d.note.as_deref()),
            chunk_cell(d.chunk_path.as_deref()),
        );
    }
    if stale > 0 {
        outln!(
            "\n{stale} row(s) marked `!exited`: the worker is gone but the row is still open. \
             These occupy a slot and are NOT free capacity — run `thegn dispatch verify <id>` \
             and close each with `set-status done|failed` before dispatching more."
        );
    }
    Ok(())
}

/// The trailing `chunk` column: the basename of the chunk file the row
/// dispatches under — the pointer, not the scope (the scope lives in the
/// file's `files:` frontmatter; `dispatch list --json` carries the parsed
/// list). `-` when unset (every pre-v60 row and any dispatch made without
/// `--chunk`).
fn chunk_cell(chunk_path: Option<&str>) -> String {
    chunk_path
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .and_then(|c| {
            std::path::Path::new(c)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "-".into())
}

/// The trailing `note` column: the daemon's transport-retry ledger (THE-86),
/// collapsed to one line and truncated so the roster stays scannable. `-`
/// when absent (every pre-v59 row and anything the observer never touched).
fn note_cell(note: Option<&str>) -> String {
    const MAX: usize = 32;
    let Some(n) = note else {
        return "-".into();
    };
    let one_line = n.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX {
        one_line
    } else {
        let t: String = one_line.chars().take(MAX).collect();
        format!("{t}…")
    }
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

/// The `set-status done` gate (THE-76 + THE-88): a row that carries an
/// artifact may only be recorded done when its handoff is real. Two
/// gates, both in [`pipeline_run::verify_report`]:
///
/// 1. The artifact is present in the row's worktree **and** tracked by git
///    (THE-76's original rule — a session exiting is not a handoff).
/// 2. The row carries a non-empty `report` (THE-88 — the worker must file
///    `thegn dispatch report <id>`; the report is what the Lead reads, the
///    artifact pointer only tells it where to look if a reviewer needs to).
///
/// `--force` is the deliberate override for BOTH rules (the refusal names
/// it). Pane-exit auto-stamps (`pty_drain.rs:855-895`) bypass the gate
/// entirely — that path writes the typed status for attribution, not as a
/// handoff verdict, so it is not subject to either check.
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
/// implementation shared by `dispatch verify`, the `done` gate, and
/// `session open --resume-work`'s finisher facts. A row with
/// no artifact is never gated, so no git subprocess is spent on it; the facts
/// read as all-false and `verify_report`'s first rule turns that into `ok`.
/// `pub(crate)`: the resume path in `cmd/session.rs` reads the same facts —
/// one implementation, two callers.
pub(crate) fn verify_facts(row: &AgentDispatch) -> pipeline_run::VerifyFacts {
    // THE-88: report presence is a row fact even for plain dispatches. It is
    // only a completion gate when an artifact is present, but `dispatch
    // verify` should faithfully report the column in either case.
    let report_present = row.report.as_deref().is_some_and(|r| !r.trim().is_empty());
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
            // A plain (non-pipeline) row is never gated; the report fact is
            // still returned so verify reflects the row faithfully.
            report_present,
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
    // THE-88: a row carrying an artifact must also file a report —
    // `verify_report` turns the missing case into an `ok=false` reason that
    // names the `dispatch report` command as the fix.
    pipeline_run::VerifyFacts {
        artifact: Some(artifact),
        exists,
        tracked,
        dirty,
        report_present,
    }
}

/// `dispatch reap` — reconcile active rows against the daemon and git.
///
/// The three-way join a supervisor otherwise performs by hand. Liveness comes
/// from the daemon when it is reachable; when it is NOT, every session reads as
/// absent, which is correct — a restarted daemon really has lost them — and is
/// safe, because a row is only ever auto-closed on a committed artifact plus a
/// filed report. The genuinely ambiguous case (artifact committed, no report)
/// is reported and left alone.
fn reap(cfg: &Config, apply: bool, json: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let (live_ids, daemon_up) = rt.block_on(live_session_ids(cfg));
    let db = Db::open()?;
    let rows: Vec<_> = db
        .list_dispatches()?
        .into_iter()
        .filter(|r| r.status.is_active())
        .collect();

    let plan = pipeline_reap::plan(&rows, |r| {
        let f = verify_facts(r);
        pipeline_reap::ReapFacts {
            session_live: r
                .session_id
                .as_deref()
                .is_some_and(|s| live_ids.iter().any(|l| l == s)),
            artifact_exists: f.exists,
            artifact_tracked: f.tracked,
            report_present: f.report_present,
        }
    });
    let summary = pipeline_reap::summarize(&plan);

    if json {
        let items: Vec<_> = plan
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "verdict": r.verdict.token(),
                    "why": match &r.verdict {
                        pipeline_reap::ReapVerdict::MarkFailed { why }
                        | pipeline_reap::ReapVerdict::NeedsDecision { why } => Some(*why),
                        _ => None,
                    },
                })
            })
            .collect();
        super::emit_json(&serde_json::json!({
            "daemon_reachable": daemon_up,
            "applied": apply,
            "rows": items,
            "summary": {
                "live": summary.live,
                "close_done": summary.close_done,
                "mark_failed": summary.mark_failed,
                "needs_decision": summary.needs_decision,
            },
        }))?;
    } else {
        if !daemon_up {
            outln!(
                "note: no daemon reachable — every session reads as absent. That is correct \\
                 after a restart, and safe: nothing closes without a committed artifact AND a \\
                 report."
            );
        }
        for r in &plan {
            match &r.verdict {
                pipeline_reap::ReapVerdict::Live => outln!("  {} live", r.id),
                pipeline_reap::ReapVerdict::MarkFailed { why }
                | pipeline_reap::ReapVerdict::NeedsDecision { why } => {
                    outln!("  {} {}: {why}", r.id, r.verdict.token());
                }
                v => outln!("  {} {}", r.id, v.token()),
            }
        }
        outln!(
            "\\n{} live, {} to close done, {} to mark failed, {} need a decision",
            summary.live,
            summary.close_done,
            summary.mark_failed,
            summary.needs_decision
        );
        if !apply && summary.actionable() > 0 {
            outln!("(dry run — pass --apply to perform the {} unambiguous transition(s))", summary.actionable());
        }
    }

    if !apply {
        return Ok(());
    }
    for r in &plan {
        match &r.verdict {
            pipeline_reap::ReapVerdict::CloseDone => {
                // Plain, gated `done`: it passes unforced precisely because the
                // artifact is committed and a report exists.
                db.update_dispatch_status(r.id, AgentDispatchStatus::Done)?;
                outln!("dispatch {} → done", r.id);
            }
            pipeline_reap::ReapVerdict::MarkFailed { why } => {
                // best-effort: the note is context; losing it must not block
                // recording the outcome, which is the thing that matters.
                let _ = db.append_dispatch_note(r.id, &format!("reaped: {why}"));
                db.update_dispatch_status(r.id, AgentDispatchStatus::Failed)?;
                outln!("dispatch {} → failed", r.id);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Live (non-tombstone) session ids, plus whether the daemon answered at all.
async fn live_session_ids(cfg: &Config) -> (Vec<String>, bool) {
    match crate::cmd::session::connect(cfg).await {
        Ok(client) => match client.sessions().await {
            Ok(sessions) => (
                sessions
                    .into_iter()
                    .filter(|s| s.exited_at_ms.is_none())
                    .map(|s| s.id)
                    .collect(),
                true,
            ),
            Err(_) => (Vec::new(), false),
        },
        Err(_) => (Vec::new(), false),
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
            "dispatch {id}  artifact {a}  exists={} tracked={} dirty={} report={}  ok={}",
            yes_no(report.exists),
            yes_no(report.tracked),
            yes_no(report.dirty),
            yes_no(report.report_present),
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

/// `dispatch report <id> --text <text>` — write/overwrite the worker's
/// structured handoff report on a roster row (THE-88). Validates through
/// `pipeline_report::report_text` (empty/oversize errors surface verbatim),
/// row must exist, writes via `db.set_dispatch_report`.
fn report(id: i64, text: &str, json: bool) -> Result<()> {
    let db = Db::open()?;
    write_report(&db, id, text, json)
}

/// THE-88 — the DB-bound half of `dispatch report`. Validated text → row.
pub(crate) fn write_report(db: &Db, id: i64, text: &str, json: bool) -> Result<()> {
    let validated = pipeline_report::report_text(text).map_err(|e| anyhow::anyhow!("{e}"))?;
    db.set_dispatch_report(id, &validated)?;
    if json {
        let v = serde_json::json!({
            "id": id,
            "report": validated,
            "bytes": validated.len(),
        });
        super::emit_json(&v)?;
    } else {
        outln!(
            "report recorded on dispatch {} ({} chars)",
            id,
            validated.chars().count()
        );
    }
    Ok(())
}

/// `dispatch note <id> --text <text>` — append a progress note to a row's
/// queue (THE-88). Validates through `pipeline_report::note_text`
/// (empty/oversize errors surface verbatim), row must exist, writes via
/// `db.append_dispatch_note`. Returns the new note's id and timestamp.
fn note(id: i64, text: &str, json: bool) -> Result<()> {
    let db = Db::open()?;
    write_note(&db, id, text, json)
}

/// THE-88 — the DB-bound half of `dispatch note`.
pub(crate) fn write_note(db: &Db, id: i64, text: &str, json: bool) -> Result<()> {
    let validated = pipeline_report::note_text(text).map_err(|e| anyhow::anyhow!("{e}"))?;
    let note_id = db.append_dispatch_note(id, &validated)?;
    let note = db
        .dispatch_notes(id, None, 0)?
        .into_iter()
        .find(|n| n.id == note_id)
        .ok_or_else(|| anyhow::anyhow!("note {note_id} vanished after insert"))?;
    if json {
        let v = serde_json::json!({
            "id": id,
            "note": validated,
            "note_id": note.id,
            "created_at_ms": note.created_at_ms,
        });
        super::emit_json(&v)?;
    } else {
        outln!(
            "note recorded on dispatch {} ({} chars)",
            id,
            validated.chars().count()
        );
    }
    Ok(())
}

/// The row-mode status display is bounded to the newest notes. The digest
/// itself receives the full since-filtered set so its `note_count` remains a
/// count of all matching notes, not merely the displayed suffix.
fn newest_status_notes(notes: &[DispatchNote]) -> &[DispatchNote] {
    if notes.len() > 20 {
        &notes[notes.len() - 20..]
    } else {
        notes
    }
}

/// `dispatch status [row] [--since <epoch-ms>] [--all] [--json]` — the
/// on-demand status summary (THE-88, `/btw`). Composes with
/// `pipeline_report::digest`: without `row` → active rows only
/// (`is_active()`), `--all` → every row; with `row` → that row (error
/// naming the id when unknown), report verbatim + notes since `--since`
/// (default all, capped last 20 via `db.dispatch_notes`). JSON: the
/// digest array (row mode: one digest + `notes` array). Human: one line
/// per row `id status stage issue notes=N last=<truncated latest>` and, in
/// row mode, the report body printed verbatim under a `report:` line.
fn status(row: Option<i64>, since_ms: Option<i64>, all: bool, json: bool) -> Result<()> {
    let db = Db::open()?;
    read_status(&db, row, since_ms, all, json)
}

/// THE-88 — the DB-bound half of `dispatch status`. Tests exercise the
/// digest composition through this entry point against a tempdir DB.
pub(crate) fn read_status(
    db: &Db,
    row: Option<i64>,
    since_ms: Option<i64>,
    all: bool,
    json: bool,
) -> Result<()> {
    let mut rows = db.list_dispatches()?;
    if let Some(id) = row {
        rows.retain(|r| r.id == id);
        if rows.is_empty() {
            anyhow::bail!("no dispatch with id {id}");
        }
    } else if !all {
        rows.retain(|d| d.status.is_active());
    }

    let row_mode = row.is_some();
    let mut notes_map: HashMap<i64, Vec<DispatchNote>> = HashMap::new();
    for r in &rows {
        // Keep the full since-filtered set for `digest`: the output is capped,
        // but `note_count` must still describe the complete queue window.
        notes_map.insert(r.id, db.dispatch_notes(r.id, since_ms, 0)?);
    }
    let digests = pipeline_report::digest(&rows, &notes_map, since_ms);
    if json {
        if row_mode {
            let digest = digests.into_iter().next().expect("one selected row");
            let mut v = serde_json::to_value(digest)?;
            let notes = newest_status_notes(
                notes_map
                    .get(&row.expect("row mode"))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            )
            .iter()
            .map(|n| {
                serde_json::json!({
                    "id": n.id,
                    "dispatch_id": n.dispatch_id,
                    "created_at_ms": n.created_at_ms,
                    "text": n.text,
                })
            })
            .collect::<Vec<_>>();
            v["notes"] = serde_json::Value::Array(notes);
            super::emit_json(&v)?;
        } else {
            super::emit_json(&digests)?;
        }
    } else {
        for digest in &digests {
            let latest = digest
                .latest_note
                .as_ref()
                .map(|(_, text)| text.as_str())
                .unwrap_or("-");
            let latest = note_cell(Some(latest));
            outln!(
                "{} {} {} {} notes={} last={}",
                digest.id,
                digest.status,
                digest.stage.as_deref().unwrap_or("-"),
                digest.issue_id,
                digest.note_count,
                latest
            );
        }
        if row_mode {
            if let Some(report) = digests.first().and_then(|d| d.report.as_deref()) {
                outln!("report:");
                outln!("{report}");
            }
            outln!("notes:");
            if let Some(all_notes) = notes_map.get(&row.expect("row mode")) {
                for note in newest_status_notes(all_notes) {
                    outln!("- {}", note.text);
                }
            }
        }
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
fn wait(cfg: &Config, row: Option<i64>, any: bool, timeout: Option<i64>, json: bool) -> Result<()> {
    validate_wait_timeout(row, any, timeout)?;
    let db = Db::open()?;
    let rows = db.list_dispatches()?;
    let targets = pipeline_run::wait_candidates(&rows, row).map_err(|e| anyhow::anyhow!("{e}"))?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(wait_wake(cfg, targets, timeout, json))
}

fn validate_wait_timeout(row: Option<i64>, any: bool, timeout: Option<i64>) -> Result<()> {
    if timeout.is_some_and(|ms| ms <= 0) {
        anyhow::bail!("dispatch wait --timeout must be greater than zero milliseconds");
    }
    // A row-less wait is the --any form, including the documented default when
    // neither --row nor --any is supplied. It is used by background monitors,
    // so an omitted timeout must never create an unbounded supervisor wait.
    if row.is_none() && timeout.is_none() {
        let form = if any { "--any" } else { "without --row" };
        anyhow::bail!("dispatch wait {form} requires --timeout");
    }
    Ok(())
}

/// Read `(report, artifact_path)` for a roster row by id — the wake-time
/// fact fetch (THE-88). Both are `None` only when the row has been reaped
/// between the daemon's exit and our DB read: a missing row is the wake's own
/// answer (the tombstone fired), the caller still gets a clean wake. Database
/// open/query failures remain errors so they cannot look like a worker with no
/// report.
fn row_report_artifact(id: i64) -> Result<(Option<String>, Option<String>)> {
    let db = Db::open()?;
    row_report_artifact_db(&db, id)
}

/// THE-88 — the DB-bound half of `row_report_artifact`. The unit tests
/// exercise this directly against a tempdir DB.
pub(crate) fn row_report_artifact_db(db: &Db, id: i64) -> Result<(Option<String>, Option<String>)> {
    row_report_artifact_from(db.get_dispatch(id))
}

fn row_report_artifact_from(
    row: Result<Option<AgentDispatch>>,
) -> Result<(Option<String>, Option<String>)> {
    match row {
        Ok(Some(row)) => Ok((row.report, row.artifact_path)),
        Ok(None) => Ok((None, None)),
        Err(e) => Err(e),
    }
}

/// The daemon's wait endpoint reports a reaped session as HTTP 404. Every
/// other control error is infrastructure or protocol failure and must remain
/// retryable rather than becoming a false worker completion.
fn classify_wait_outcome(
    outcome: anyhow::Result<serde_json::Value>,
) -> Result<(bool, Option<i64>, bool)> {
    match outcome {
        Ok(v) => {
            let matched = v
                .get("matched")
                .and_then(|m| m.as_bool())
                .ok_or_else(|| anyhow::anyhow!("malformed wait response: {v}"))?;
            let exit_code = match v.get("exit_code") {
                None | Some(serde_json::Value::Null) => None,
                Some(code) => Some(
                    code.as_i64()
                        .ok_or_else(|| anyhow::anyhow!("malformed wait response: {v}"))?,
                ),
            };
            Ok((matched, exit_code, false))
        }
        Err(e) if is_reaped_wait_error(&e) => Ok((true, None, true)),
        Err(e) => Err(e),
    }
}

fn is_reaped_wait_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<thegn_svc::control::client::ControlRequestError>()
        .is_some_and(|e| e.status() == 404)
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
            let out = match timeout {
                Some(ms) => match tokio::time::timeout(
                    std::time::Duration::from_millis(ms as u64),
                    client.wait(
                        &t.session_id,
                        serde_json::json!({ "kind": "exited" }),
                        Some(ms),
                    ),
                )
                .await
                {
                    Ok(out) => out,
                    Err(_) => Err(anyhow::anyhow!(
                        "dispatch wait timed out after {ms} milliseconds"
                    )),
                },
                None => {
                    client
                        .wait(&t.session_id, serde_json::json!({ "kind": "exited" }), None)
                        .await
                }
            };
            (t, out)
        });
    }
    // The first wake wins and dropping the set cancels the remaining waits
    // (JoinSet's cancel-on-drop). A `matched: false` is a timeout answer, not a
    // wake: keep listening for the others until every target has answered, so
    // one target timing out a beat before another exits still wakes us.
    while let Some(joined) = set.join_next().await {
        let (t, outcome) = joined.map_err(|e| anyhow::anyhow!("wait task failed: {e}"))?;
        // Only a daemon HTTP 404 means the selected session was reaped after
        // selection. Preserve all other control/protocol errors and mark them
        // retryable so the monitor cannot advance on a false wake.
        let (matched, exit_code, gone) = classify_wait_outcome(outcome)
            .map_err(|e| anyhow::Error::new(crate::cmd::Retryable(e)))?;
        if !matched {
            continue;
        }
        // THE-88: read the row's report/artifact AT WAKE TIME (not at candidate
        // selection — the worker files the report seconds before exit). Only a
        // missing row is an empty fact; DB open/query failures are retryable.
        let (report, artifact) =
            row_report_artifact(t.id).map_err(|e| anyhow::Error::new(crate::cmd::Retryable(e)))?;
        if json {
            let mut v = serde_json::json!({
                "row": t.id,
                "session": t.session_id,
                "stage": t.stage,
                "issue": t.issue_id,
                "exit_code": exit_code,
                "matched": true,
                "report": report,
                "artifact": artifact,
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
        // Print the report under the exited line — the Lead reads it and
        // nothing else. A missing/blank report prints nothing: the empty
        // line keeps the human output one event per wake.
        if let Some(text) = report.as_deref().filter(|t| !t.trim().is_empty()) {
            outln!("report:");
            outln!("{text}");
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
    fn the_note_column_is_one_line_and_truncated() {
        assert_eq!(note_cell(None), "-");
        assert_eq!(
            note_cell(Some("limit: weekly limit")),
            "limit: weekly limit"
        );
        // A multi-line note (an attempt line with a relaunch failure under it)
        // collapses to one line; a long one truncates with an ellipsis.
        let two_lines = "transport: connection error. (attempt 3/3)\nrelaunch failed: no harness";
        assert_eq!(
            note_cell(Some(two_lines)),
            "transport: connection error. (at…"
        );
    }

    #[test]
    fn status_display_keeps_only_the_newest_twenty_notes() {
        let notes: Vec<DispatchNote> = (0..21)
            .map(|id| DispatchNote {
                id,
                dispatch_id: 7,
                created_at_ms: id,
                text: format!("note-{id}"),
            })
            .collect();
        let shown = newest_status_notes(&notes);
        assert_eq!(shown.len(), 20);
        assert_eq!(shown.first().map(|n| n.id), Some(1));
        assert_eq!(shown.last().map(|n| n.id), Some(20));
        assert_eq!(notes.len(), 21);
    }

    #[test]
    fn rowless_wait_requires_a_positive_hard_timeout() {
        assert!(
            validate_wait_timeout(None, true, None)
                .unwrap_err()
                .to_string()
                .contains("requires --timeout")
        );
        assert!(validate_wait_timeout(None, false, Some(0)).is_err());
        assert!(validate_wait_timeout(Some(7), false, None).is_ok());
        assert!(validate_wait_timeout(None, true, Some(600_000)).is_ok());
    }

    #[test]
    fn the_chunk_cell_is_the_basename_or_a_dash() {
        assert_eq!(chunk_cell(None), "-");
        assert_eq!(chunk_cell(Some("   ")), "-");
        assert_eq!(
            chunk_cell(Some(".thegn/pipeline/A-1/code/chunk-2.md")),
            "chunk-2.md"
        );
        // An absolute path shows the same pointer; the scope itself lives in
        // the file and is only in `dispatch list --json`.
        assert_eq!(chunk_cell(Some("/wt/a/chunk-7.md")), "chunk-7.md");
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
                chunk_path: Some(".thegn/pipeline/A-1/code/chunk-1.md"),
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
        assert_eq!(
            chunk.chunk_path.as_deref(),
            Some(".thegn/pipeline/A-1/code/chunk-1.md")
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

    // --- chunk-scope gate (THE-86) -------------------------------------------

    /// A worktree dir holding the named chunk files (`path, body`), for the
    /// gate tests. No git: the gate touches the roster and plain files only.
    /// The `String` is the worktree path, borrowed by every call below.
    fn chunk_wt(files: &[(&str, &str)]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        for (path, body) in files {
            let p = dir.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
        }
        let wt = dir.path().to_string_lossy().into_owned();
        (dir, wt)
    }

    /// One queued (active) sibling row carrying `chunk` in worktree `wt`.
    fn chunk_row(db: &Db, wt: &str, chunk: &str) -> i64 {
        put(
            db,
            NewDispatch {
                chunk_path: Some(chunk),
                ..NewDispatch::new("linear:A-1", wt, "coder")
            },
        )
        .unwrap()
        .id
    }

    #[test]
    fn the_gate_refuses_an_overlapping_active_sibling_naming_paths_rows_and_force() {
        let (_d, wt) = chunk_wt(&[
            ("chunk-1.md", "---\nfiles:\n  - lib.rs\n---\n# one\n"),
            ("chunk-2.md", "---\nfiles: [lib.rs]\n---\n# two\n"),
        ]);
        let db = db("gate-overlap").1;
        let sib = chunk_row(&db, &wt, "chunk-1.md");
        let err = chunk_gate(&db, &wt, "linear:A-1", "chunk-2.md", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("chunk scope gate refused chunk-2.md"), "{err}");
        // The refusal names the CONCRETE colliding paths…
        assert!(err.contains("lib.rs collides with lib.rs"), "{err}");
        // …the sibling's chunk name and roster row id…
        assert!(err.contains("chunk-1"), "{err}");
        assert!(err.contains(&format!("active row {sib}")), "{err}");
        // …and the way out.
        assert!(err.contains("--force"), "{err}");
        // A refusal must leave no row behind (the gate runs before the put).
        assert_eq!(db.list_dispatches().unwrap().len(), 1);
    }

    #[test]
    fn a_row_in_another_worktree_or_issue_is_not_a_sibling() {
        let (_d, wt) = chunk_wt(&[("c.md", "---\nfiles: [lib.rs]\n---\n")]);
        let db = db("gate-scope").1;
        // Same chunk file, same issue, DIFFERENT worktree: not a sibling.
        chunk_row(&db, "/wt/elsewhere", "c.md");
        // Same worktree, DIFFERENT issue: not a sibling either.
        put(
            &db,
            NewDispatch {
                issue_id: "linear:B-2",
                chunk_path: Some("c.md"),
                ..NewDispatch::new("linear:B-2", &wt, "coder")
            },
        )
        .unwrap();
        chunk_gate(&db, &wt, "linear:A-1", "c.md", false).unwrap();
    }

    #[test]
    fn after_is_checked_against_the_done_set_and_names_the_row() {
        let (_d, wt) = chunk_wt(&[
            ("chunk-1.md", "---\nfiles: [a.rs]\n---\n"),
            ("chunk-3.md", "---\nfiles: [b.rs]\nafter: [chunk-1]\n---\n"),
        ]);
        let db = db("gate-after").1;
        let sib = chunk_row(&db, &wt, "chunk-1.md");
        let err = chunk_gate(&db, &wt, "linear:A-1", "chunk-3.md", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("after chunk-1 is not done"), "{err}");
        // The refusal names the row holding that chunk and its status.
        assert!(err.contains(&format!("row {sib}: queued")), "{err}");

        // Done flips the after-gate open — the normal pipeline order.
        db.update_dispatch_status(sib, AgentDispatchStatus::Done)
            .unwrap();
        chunk_gate(&db, &wt, "linear:A-1", "chunk-3.md", false).unwrap();
    }

    #[test]
    fn an_unreadable_sibling_file_degrades_to_an_empty_scope() {
        let (_d, wt) = chunk_wt(&[("c.md", "---\nfiles: [lib.rs]\n---\n")]);
        let db = db("gate-broken-sib").1;
        // The sibling row points at a chunk file that is GONE from its own
        // worktree. Best-effort: the sibling contributes an empty scope (which
        // never conflicts) instead of wedging every later dispatch.
        chunk_row(&db, &wt, "missing.md");
        chunk_gate(&db, &wt, "linear:A-1", "c.md", false).unwrap();
    }

    #[test]
    fn force_overrides_a_refusal_and_skips_the_new_file_read() {
        let (_d, wt) = chunk_wt(&[("chunk-1.md", "---\nfiles: [lib.rs]\n---\n")]);
        let db = db("gate-force").1;
        chunk_row(&db, &wt, "chunk-1.md");
        // The same scope refused without --force…
        assert!(chunk_gate(&db, &wt, "linear:A-1", "chunk-1.md", false).is_err());
        // …passes under --force without reading anything: the new chunk file
        // here does not even exist, which without --force is a refusal.
        chunk_gate(&db, &wt, "linear:A-1", "no-such-chunk.md", true).unwrap();

        // Without --force a missing new chunk file names the path and the fix.
        let err = chunk_gate(&db, &wt, "linear:A-1", "no-such-chunk.md", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no-such-chunk.md"), "{err}");
        assert!(err.contains("not readable"), "{err}");

        // And an unparseable one names the offending line.
        std::fs::write(
            std::path::Path::new(&wt).join("chunk-9.md"),
            "---\nfiles: [a.rs\n---\n",
        )
        .unwrap();
        let err = chunk_gate(&db, &wt, "linear:A-1", "chunk-9.md", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a valid scope"), "{err}");
        assert!(err.contains("line 2"), "{err}");
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
            note: None,
            chunk_path: None,
            report: None,
            exit_code: None,
            exited_at_ms: None,
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

    // Symlink-at-the-artifact-path is a POSIX shape (creating one on Windows
    // needs a privilege the runners lack), so the test is a no-op there.
    #[test]
    fn the_done_gate_refuses_a_symlinked_artifact_even_when_committed() {
        if !cfg!(unix) {
            return;
        }
        // A committed *symlink* at the artifact path is `tracked` (the link
        // itself is in the index) but redirects the Lead's artifact read at
        // whatever the worker pointed it at. `exists` must follow the link's
        // metadata, not its target: the gate reads it as missing and refuses.
        let (_d, root) = git_repo("symlink");
        let path = root.join(ARTIFACT);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::platform::symlink_file(std::path::Path::new("/etc/passwd"), &path).unwrap();
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

        // Committed artifact AND a report: the gate passes. (THE-88: the
        // report is the second of the two gates; a row with an artifact
        // but no report is refused with a reason that names
        // `dispatch report`. That refusal is exercised in
        // `facts_record_report_present_and_done_gate_names_the_report_command`.)
        commit_artifact(&root);
        let mut row = row_in(&root, Some(ARTIFACT));
        row.report = Some("verdict: done".into());
        done_gate(&row).unwrap();

        // A dirty tree never blocks: reported, never gating (the tracked check
        // already holds the line).
        std::fs::write(root.join(ARTIFACT), "post-commit edit").unwrap();
        done_gate(&row).unwrap();
    }

    #[test]
    fn the_done_gate_passes_by_construction_for_a_row_without_artifact() {
        let (_d, root) = git_repo("gate-none");
        done_gate(&row_in(&root, None)).unwrap();
    }

    // --- THE-88: report / note / status verbs ------------------------------

    /// THE-88: `dispatch report <id> --text …` validates through
    /// `report_text` (empty/oversize), writes via `set_dispatch_report`,
    /// and the row is what the DB says it is. Refuses an unknown row.
    #[test]
    fn dispatch_report_writes_overwrites_and_refuses_an_unknown_row() {
        let (_d, db) = db("report-write");
        let row = put(&db, NewDispatch::new("linear:A-1", "/wt/a", "claude")).unwrap();
        // First write: stored verbatim (trimmed by the core validator).
        write_report(&db, row.id, "  verdict: done  ", false).unwrap();
        let after = db.get_dispatch(row.id).unwrap().unwrap();
        assert_eq!(after.report.as_deref(), Some("verdict: done"));

        // Second write: last write wins, the report is overwriteable.
        write_report(&db, row.id, "verdict: blocked", false).unwrap();
        let after = db.get_dispatch(row.id).unwrap().unwrap();
        assert_eq!(after.report.as_deref(), Some("verdict: blocked"));

        // Empty text is refused verbatim (the core caps, not thegn).
        let err = write_report(&db, row.id, "   ", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be empty"), "{err}");

        // Unknown row is refused by name.
        let err = write_report(&db, 99, "x", false).unwrap_err().to_string();
        assert!(err.contains("99"), "{err}");
    }

    /// THE-88: `dispatch note <id> --text …` appends and returns the note id.
    #[test]
    fn dispatch_note_appends_and_assigns_ids_in_order() {
        let (_d, db) = db("note-append");
        let row = put(&db, NewDispatch::new("linear:A-1", "/wt/a", "claude")).unwrap();
        write_note(&db, row.id, "first", false).unwrap();
        // Second append: ids are monotonic and the second text is what we wrote.
        // `write_note` validates through the same core policy as the report.
        write_note(&db, row.id, "second", false).unwrap();
        let all = db.dispatch_notes(row.id, None, 0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].text, "first");
        assert_eq!(all[1].text, "second");
        // Unknown row refused.
        let err = write_note(&db, 99, "x", false).unwrap_err().to_string();
        assert!(err.contains("99"), "{err}");
        // Empty text refused verbatim.
        let err = write_note(&db, row.id, "  ", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be empty"), "{err}");
    }

    /// THE-88: `dispatch status` filters active by default, returns the row
    /// verbatim in row mode, and composes the `digest` fact faithfully.
    #[test]
    fn dispatch_status_filters_active_and_reads_row_verbatim() {
        let (_d, db) = db("status-filter");
        let active = put(&db, NewDispatch::new("linear:A", "/wt/a", "claude")).unwrap();
        // Terminal row: present in DB but filtered out of the default read.
        let mut done = NewDispatch::new("linear:A", "/wt/a", "claude");
        done.stage = Some("review");
        let done = put(&db, done).unwrap();
        db.update_dispatch_status(done.id, AgentDispatchStatus::Done)
            .unwrap();
        // The active row gets a report and a note; the digest must carry
        // both, and the row-mode read must return the report verbatim.
        write_report(&db, active.id, "verdict: done", false).unwrap();
        write_note(&db, active.id, "first", false).unwrap();

        // List mode: only the active row (no `all`, no `row`).
        read_status(&db, None, None, false, false).unwrap();
        // Row mode: that one active row, report under `report:`.
        read_status(&db, Some(active.id), None, false, false).unwrap();
        // `--all` would include the done row, but we don't capture stdout
        // here; the contract is asserted in the digest composition below.

        // Unknown row mode is refused with the id named.
        let err = read_status(&db, Some(99), None, false, false).unwrap_err();
        assert!(err.to_string().contains("99"), "{err}");

        // The notes read is since-filtered and bounded to the newest 20;
        // the DB convention remains oldest-first within that bounded set.
        let notes = db.dispatch_notes(active.id, None, 0).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].text, "first");
    }

    /// THE-88: `verify_facts` populates `report_present` from the row's
    /// report column. The done gate's refusal text names the `dispatch
    /// report` command as the fix when artifact is set but report is empty.
    #[test]
    fn facts_record_report_present_and_done_gate_names_the_report_command() {
        let (_d, root) = git_repo("facts-report");
        commit_artifact(&root);
        // No report yet: facts reflect that, and the done gate refuses with
        // a reason that names `dispatch report` (chunk-1 added the rule;
        // chunk-2 pins the test surface).
        let mut row = row_in(&root, Some(ARTIFACT));
        let f = verify_facts(&row);
        assert!(!f.report_present, "no report on the row");
        let err = done_gate(&row).unwrap_err().to_string();
        assert!(err.contains("dispatch report"), "{err}");

        // Filing a report flips `report_present` on and the gate passes.
        row.report = Some("verdict: done".into());
        let f = verify_facts(&row);
        assert!(f.report_present);
        done_gate(&row).unwrap();

        // A blank/whitespace report is the same as none.
        row.report = Some("   \n  ".into());
        let f = verify_facts(&row);
        assert!(!f.report_present, "whitespace-only is not present");
    }

    /// THE-88: `row_report_artifact_db` returns the row's `report` and
    /// `artifact_path` after a wake, and `(None, None)` for a reaped row.
    #[test]
    fn row_report_artifact_reads_the_row_and_tolerates_a_miss() {
        let (_d, db) = db("wake-row");
        let row = put(
            &db,
            NewDispatch {
                artifact_path: Some(".thegn/pipeline/A/1.md"),
                ..NewDispatch::new("linear:A", "/wt/a", "claude")
            },
        )
        .unwrap();
        // Initial: report absent, artifact path present from the put.
        assert_eq!(
            row_report_artifact_db(&db, row.id).unwrap(),
            (None, Some(".thegn/pipeline/A/1.md".to_string()))
        );
        // Worker files a report: both fields populated.
        write_report(&db, row.id, "verdict: done", false).unwrap();
        assert_eq!(
            row_report_artifact_db(&db, row.id).unwrap(),
            (
                Some("verdict: done".to_string()),
                Some(".thegn/pipeline/A/1.md".to_string())
            )
        );
        // Missing row: reaped mid-wake, both nulls, no error.
        assert_eq!(
            row_report_artifact_db(&db, 9_999_999).unwrap(),
            (None, None)
        );
    }

    #[test]
    fn wait_only_treats_a_control_404_as_a_gone_wake() {
        let not_found = anyhow::Error::new(thegn_svc::control::client::ControlRequestError::new(
            404,
            "not found: session s1",
        ));
        assert_eq!(
            classify_wait_outcome(Err(not_found)).unwrap(),
            (true, None, true)
        );

        let transport = anyhow::anyhow!("connect control endpoint: connection reset");
        let err = classify_wait_outcome(Err(transport)).unwrap_err();
        assert!(err.to_string().contains("connection reset"), "{err}");

        let forbidden = anyhow::Error::new(thegn_svc::control::client::ControlRequestError::new(
            403,
            "missing required scope",
        ));
        assert!(classify_wait_outcome(Err(forbidden)).is_err());
    }

    #[test]
    fn wake_response_and_db_read_errors_are_not_timeouts_or_missing_rows() {
        let err = classify_wait_outcome(Ok(serde_json::json!({"exit_code": 0}))).unwrap_err();
        assert!(err.to_string().contains("malformed wait response"), "{err}");
        assert!(
            classify_wait_outcome(Ok(serde_json::json!({
                "matched": true,
                "exit_code": "0"
            })))
            .is_err()
        );

        let err =
            row_report_artifact_from(Err(anyhow::anyhow!("database is corrupt"))).unwrap_err();
        assert!(err.to_string().contains("database is corrupt"), "{err}");
    }
}
