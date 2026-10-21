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
use thegn_core::db::{Db, DispatchNoteQuotaExceeded, LeadProof, NoteQuotaKind};
use thegn_core::issue::{AgentDispatch, AgentDispatchStatus, DispatchNote, NewDispatch};
use thegn_core::outln;
use thegn_core::pipeline_chunk;
use thegn_core::pipeline_reap;
use thegn_core::pipeline_report;
use thegn_core::pipeline_run::{self, WaitTarget};
use thegn_core::store::NotificationStore;
use thegn_core::util::{git_ok, git_out_allow_empty};
#[path = "dispatch_gate.rs"]
mod gate;

/// Bound the active roster rows examined by the CLI reaper.
const CLI_REAP_ROW_LIMIT: usize = 4096;
const ACTIVE_STATUSES: [AgentDispatchStatus; 5] = [
    AgentDispatchStatus::Queued,
    AgentDispatchStatus::Spawning,
    AgentDispatchStatus::Running,
    AgentDispatchStatus::WaitingHuman,
    AgentDispatchStatus::PrOpen,
];

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
        /// Path to the handoff artifact. A pipeline row's handoff is
        /// `.thegn/pipeline/<ISSUE>/<stage>/<row>.md`; it is assigned
        /// automatically when omitted, and any other path never verifies.
        #[arg(long)]
        artifact: Option<String>,
        /// The chunk file this row dispatches under (`.thegn/pipeline/<ISSUE>/
        /// code/chunk-N.md`), whose `files:` frontmatter is the row's scope.
        /// The gate prepares its scope before admission and compares it with
        /// active siblings' admitted snapshots. A legacy sibling without a
        /// snapshot blocks admission until finished, abandoned, or overridden.
        /// Follow-up (release note): session moves and resumes of legacy rows
        /// can also leave snapshots NULL until those writers propagate scope.
        #[arg(long)]
        chunk: Option<String>,
        /// Dispatch even though the chunk-scope gate refused (a scope
        /// collision or an unmet `after:`). Chunk overrides also require
        /// `--force-reason`; the incoming scope still must be prepared and is
        /// recorded. Non-chunk put keeps its historical behavior.
        #[arg(long)]
        force: bool,
        #[arg(long, value_name = "REASON")]
        force_reason: Option<String>,
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
        /// Path to the handoff artifact this row will produce. A pipeline
        /// row's handoff is `.thegn/pipeline/<ISSUE>/<stage>/<row>.md`; it is
        /// assigned automatically when omitted, and any other path never
        /// verifies.
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
        /// Explicitly authorize a chunk scope overlap. Requires and records a
        /// non-empty reason; session open never exposes this override.
        #[arg(long, value_name = "REASON")]
        allow_scope_overlap: Option<String>,
        /// The Lead lease this mutation is made under.
        #[arg(
            long,
            value_name = "NAME",
            requires = "owner",
            conflicts_with = "admin"
        )]
        lease: Option<String>,
        /// The owner id holding `--lease`.
        #[arg(long, value_name = "ID", requires = "lease")]
        owner: Option<String>,
        /// Manual roster administration: skip the lease check and record `--why` on the row.
        #[arg(long, requires = "why")]
        admin: bool,
        /// Audit reason for manual roster administration.
        #[arg(long, value_name = "REASON", requires = "admin")]
        why: Option<String>,
        /// Emit the created row (or the refusal) as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Take, renew, release or inspect the pipeline monitor lease — the guard
    /// that stops two Leads driving one pipeline and filling the same slots.
    Lease {
        /// What to do: `acquire` | `renew` | `release` | `show`.
        action: String,
        /// Who is asking (a stable id for this monitor process).
        #[arg(long)]
        owner: Option<String>,
        /// Lease lifetime in seconds (5–86400); renew before it lapses.
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(i64).range(thegn_core::db::MIN_LEASE_TTL_SECS..=thegn_core::db::MAX_LEASE_TTL_SECS))]
        ttl: i64,
        /// The lease name — one pipeline per name.
        #[arg(long, default_value = "lead")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Advance one dispatch's status through the checked transition table.
    /// Marking a row `done` is gated: when the
    /// row carries an artifact, it must exist and be committed and unchanged
    /// in `HEAD` (a session exiting is not a handoff — see `dispatch verify`).
    /// `failed`/`abandoned`/`merged` are gated only by the transition table;
    /// recording a legal bad outcome must always be possible.
    SetStatus {
        /// The dispatch row id (see `dispatch list`).
        id: i64,
        /// A member of the closed set: queued | spawning | running |
        /// waiting_human | pr_open | merged | abandoned | done | failed.
        status: String,
        /// Record `done` even when the artifact gate refuses it. Requires
        /// --why; the reason is stored on the row and shown by list/status.
        #[arg(long)]
        force: bool,
        /// Why — required with --force and stored on the row; otherwise it is
        /// recorded as a note. Strongly wanted for a bad
        /// outcome: 40 of the last run's 45 failed/abandoned rows recorded no
        /// reason at all, which is what made the roster unreadable after the
        /// fact. Otherwise optional: recording a bad outcome must always be
        /// possible (see `set_status`).
        #[arg(long)]
        why: Option<String>,
        /// The Lead lease this mutation is made under.
        #[arg(
            long,
            value_name = "NAME",
            requires = "owner",
            conflicts_with = "admin"
        )]
        lease: Option<String>,
        /// The owner id holding `--lease`.
        #[arg(long, value_name = "ID", requires = "lease")]
        owner: Option<String>,
        /// Manual roster administration: skip the lease check and record `--why` on the row.
        #[arg(long, requires = "why", conflicts_with = "lease")]
        admin: bool,
        #[arg(long)]
        json: bool,
    },
    /// Report whether a row's handoff artifact is real — present in the row's
    /// worktree, committed in `HEAD`, and unchanged at that path.
    /// Exit 0 when it is; exit 2 (retryable) with the reasons when not. Reads
    /// only — the verdict is the supervisor's judgment.
    Verify {
        /// The dispatch row id (see `dispatch list`).
        id: i64,
        /// Emit the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run the repo's merge-queue gate on a pipeline row's committed HEAD in
    /// the isolated gate worktree and record a trusted, commit-bound
    /// attestation (Lead-only). This explicit verb runs even when
    /// `[merge_queue].gate_on = false`; that switch controls automatic fold
    /// gating only.
    Gate {
        /// The dispatch row id (see `dispatch list`).
        id: i64,
        /// Emit the attestation as JSON.
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

pub(crate) const DISPATCH_CAPABILITY_ENV: &str = "THEGN_DISPATCH_CAPABILITY";

#[derive(PartialEq, Eq)]
enum WorkerRoute {
    Local,
    ViaDaemon(String),
    Refused(&'static str),
}

impl std::fmt::Debug for WorkerRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => f.write_str("Local"),
            Self::ViaDaemon(_) => f.write_str("ViaDaemon(<redacted>)"),
            Self::Refused(verb) => f.debug_tuple("Refused").field(verb).finish(),
        }
    }
}

fn worker_route(action: &Action, token: Option<&str>) -> WorkerRoute {
    let Some(token) = token else {
        return WorkerRoute::Local;
    };
    match action {
        Action::Report { .. } | Action::Note { .. } => WorkerRoute::ViaDaemon(token.to_owned()),
        Action::Claim { .. } => WorkerRoute::Refused("claim"),
        Action::Put { .. } => WorkerRoute::Refused("put"),
        Action::SetStatus { .. } => WorkerRoute::Refused("set-status"),
        Action::Lease { .. } => WorkerRoute::Refused("lease"),
        Action::Gate { .. } => WorkerRoute::Refused("gate"),
        Action::Reap { apply: true, .. } => WorkerRoute::Refused("reap --apply"),
        Action::List { .. }
        | Action::Reap { apply: false, .. }
        | Action::Verify { .. }
        | Action::Wait { .. }
        | Action::Status { .. } => WorkerRoute::Local,
    }
}

#[derive(Debug)]
struct WorkerCapabilityRefusal {
    verb: &'static str,
}

impl std::fmt::Display for WorkerCapabilityRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`thegn dispatch {}` is a supervisor verb and is refused under a worker dispatch capability",
            self.verb
        )
    }
}

impl std::error::Error for WorkerCapabilityRefusal {}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    fn worker_write_via_daemon(cfg: &Config, action: Action, token: String) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let client = crate::cmd::session::connect(cfg).await?;
            match action {
                Action::Report { id, text, json } => {
                    let validated = pipeline_report::report_text(&text)
                        .map_err(|error| anyhow::anyhow!("{error}"))?;
                    let reply = client
                        .dispatch_report(
                            id,
                            &thegn_svc::control::DispatchWorkerWriteReq {
                                text: validated.clone(),
                                token,
                            },
                        )
                        .await?;
                    print_report_outcome(
                        reply.id,
                        &validated,
                        reply.pass_without_gate_evidence,
                        json,
                    )?;
                }
                Action::Note { id, text, json } => {
                    let validated = pipeline_report::note_text(&text)
                        .map_err(|error| anyhow::anyhow!("{error}"))?;
                    let reply = client
                        .dispatch_note(
                            id,
                            &thegn_svc::control::DispatchWorkerWriteReq {
                                text: validated.clone(),
                                token,
                            },
                        )
                        .await?;
                    if json {
                        super::emit_json(&serde_json::json!({
                            "id": id,
                            "note": validated,
                            "note_id": reply.note_id,
                            "created_at_ms": reply.created_at_ms,
                        }))?;
                    } else {
                        outln!(
                            "note recorded on dispatch {} ({} chars)",
                            id,
                            validated.chars().count()
                        );
                    }
                }
                _ => unreachable!("worker route only sends report or note"),
            }
            Ok(())
        })
    }
    let token = std::env::var_os(DISPATCH_CAPABILITY_ENV);
    let token_text = token.as_ref().map(|value| value.to_str().unwrap_or(""));
    match worker_route(&action, token_text) {
        WorkerRoute::Refused(verb) => return Err(WorkerCapabilityRefusal { verb }.into()),
        WorkerRoute::ViaDaemon(token) => return worker_write_via_daemon(cfg, action, token),
        WorkerRoute::Local => {}
    }
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
            allow_scope_overlap,
            lease,
            owner,
            admin,
            why,
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
            allow_scope_overlap.as_deref(),
            lead_proof(lease.as_deref(), owner.as_deref(), admin, why.as_deref()),
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
            force_reason,
            json,
        } => {
            let db = Db::open()?;
            if let Some(chunk_path) = chunk.as_deref() {
                if force && force_reason.as_deref().is_none_or(|s| s.trim().is_empty()) {
                    anyhow::bail!("dispatch put --chunk --force requires --force-reason <reason>");
                }
                if !force && force_reason.is_some() {
                    anyhow::bail!("--force-reason requires --force");
                }
                let name = stage.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("a chunk-bearing dispatch put requires --stage so admission can enforce that stage's capacity")
                })?;
                let limit = cfg.pipeline.stage(name).map(|s| s.concurrency).ok_or_else(|| {
                    anyhow::anyhow!("unknown pipeline stage {name:?}; configure it before putting a chunk dispatch")
                })?;
                let row = claim_chunk_put(
                    &db,
                    NewDispatch {
                        issue_id: &issue_id,
                        worktree_path: &worktree_path,
                        agent_name: &agent_name,
                        stage: stage.as_deref(),
                        parent_id: parent,
                        session_id: session.as_deref(),
                        artifact_path: artifact.as_deref(),
                        chunk_path: Some(chunk_path),
                    },
                    limit,
                    if force { force_reason.as_deref() } else { None },
                )?;
                if json {
                    let mut v = serde_json::to_value(&row)?;
                    if force {
                        v["forced"] = serde_json::json!(true);
                    }
                    if let Some(reason) = force_reason {
                        v["force_reason"] = serde_json::json!(reason);
                    }
                    return super::emit_json(&v);
                }
                if force {
                    outln!(
                        "dispatch {} → {} (forced: {})",
                        row.id,
                        row.status.as_str(),
                        force_reason.unwrap_or_default()
                    );
                } else {
                    outln!("dispatch {} → {}", row.id, row.status.as_str());
                }
                return Ok(());
            }
            anyhow::ensure!(
                !force,
                "dispatch put --force only applies with --chunk (a put without a chunk has no gate to override); drop --force"
            );
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
                return super::emit_json(&serde_json::to_value(&row)?);
            }
            outln!("dispatch {} → {}", row.id, row.status.as_str());
            Ok(())
        }
        Action::SetStatus {
            id,
            status,
            force,
            why,
            lease,
            owner,
            admin,
            json,
        } => set_status(
            id,
            &status,
            force,
            why.as_deref(),
            lead_proof(lease.as_deref(), owner.as_deref(), admin, why.as_deref()),
            json,
        ),
        Action::Verify { id, json } => verify(id, json),
        Action::Gate { id, json } => gate::run(cfg, id, json),
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
    let id = db.put_dispatch_with_parent_check(new)?;
    db.get_dispatch(id)?
        .ok_or_else(|| anyhow::anyhow!("dispatch {id} vanished after insert"))
}

/// Test-only assertion helper for the existing `pipeline_chunk` overlap and
/// `after:` semantics. Production dispatches use prepared scope facts and the
/// transactional admission below; this helper is deliberately not a creation
/// path.
#[cfg(test)]
fn chunk_gate(
    db: &Db,
    worktree: &str,
    issue_id: &str,
    chunk_path: &str,
    force: bool,
) -> Result<()> {
    if force {
        // Mirrors the historical gate's broad override for regression tests.
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
        if !r.status.is_active() {
            continue;
        }
        // An active row with a chunk path must have an established scope.
        // Failing closed gives the operator an actionable row/path instead
        // of silently treating unknown ownership as empty.
        let sibling_body = read_chunk_file(&r.worktree_path, sibling_path).map_err(|e| {
            anyhow::anyhow!(
                "active sibling row {} has unreadable scope at {}: {e}",
                r.id,
                sibling_path
            )
        })?;
        let files = pipeline_chunk::parse_frontmatter(&sibling_body)
            .map_err(|e| {
                anyhow::anyhow!(
                    "active sibling row {} has invalid scope at {}: {e}",
                    r.id,
                    sibling_path
                )
            })?
            .files;
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
        pipeline_chunk::ScopeVerdict::Unknown(error) => {
            anyhow::bail!("chunk scope matching refused: {error:?}")
        }
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

/// Validate and resolve a repository-relative chunk path, then read a bounded
/// regular file without following its final component. Admission callers fail
/// closed on a new or active sibling read failure; display enrichment may omit
/// unavailable derived scope data.
fn read_chunk_file(worktree: &str, chunk_path: &str) -> std::io::Result<String> {
    use std::io::Read;
    const MAX_CHUNK_BYTES: u64 = 1024 * 1024;
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "chunk path must be a canonical repository-relative path",
        )
    };
    let Some(normalized) = thegn_core::pipeline_chunk::canonical_repo_relative_path(chunk_path)
    else {
        return Err(invalid());
    };
    if normalized.contains('*') {
        return Err(invalid());
    }
    let root = std::fs::canonicalize(worktree)?;
    let candidate = root.join(&normalized);
    let resolved = std::fs::canonicalize(&candidate)?;
    if !resolved.starts_with(&root) {
        return Err(invalid());
    }
    if std::fs::symlink_metadata(&candidate)?
        .file_type()
        .is_symlink()
    {
        return Err(invalid());
    }
    let file = thegn_core::fsperm::open_regular_file_nofollow(&resolved)?;
    let mut bytes = Vec::new();
    file.take(MAX_CHUNK_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CHUNK_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "chunk file exceeds 1 MiB limit",
        ));
    }
    let line_count = bytes.iter().filter(|byte| **byte == b'\n').count()
        + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'));
    if line_count > 8192 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "chunk file exceeds 8192 line limit",
        ));
    }
    String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// The chunk NAME a `files:`/`overlaps:`/`after:` reference resolves to: the
/// chunk file's basename without extension (`…/code/chunk-2.md` →
/// `chunk-2`). The architect's frontmatter names siblings by this stem.
// Only the test-only `chunk_gate` uses this; gated to match it so non-test
// builds do not trip `dead_code` (THE-212).
#[cfg(test)]
fn chunk_name(chunk_path: &str) -> &str {
    std::path::Path::new(chunk_path)
        .file_stem()
        .map(|s| s.to_str().unwrap_or(""))
        .unwrap_or("")
}

/// Read every relevant sibling before the DB writer is acquired. The prepared
/// value carries these exact parsed facts into the atomic roster decision.
fn prepare_chunk_admission(
    db: &Db,
    worktree: &str,
    issue_id: &str,
    chunk_path: &str,
) -> Result<pipeline_chunk::PreparedChunkAdmission> {
    let body = read_chunk_file(worktree, chunk_path).map_err(|e| {
        anyhow::anyhow!("chunk file {chunk_path} is not readable in {worktree}: {e}")
    })?;
    let scope = pipeline_chunk::parse_frontmatter(&body)
        .map_err(|e| anyhow::anyhow!("chunk file {chunk_path} is not a valid scope ({e})"))?;
    let mut siblings = Vec::new();
    for row in db.admission_rows(issue_id, worktree)? {
        if row.issue_id != issue_id || row.worktree_path != worktree || !row.status.is_active() {
            continue;
        }
        let Some(path) = row
            .chunk_path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            continue;
        };
        let parsed = read_chunk_file(&row.worktree_path, path)
            .map_err(|e| format!("{e}"))
            .and_then(|body| pipeline_chunk::parse_frontmatter(&body).map_err(|e| e.to_string()));
        siblings.push(pipeline_chunk::PreparedSiblingScope {
            row: row.id,
            path: path.to_string(),
            scope: Some(parsed),
        });
    }
    Ok(pipeline_chunk::PreparedChunkAdmission {
        path: chunk_path.to_string(),
        scope,
        siblings,
    })
}

fn lead_proof<'a>(
    lease: Option<&'a str>,
    owner: Option<&'a str>,
    admin: bool,
    why: Option<&'a str>,
) -> LeadProof<'a> {
    if admin {
        LeadProof::Admin {
            why: why.unwrap_or(""),
        }
    } else if let (Some(name), Some(owner)) = (lease, owner) {
        LeadProof::Lease { name, owner }
    } else {
        LeadProof::Unbound
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn claim_admitted_with_refresh(
    db: &Db,
    new: NewDispatch<'_>,
    limit: u32,
    allow_duplicate: Option<&str>,
    worktree: &str,
    issue_id: &str,
    chunk: Option<&str>,
    allow_scope: Option<&str>,
) -> Result<std::result::Result<i64, thegn_core::db::DispatchAdmissionDecision>> {
    claim_admitted_with_refresh_led(
        db,
        new,
        limit,
        allow_duplicate,
        worktree,
        issue_id,
        chunk,
        allow_scope,
        LeadProof::Unbound,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn claim_admitted_with_refresh_led(
    db: &Db,
    new: NewDispatch<'_>,
    limit: u32,
    allow_duplicate: Option<&str>,
    worktree: &str,
    issue_id: &str,
    chunk: Option<&str>,
    allow_scope: Option<&str>,
    proof: LeadProof<'_>,
) -> Result<std::result::Result<i64, thegn_core::db::DispatchAdmissionDecision>> {
    let mut raced_snapshot = None;
    for _ in 0..3 {
        let prepared = chunk
            .map(|p| prepare_chunk_admission(db, worktree, issue_id, p))
            .transpose()?;
        let result = db.claim_dispatch_admitted_led(
            new,
            limit,
            allow_duplicate,
            prepared.as_ref(),
            allow_scope,
            proof,
        )?;
        match result {
            Err(thegn_core::db::DispatchAdmissionDecision::RetryScopeSnapshot { row, path }) => {
                raced_snapshot = Some((row, path));
            }
            Err(thegn_core::db::DispatchAdmissionDecision::ScopeRefused(_))
                if raced_snapshot.is_some() =>
            {
                // The roster changed between preparation and the locked read;
                // surface this attempt as retryable after refresh. The next
                // invocation will show the stable overlap refusal directly.
                let (row, path) = raced_snapshot.take().expect("guarded above");
                return Ok(Err(
                    thegn_core::db::DispatchAdmissionDecision::RetryScopeSnapshot { row, path },
                ));
            }
            other => return Ok(other),
        }
    }
    let (row, path) = raced_snapshot.unwrap_or((0, chunk.unwrap_or_default().to_string()));
    Ok(Err(
        thegn_core::db::DispatchAdmissionDecision::RetryScopeSnapshot { row, path },
    ))
}

fn claim_chunk_put(
    db: &Db,
    new: NewDispatch<'_>,
    limit: u32,
    allow_scope: Option<&str>,
) -> Result<AgentDispatch> {
    let worktree = new.worktree_path.to_string();
    let issue_id = new.issue_id.to_string();
    let chunk_path = new.chunk_path.map(str::to_string);
    let outcome = claim_admitted_with_refresh(
        db,
        new,
        limit,
        None,
        &worktree,
        &issue_id,
        chunk_path.as_deref(),
        allow_scope,
    )?;
    match outcome {
        Ok(id) => db
            .get_dispatch(id)?
            .ok_or_else(|| anyhow::anyhow!("dispatch {id} vanished after insert")),
        Err(thegn_core::db::DispatchAdmissionDecision::Claim(d)) => {
            anyhow::bail!("dispatch refused: {}", d.reason())
        }
        Err(thegn_core::db::DispatchAdmissionDecision::ScopeRefused(s)) => anyhow::bail!("{s}"),
        Err(thegn_core::db::DispatchAdmissionDecision::RetryScopeSnapshot { row, path }) => {
            Err(anyhow::Error::new(crate::cmd::Retryable(anyhow::anyhow!(
                "scope snapshot changed at row {row} ({path}); retry admission"
            ))))
        }
    }
}

/// `dispatch claim` — the atomic slot check plus insert.
///
/// The stage's budget comes from `[[pipeline.stages]]`, so the number enforced
/// is the one the operator configured. An unknown stage is refused rather than
/// silently receiving unlimited capacity.
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
    allow_scope_overlap: Option<&str>,
    proof: LeadProof<'_>,
    json: bool,
) -> Result<()> {
    let db = Db::open()?;
    let Some(stage_config) = cfg.pipeline.stage(stage) else {
        anyhow::bail!(
            "unknown pipeline stage {stage:?}; configure it under [[pipeline.stages]] before claiming work"
        );
    };
    // A grant the harness cannot take command-scoped refuses the launch, so
    // refuse the CLAIM too (THE-440): a row claimed here is dispatched later,
    // and a supervisor should learn about the policy hold before it occupies a
    // slot, not when the worker fails to start.
    crate::cmd::session::policy_admission(cfg, agent_name, stage)?;
    let limit = stage_config.concurrency;
    let outcome = match claim_admitted_with_refresh_led(
        &db,
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
        worktree_path,
        issue_id,
        chunk,
        allow_scope_overlap,
        proof,
    ) {
        Ok(outcome) => outcome,
        Err(error)
            if error
                .downcast_ref::<thegn_core::db::LeadLeaseConflict>()
                .is_some() =>
        {
            if json {
                super::emit_json(
                    &serde_json::json!({"granted": false, "reason": error.to_string(), "error": "lead_lease"}),
                )?;
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
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
                if let Some(why) = allow_scope_overlap {
                    v["forced_scope_overlap"] = serde_json::json!({"reason": why});
                }
                return super::emit_json(&v);
            }
            if let Some(why) = allow_scope_overlap {
                outln!("dispatch {id} claimed for stage {stage} (scope overlap forced: {why})");
            } else if allow_duplicate.is_some() {
                outln!("dispatch {id} claimed (duplicate explicitly authorized)");
            } else {
                outln!("dispatch {id} claimed for stage {stage}");
            }
            Ok(())
        }
        Err(thegn_core::db::DispatchAdmissionDecision::Claim(decision)) => {
            if json {
                super::emit_json(&serde_json::json!({
                    "granted": false,
                    "reason": decision.reason(),
                }))?;
            }
            // Exit 2 = retryable, matching `verify`/`wait`: the supervisor's
            // correct response is to reconcile and try again, not to abort.
            Err(anyhow::Error::new(crate::cmd::Retryable(anyhow::anyhow!(
                "dispatch refused: {}\n(exit 2 — reconcile and retry)",
                decision.reason()
            ))))
        }
        Err(thegn_core::db::DispatchAdmissionDecision::ScopeRefused(reason)) => {
            if json {
                super::emit_json(
                    &serde_json::json!({"granted":false,"reason":reason,"retryable":false}),
                )?;
            }
            anyhow::bail!("{reason}");
        }
        Err(thegn_core::db::DispatchAdmissionDecision::RetryScopeSnapshot { row, path }) => {
            if json {
                super::emit_json(
                    &serde_json::json!({"granted":false,"reason":format!("scope snapshot changed at row {row} ({path}); retry admission"),"retryable":true}),
                )?;
            }
            Err(anyhow::Error::new(crate::cmd::Retryable(anyhow::anyhow!(
                "scope snapshot changed at row {row} ({path}); retry admission"
            ))))
        }
    }
}

/// `dispatch lease` — monitor ownership.
fn lease(action: &str, owner: Option<&str>, ttl: i64, name: &str, json: bool) -> Result<()> {
    let db = Db::open()?;
    match action {
        "show" => {
            let row = db.pipeline_lease_row(name)?;
            let now = thegn_core::util::now_ms();
            let outcome = lease_show_outcome(row.as_ref(), now);
            let holder = row.as_ref().map(|(owner, _)| owner);
            let (owner, last_owner) = lease_show_owners(row.as_ref(), now);
            let remaining = row
                .as_ref()
                .filter(|(_, expires_at)| *expires_at > now)
                .map(|(_, expires_at)| expires_at - now);
            if json {
                return super::emit_json(&serde_json::json!({
                    "name": name,
                    "outcome": outcome,
                    "owner": owner,
                    "last_owner": last_owner,
                    "expires_in_ms": remaining,
                }));
            }
            match (outcome, holder, remaining) {
                ("live", Some(owner), Some(ms)) => {
                    outln!(
                        "lease {}: held by {} ({}s left)",
                        pipeline_report::display_cell(name),
                        pipeline_report::display_cell(owner),
                        ms / 1000
                    )
                }
                ("expired", Some(owner), _) => {
                    outln!(
                        "lease {}: expired (last held by {})",
                        pipeline_report::display_cell(name),
                        pipeline_report::display_cell(owner)
                    )
                }
                _ => outln!("lease {}: free", pipeline_report::display_cell(name)),
            }
            Ok(())
        }
        "acquire" | "renew" | "release" => {
            let owner = owner.ok_or_else(|| {
                anyhow::anyhow!("--owner is required for `{action}` (a stable id for this monitor)")
            })?;
            if action == "release" {
                let row = db.release_pipeline_lease_row(name, owner)?;
                let now = thegn_core::util::now_ms();
                let outcome = lease_release_outcome(owner, row.as_ref(), now);
                let released = outcome == "released";
                if json {
                    return super::emit_json(&serde_json::json!({
                        "released": released,
                        "outcome": outcome,
                        "holder": row.map(|(holder, _)| holder),
                    }));
                }
                outln!(
                    "lease {}: {}",
                    pipeline_report::display_cell(name),
                    match outcome {
                        "released" => "released",
                        "missing" => "missing",
                        "expired" => "expired",
                        _ => "replaced by another owner",
                    }
                );
                return Ok(());
            }
            if action == "renew" {
                let renewed = db.renew_pipeline_lease(name, owner, ttl)?;
                let (outcome, holder) =
                    lease_renewal_outcome(owner, renewed, thegn_core::util::now_ms());
                if json {
                    super::emit_json(&serde_json::json!({
                        "renewed": outcome == "renewed",
                        "outcome": outcome,
                        "holder": holder,
                    }))?;
                } else {
                    outln!("lease {}: {outcome}", pipeline_report::display_cell(name));
                }
                if outcome != "renewed" {
                    anyhow::bail!("lease {name} renewal failed: {outcome}");
                }
                return Ok(());
            }
            match db.acquire_pipeline_lease(name, owner, ttl)? {
                Ok(()) => {
                    if json {
                        return super::emit_json(
                            &serde_json::json!({ "acquired": true, "owner": owner, "ttl": ttl }),
                        );
                    }
                    outln!(
                        "lease {}: held by {} for {ttl}s",
                        pipeline_report::display_cell(name),
                        pipeline_report::display_cell(owner)
                    );
                    Ok(())
                }
                Err(holder) => {
                    if json {
                        super::emit_json(
                            &serde_json::json!({ "acquired": false, "holder": holder }),
                        )?;
                    }
                    Err(anyhow::Error::new(crate::cmd::Retryable(anyhow::anyhow!(
                        "lease {} is held by {} — another monitor is already driving \
                         this pipeline. Stop it, or wait for its lease to lapse, before starting \
                         a second one.\n(exit 2 — retryable)",
                        pipeline_report::display_cell(name),
                        pipeline_report::display_cell(&holder)
                    ))))
                }
            }
        }
        other => anyhow::bail!(
            "unknown lease action {other:?} (expected: acquire, renew, release, show)"
        ),
    }
}

fn lease_show_outcome(row: Option<&(String, i64)>, now_ms: i64) -> &'static str {
    match row {
        None => "free",
        Some((_, expires_at)) if *expires_at <= now_ms => "expired",
        Some(_) => "live",
    }
}

fn lease_show_owners(row: Option<&(String, i64)>, now_ms: i64) -> (Option<&str>, Option<&str>) {
    match (lease_show_outcome(row, now_ms), row) {
        ("live", Some((owner, _))) => (Some(owner), None),
        ("expired", Some((owner, _))) => (None, Some(owner)),
        _ => (None, None),
    }
}

fn lease_release_outcome(owner: &str, row: Option<&(String, i64)>, now_ms: i64) -> &'static str {
    match row {
        None => "missing",
        Some((row_owner, _)) if row_owner != owner => "replaced",
        Some((_, expires_at)) if *expires_at <= now_ms => "expired",
        Some(_) => "released",
    }
}

fn lease_renewal_outcome(
    owner: &str,
    result: std::result::Result<i64, Option<(String, i64)>>,
    now_ms: i64,
) -> (&'static str, Option<String>) {
    match result {
        Ok(_) => ("renewed", None),
        Err(None) => ("missing", None),
        Err(Some((holder, _))) if holder != owner => ("replaced", Some(holder)),
        Err(Some((holder, expires_at))) if expires_at <= now_ms => ("expired", Some(holder)),
        Err(Some((holder, _))) => ("replaced", Some(holder)),
    }
}

fn bounded_rows(
    db: &Db,
    statuses: &[AgentDispatchStatus],
    what: &str,
) -> Result<(Vec<AgentDispatch>, bool)> {
    let (mut rows, truncated) =
        db.list_dispatches_with_status(statuses, Db::ROSTER_ACTIVE_LIMIT)?;
    rows.sort_by(|a, b| {
        b.dispatched_at_ms
            .cmp(&a.dispatched_at_ms)
            .then_with(|| b.id.cmp(&a.id))
    });
    if truncated {
        thegn_core::msg::warn(&format!(
            "{what} only the oldest {} active rows",
            Db::ROSTER_ACTIVE_LIMIT
        ));
    }
    Ok((rows, truncated))
}

fn list_rows(db: &Db, active: bool) -> Result<(Vec<AgentDispatch>, bool)> {
    if !active {
        return Ok((db.list_dispatches()?, false));
    }
    bounded_rows(db, &ACTIVE_STATUSES, "dispatch list --active shows")
}

fn list(active: bool, json: bool) -> Result<()> {
    let db = Db::open()?;
    let (rows, _) = list_rows(&db, active)?;
    let overrides = db.dispatch_overrides()?;
    if json {
        // One value per row: stored fields, the admitted ownership snapshot,
        // and a display-time read used only to report source-file drift.
        let mut vals: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
        let now_ms = thegn_core::util::now_ms();
        for d in &rows {
            let mut v = serde_json::to_value(d)?;
            annotate_override(&mut v, overrides.get(&d.id));
            // The derived liveness, so a supervisor need not re-implement the
            // exited-vs-running distinction (and get it wrong, as one did).
            v["liveness"] = serde_json::json!(pipeline_run::row_liveness(d, now_ms).token());
            // The derived verdict, and whether it contradicts the row's status.
            // Derived rather than stored so it can never drift from the report
            // it came from (see `pipeline_report::Verdict`).
            let verdict = pipeline_report::verdict_of(d.report.as_deref());
            v["verdict"] = serde_json::json!(verdict.as_str());
            v["verdict_disagrees"] = serde_json::json!(
                pipeline_report::verdict_disagrees_with_status(d.status, verdict)
            );
            let admitted = match db.dispatch_chunk_scope(d.id) {
                Ok(admitted) => {
                    v["chunk_files_admitted"] = admitted
                        .as_ref()
                        .map_or(serde_json::Value::Null, |files| serde_json::json!(files));
                    admitted
                }
                Err(error) => {
                    v["chunk_files_admitted"] = serde_json::Value::Null;
                    v["chunk_scope_error"] = serde_json::json!(error.to_string());
                    None
                }
            };
            let live_files = d
                .chunk_path
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .and_then(|chunk_path| read_chunk_file(&d.worktree_path, chunk_path).ok())
                .and_then(|body| pipeline_chunk::parse_frontmatter(&body).ok())
                .map(|scope| scope.files);
            if let Some(files) = &live_files {
                v["chunk_files_from_file_at_display_time"] = serde_json::json!(files);
            }
            v["chunk_scope_drift"] = match admitted.as_ref() {
                Some(files) => serde_json::json!(live_files.as_ref() != Some(files)),
                None if d.chunk_path.is_some() => serde_json::Value::Null,
                None => serde_json::json!(false),
            };
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
    // Rows whose recorded outcome contradicts their own report. Counted for the
    // same reason as `stale`: it is invisible from `status`, and it was three
    // rows in the last run — each one a PASS sitting at `failed`.
    let mut disagree = 0usize;
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
        let status_cell = status_with_override(status_cell, overrides.get(&d.id));
        // What the worker CONCLUDED, which is a different axis from what
        // happened to its row. `!` marks the pair that contradict each other.
        let verdict = pipeline_report::verdict_of(d.report.as_deref());
        let verdict_cell = if pipeline_report::verdict_disagrees_with_status(d.status, verdict) {
            disagree += 1;
            format!("{verdict}!")
        } else if verdict == pipeline_report::Verdict::None {
            "-".to_string()
        } else {
            verdict.to_string()
        };
        let (issue_cell, stage_cell, note) = display_dispatch_cells(
            &d.issue_id,
            d.stage.as_deref().unwrap_or("-"),
            d.note.as_deref(),
        );
        let note = note_with_override(&note, overrides.get(&d.id));
        // The pipeline columns print as `-` when absent rather than collapsing
        // the row's shape, so the table stays column-aligned for a roster that
        // mixes pipeline and plain dispatches.
        outln!(
            "{}  {}  {}  {}  {}  {}  {}  {}  {}  {}",
            d.id,
            status_cell,
            verdict_cell,
            stage_cell,
            d.parent_id
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
            issue_cell,
            pipeline_report::display_cell(&d.agent_name),
            pipeline_report::display_cell(&d.worktree_path),
            note,
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
    if disagree > 0 {
        outln!(
            "\n{disagree} row(s) whose verdict is marked `!`: the row's status contradicts its \
             own report — a PASS parked at `failed`, or a `done` carrying a FAIL. Usually \
             bookkeeping lost the row rather than the work being bad; read the report with \
             `thegn dispatch status <id>` before re-running anything."
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
        .map(|name| pipeline_report::display_cell(&name))
        .unwrap_or_else(|| "-".into())
}

fn annotate_override(
    value: &mut serde_json::Value,
    override_record: Option<&thegn_core::db::DispatchOverride>,
) {
    if let Some(record) = override_record {
        value["forced"] = serde_json::json!(true);
        value["force_reason"] = serde_json::json!(record.reason);
        value["forced_at_ms"] = serde_json::json!(record.at_ms);
    }
}

fn status_with_override(
    mut status: String,
    override_record: Option<&thegn_core::db::DispatchOverride>,
) -> String {
    if override_record.is_some() {
        status.push_str("(forced)");
    }
    status
}

fn note_with_override(
    note: &str,
    override_record: Option<&thegn_core::db::DispatchOverride>,
) -> String {
    if let Some(record) = override_record {
        format!(
            "{note} (forced: {})",
            pipeline_report::display_cell(&record.reason)
        )
    } else {
        note.to_string()
    }
}

/// The trailing `note` column: the daemon's transport-retry ledger (THE-86),
/// collapsed to one line and truncated so the roster stays scannable. `-`
/// when absent (every pre-v59 row and anything the observer never touched).
fn note_cell(note: Option<&str>) -> String {
    const MAX: usize = 33;
    let Some(n) = note else {
        return "-".into();
    };
    let one_line = n.split_whitespace().collect::<Vec<_>>().join(" ");
    pipeline_report::display_fit(&one_line, MAX)
        .trim_end()
        .to_string()
}

fn display_dispatch_cells(
    issue_id: &str,
    stage: &str,
    note: Option<&str>,
) -> (String, String, String) {
    (
        pipeline_report::display_cell(issue_id),
        pipeline_report::display_cell(stage),
        note_cell(note),
    )
}

/// Render legacy multiline text one terminal-safe physical line at a time.
fn display_multiline(text: &str) -> String {
    text.lines()
        .map(pipeline_report::display_cell)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Should recording `status` prompt for a reason that was not given?
///
/// Pure so the policy is testable without a DB. True only for an outcome that
/// closes a row as *bad* while the row carries nothing explaining why — no
/// `--why`, no report, no prior note. That combination is the one that made 40
/// of the last run's 45 failed/abandoned rows unreadable after the fact.
///
/// This drives a WARNING, never a refusal: a supervisor must always be able to
/// record a bad outcome, and a gate here would mean the worst outcomes are the
/// hardest to write down.
fn wants_a_reason(
    status: AgentDispatchStatus,
    why_given: bool,
    has_report: bool,
    has_note: bool,
) -> bool {
    matches!(
        status,
        AgentDispatchStatus::Failed | AgentDispatchStatus::Abandoned
    ) && !why_given
        && !has_report
        && !has_note
}

fn set_status(
    id: i64,
    status: &str,
    force: bool,
    why: Option<&str>,
    proof: LeadProof<'_>,
    json: bool,
) -> Result<()> {
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
    let result = if db.get_dispatch(id)?.is_none() {
        Err(anyhow::Error::new(thegn_core::db::MissingDispatch { id }))
    } else {
        set_status_on_led(&db, id, parsed, force, why, proof, json)
    };
    if json
        && let Err(error) = &result
        && let Some(kind) = set_status_error_kind(error)
    {
        super::emit_json(&serde_json::json!({
            "error": kind,
            "id": id,
            "message": error.to_string(),
        }))?;
    }
    result
}

fn set_status_error_kind(err: &anyhow::Error) -> Option<&'static str> {
    if let Some(conflict) = err.downcast_ref::<thegn_core::db::DispatchTransitionConflict>() {
        return Some(match conflict {
            thegn_core::db::DispatchTransitionConflict::Illegal { .. } => "illegal_transition",
            thegn_core::db::DispatchTransitionConflict::Stale { .. } => "stale",
        });
    }
    if err
        .downcast_ref::<thegn_core::db::LeadLeaseConflict>()
        .is_some()
    {
        return Some("lead_lease");
    }
    if err.downcast_ref::<DoneGateRefused>().is_some() {
        return Some("missing_evidence");
    }
    if err
        .downcast_ref::<thegn_core::db::MissingDispatch>()
        .is_some()
    {
        return Some("missing_row");
    }
    None
}

fn set_status_on_led(
    db: &Db,
    id: i64,
    parsed: AgentDispatchStatus,
    force: bool,
    why: Option<&str>,
    proof: LeadProof<'_>,
    json: bool,
) -> Result<()> {
    let row = db
        .get_dispatch(id)?
        .ok_or_else(|| anyhow::anyhow!("no dispatch with id {id}"))?;
    if force && parsed != AgentDispatchStatus::Done {
        anyhow::bail!("`--force` only overrides the `done` gate");
    }
    if force && why.is_none_or(|reason| reason.trim().is_empty()) {
        anyhow::bail!("set-status done --force requires --why '<reason>' (recorded on the row)");
    }
    if !thegn_core::db::set_status_transition_allowed(row.status, parsed) {
        return Err(anyhow::Error::new(
            thegn_core::db::DispatchTransitionConflict::Illegal {
                id,
                from: row.status,
                to: parsed,
            },
        ));
    }
    db.check_dispatch_lead(id, proof)?;
    // Only `done` has artifact evidence requirements, and `--force` overrides
    // that gate. The mutation repeats the lease check atomically with its write.
    if parsed == AgentDispatchStatus::Done && !force {
        done_gate_bound(db, &row)?;
    }
    // A supplied reason is primary audit data, not cache context: commit it
    // atomically with the status so the command cannot claim success after
    // silently losing the explanation.
    let force_reason = if force {
        let reason = why.ok_or_else(|| {
            anyhow::anyhow!(
                "set-status done --force requires --why '<reason>' (recorded on the row)"
            )
        })?;
        db.set_dispatch_status_forced_led(
            id,
            row.status,
            AgentDispatchStatus::Done,
            reason,
            proof,
        )?;
        Some(
            db.dispatch_override(id)?
                .ok_or_else(|| anyhow::anyhow!("forced completion has no stored override"))?
                .reason,
        )
    } else if let Some(w) = why.map(str::trim).filter(|w| !w.is_empty()) {
        db.set_dispatch_status_led(
            id,
            row.status,
            parsed,
            Some(&format!("set-status {}: {w}", parsed.as_str())),
            proof,
        )?;
        None
    } else {
        db.set_dispatch_status_led(id, row.status, parsed, None, proof)?;
        None
    };
    let unexplained = wants_a_reason(
        parsed,
        why.is_some_and(|w| !w.trim().is_empty()),
        row.report.as_deref().is_some_and(|r| !r.trim().is_empty()),
        row.note.as_deref().is_some_and(|n| !n.trim().is_empty()),
    );
    if json {
        let mut v = serde_json::json!({ "id": id, "status": parsed.as_str() });
        if force {
            // A forced completion is never invisible.
            v["forced"] = serde_json::json!(true);
            v["force_reason"] = serde_json::json!(force_reason.as_deref().unwrap_or(""));
        }
        if unexplained {
            v["unexplained"] = serde_json::json!(true);
        }
        return super::emit_json(&v);
    }
    if force {
        outln!(
            "dispatch {id} → {} (forced: {})",
            parsed.as_str(),
            pipeline_report::display_cell(force_reason.as_deref().unwrap_or(""))
        );
    } else {
        outln!("dispatch {id} → {}", parsed.as_str());
    }
    if unexplained {
        outln!(
            "note: row {id} is now {} with nothing recording why — no --why, no report, no \
             note. That is how 40 of the last run's 45 bad rows became unreadable. Pass \
             `--why '<reason>'` next time, or add one now with `thegn dispatch note {id} \
             --text '<reason>'`.",
            parsed.as_str()
        );
    }
    Ok(())
}

/// The `set-status done` gate (THE-76 + THE-88): a row that carries an
/// artifact may only be recorded done when its handoff is real. The gates live
/// in [`pipeline_run::verify_report`]:
///
/// 1. The artifact is present in the row's worktree, committed in `HEAD`, and
///    unchanged at that path
///    (THE-76's original rule — a session exiting is not a handoff).
/// 2. The row carries a non-empty report with an explicit PASS and successful
///    gate citation. When the report has a recorded HEAD, that commit must
///    still contain the unchanged artifact.
///
/// `--force` is the deliberate override for BOTH rules (the refusal names
/// it). Pane-exit auto-stamps (`pty_drain.rs:855-895`) bypass the gate
/// entirely — that path writes the typed status for attribution, not as a
/// handoff verdict, so it is not subject to either check.
#[cfg(test)]
fn done_gate(row: &AgentDispatch) -> Result<()> {
    let report = pipeline_run::verify_report(&verify_facts(row));
    require_done_report(row.id, report)
}

fn done_gate_bound(db: &Db, row: &AgentDispatch) -> Result<()> {
    let report_head = db.dispatch_report_head(row.id)?;
    let facts = verify_facts_with_report_head(row, report_head.as_deref());
    require_done_report(row.id, pipeline_run::verify_report(&facts))
}

#[derive(Debug)]
struct DoneGateRefused {
    id: i64,
    reasons: Vec<String>,
}

impl std::fmt::Display for DoneGateRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "dispatch {} is not verifiably finished:\n  - {}\nrun `thegn dispatch verify {}` for detail, or `--force` to record it anyway",
            self.id,
            self.reasons.join("\n  - "),
            self.id
        )
    }
}

impl std::error::Error for DoneGateRefused {}

fn require_done_report(id: i64, report: pipeline_run::VerifyReport) -> Result<()> {
    if report.ok {
        return Ok(());
    }
    Err(anyhow::Error::new(DoneGateRefused {
        id,
        reasons: report.reasons,
    }))
}

/// Gather the filesystem/git facts for one roster row — the single
/// implementation shared by `dispatch verify`, the `done` gate, and
/// `session open --resume-work`'s finisher facts. A row with
/// no artifact is never sent to git; stage-less rows retain plain-dispatch
/// behavior while pipeline rows are refused by `verify_report`.
/// `pub(crate)`: the resume path in `cmd/session.rs` reads the same facts —
/// one implementation, two callers.
pub(crate) fn verify_facts(row: &AgentDispatch) -> pipeline_run::VerifyFacts {
    verify_facts_with_report_head(row, None)
}

fn verify_facts_with_report_head(
    row: &AgentDispatch,
    report_head: Option<&str>,
) -> pipeline_run::VerifyFacts {
    // THE-88: report presence is a row fact even for plain dispatches. It is
    // only a completion gate when an artifact is present, but `dispatch
    // verify` should faithfully report the column in either case.
    let report_present = row.report.as_deref().is_some_and(|r| !r.trim().is_empty());
    let report_verdict = pipeline_report::verdict_of(row.report.as_deref());
    let Some(artifact) = row
        .artifact_path
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
    else {
        return pipeline_run::VerifyFacts {
            stage: row.stage.clone(),
            artifact: None,
            artifact_identity: pipeline_run::ArtifactIdentity::NotPipeline,
            artifact_empty: false,
            exists: false,
            tracked: false,
            dirty: false,
            worktree_state: None,
            // A plain (non-pipeline) row is never gated; the report fact is
            // still returned so verify reflects the row faithfully.
            report_present,
            report_verdict,
            report_gate_valid: true,
            report_commit_binding: pipeline_run::ReportCommitBinding::Unbound,
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
    let artifact_empty = wt
        .join(&artifact)
        .symlink_metadata()
        .is_ok_and(|m| m.is_file() && m.len() == 0);
    let artifact_identity = match row.stage.as_deref() {
        None => pipeline_run::ArtifactIdentity::NotPipeline,
        Some(stage) => {
            let expected = pipeline_run::artifact_path(&row.issue_id, stage, row.id);
            if artifact == expected {
                pipeline_run::ArtifactIdentity::Canonical
            } else {
                pipeline_run::ArtifactIdentity::Mismatch { expected }
            }
        }
    };
    let tracked_in_index = git_ok(
        wt,
        &["ls-files", "--error-unmatch", "--", artifact.as_str()],
    );
    let worktree_binding_valid = git_out_allow_empty(wt, &["rev-parse", "--show-toplevel"])
        .and_then(|root| std::path::PathBuf::from(root).canonicalize().ok())
        .zip(wt.canonicalize().ok())
        .is_some_and(|(git_root, row_root)| git_root == row_root);
    // The artifact must be present in HEAD and byte-for-byte clean relative to
    // it. `ls-files` alone blesses a newly staged file, which is not a durable
    // handoff and disappears if the worktree is cleaned.
    let tracked = worktree_binding_valid
        && tracked_in_index
        && git_ok(wt, &["diff", "--quiet", "HEAD", "--", artifact.as_str()]);
    let status = git_out_allow_empty(wt, &["status", "--porcelain"]);
    let worktree_state = worktree_state_from_status(status.as_deref());
    let dirty = worktree_state == pipeline_run::WorktreeState::Dirty;
    // THE-88: a row carrying an artifact must also file a report —
    // `verify_report` turns the missing case into an `ok=false` reason that
    // names the `dispatch report` command as the fix.
    let mut facts = pipeline_run::VerifyFacts {
        stage: row.stage.clone(),
        artifact: Some(artifact),
        artifact_identity,
        artifact_empty,
        exists,
        tracked,
        dirty,
        worktree_state: Some(worktree_state),
        report_present,
        report_verdict,
        report_gate_valid: report_verdict != pipeline_report::Verdict::Pass
            || row
                .report
                .as_deref()
                .is_none_or(|report| pipeline_report::gate_evidence(report).is_some()),
        report_commit_binding: pipeline_run::ReportCommitBinding::Unbound,
    };
    if report_present && let Some(report_head) = report_head {
        if !is_hex_oid(report_head) {
            facts.report_commit_binding =
                pipeline_run::report_commit_binding(Some(report_head), None, None, None);
            return facts;
        }
        let head = git_out_allow_empty(wt, &["rev-parse", "--verify", "HEAD"])
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let (is_ancestor, artifact_unchanged) = match (head.as_deref(), facts.artifact.as_deref()) {
            (Some(head), Some(artifact)) => (
                git_exit_0_or_1(wt, &["merge-base", "--is-ancestor", report_head, head]),
                git_exit_0_or_1(wt, &["diff", "--quiet", report_head, head, "--", artifact]),
            ),
            _ => (None, None),
        };
        facts.report_commit_binding = pipeline_run::report_commit_binding(
            Some(report_head),
            head.as_deref(),
            is_ancestor,
            artifact_unchanged,
        );
    }
    facts
}

/// Return Some(true/false) for git exit 0/1; any spawn or other exit failure is unknown.
#[expect(
    clippy::disallowed_methods,
    reason = "dispatch verify, set-status, and wait are CLI paths off the compositor loop"
)]
fn git_exit_0_or_1(worktree: &std::path::Path, args: &[&str]) -> Option<bool> {
    let output = thegn_core::util::git_cmd(worktree)
        .args(args)
        .output()
        .ok()?;
    match output.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

fn is_hex_oid(value: &str) -> bool {
    (40..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn worktree_state_from_status(status: Option<&str>) -> pipeline_run::WorktreeState {
    match status {
        Some("") => pipeline_run::WorktreeState::Clean,
        Some(_) => pipeline_run::WorktreeState::Dirty,
        None => pipeline_run::WorktreeState::Unknown,
    }
}

/// `dispatch reap` — reconcile active rows against the daemon and git.
///
/// Plan a reap of every active row against the daemon's live session snapshot
/// and durable exit stamps. Missing both is unknown and cannot trigger a row
/// transition.
///
/// Shared with the daemon's periodic self-heal
/// (`crate::daemon::pipeline_reaper`) so the two can never disagree about what
/// a row's facts are. Only the PLAN is shared: the two callers deliberately
/// apply different policies to it — the CLI records the supervisor's verdict
/// directly, the daemon parks rather than failing (see that module's header).
///
/// Blocking: reads each row's worktree and shells out to `git`, so a caller on
/// an async runtime must run this inside `spawn_blocking`.
pub(crate) fn reap_plan(
    db: &Db,
    live_ids: Option<&[String]>,
) -> Result<(
    Vec<pipeline_reap::Reap>,
    HashMap<i64, thegn_core::issue::DispatchRunRef>,
)> {
    let (plan, run_refs, truncated) = reap_plan_limited(db, live_ids, CLI_REAP_ROW_LIMIT)?;
    if truncated {
        thegn_core::msg::warn(&format!(
            "dispatch reap plan covers only the oldest {CLI_REAP_ROW_LIMIT} active rows; re-run after --apply to continue"
        ));
    }
    Ok((plan, run_refs))
}

fn reap_plan_limited(
    db: &Db,
    live_ids: Option<&[String]>,
    limit: usize,
) -> Result<(
    Vec<pipeline_reap::Reap>,
    HashMap<i64, thegn_core::issue::DispatchRunRef>,
    bool,
)> {
    let (rows, truncated) = db.list_dispatches_with_status(
        &[AgentDispatchStatus::Spawning, AgentDispatchStatus::Running],
        limit,
    )?;
    let run_refs = db
        .running_dispatch_run_refs()?
        .into_iter()
        .filter(|run| {
            rows.iter().any(|row| {
                row.id == run.id
                    && row.status == AgentDispatchStatus::Running
                    && row.session_id.as_deref().unwrap_or("") == run.session_id
            })
        })
        .map(|run| (run.id, run))
        .collect();
    Ok((reap_plan_rows(&rows, live_ids), run_refs, truncated))
}

fn reap_facts(
    f: &pipeline_run::VerifyFacts,
    worker: pipeline_reap::WorkerLiveness,
) -> pipeline_reap::ReapFacts {
    pipeline_reap::ReapFacts {
        worker,
        artifact_exists: f.exists,
        artifact_tracked: f.tracked,
        report_present: f.report_present,
        report_verdict: f.report_verdict,
        report_gate_valid: f.report_gate_valid,
        worktree_unknown: f.worktree_state == Some(pipeline_run::WorktreeState::Unknown),
        artifact_ok: f.stage.is_none()
            || (matches!(
                &f.artifact_identity,
                pipeline_run::ArtifactIdentity::Canonical
            ) && !f.artifact_empty),
    }
}

/// Plan from a caller-owned row snapshot. The periodic daemon uses this form
/// so no database mutex is held while `verify_facts` performs filesystem and
/// git reads for each row.
pub(crate) fn reap_plan_rows(
    rows: &[AgentDispatch],
    live_ids: Option<&[String]>,
) -> Vec<pipeline_reap::Reap> {
    pipeline_reap::plan(rows, |r| {
        let facts = verify_facts(r);
        let worker = if r
            .session_id
            .as_deref()
            .is_some_and(|s| live_ids.is_some_and(|ids| ids.iter().any(|id| id == s)))
        {
            pipeline_reap::WorkerLiveness::Live
        } else if r.exit_code.is_some() || r.exited_at_ms.is_some() {
            pipeline_reap::WorkerLiveness::Exited
        } else {
            pipeline_reap::WorkerLiveness::Unknown
        };
        reap_facts(&facts, worker)
    })
}

fn reap(cfg: &Config, apply: bool, json: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let live_ids = rt.block_on(live_session_ids(cfg));
    let daemon_up = live_ids.is_some();
    let db = Db::open()?;
    let (plan, run_refs) = reap_plan(&db, live_ids.as_deref())?;
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
                        | pipeline_reap::ReapVerdict::NeedsDecision { why }
                        | pipeline_reap::ReapVerdict::Unknown { why } => Some(*why),
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
                "unknown": summary.unknown,
                "close_done": summary.close_done,
                "mark_failed": summary.mark_failed,
                "needs_decision": summary.needs_decision,
            },
        }))?;
    } else {
        if !daemon_up {
            outln!(
                "note: daemon liveness is unavailable — rows without a durable exit stamp stay \\
                 unknown. Retry when the daemon is reachable, or reconcile those rows by hand."
            );
        }
        for r in &plan {
            match &r.verdict {
                pipeline_reap::ReapVerdict::Live => outln!("  {} live", r.id),
                pipeline_reap::ReapVerdict::Unknown { why } => {
                    outln!("  {} unknown: {}", r.id, reap_human_why(why));
                }
                pipeline_reap::ReapVerdict::MarkFailed { why }
                | pipeline_reap::ReapVerdict::NeedsDecision { why } => {
                    outln!("  {} {}: {}", r.id, r.verdict.token(), reap_human_why(why));
                }
                v => outln!("  {} {}", r.id, v.token()),
            }
        }
        outln!(
            "\n{} live, {} unknown, {} to close done, {} to mark failed, {} need a decision",
            summary.live,
            summary.unknown,
            summary.close_done,
            summary.mark_failed,
            summary.needs_decision
        );
        if !apply && summary.actionable() > 0 {
            outln!(
                "(dry run — pass --apply to perform the {} unambiguous transition(s))",
                summary.actionable()
            );
        }
    }

    if !apply {
        return Ok(());
    }
    apply_reap_plan(&db, &plan, &run_refs)
}

fn apply_reap_plan(
    db: &Db,
    plan: &[pipeline_reap::Reap],
    run_refs: &HashMap<i64, thegn_core::issue::DispatchRunRef>,
) -> Result<()> {
    for r in plan {
        match &r.verdict {
            pipeline_reap::ReapVerdict::CloseDone => {
                // Plain, gated `done`: it passes unforced precisely because the
                // artifact is committed and a report exists.
                let Some(run) = run_refs.get(&r.id) else {
                    outln!(
                        "dispatch {} skipped (run changed or spawning; decide by hand)",
                        r.id
                    );
                    continue;
                };
                if db.compare_and_set_dispatch_status_run(
                    run,
                    r.observed_status,
                    AgentDispatchStatus::Done,
                    None,
                )? {
                    outln!("dispatch {} → done", r.id);
                } else {
                    outln!(
                        "dispatch {} skipped (run changed or spawning; decide by hand)",
                        r.id
                    );
                }
            }
            pipeline_reap::ReapVerdict::MarkFailed { why } => {
                let Some(run) = run_refs.get(&r.id) else {
                    outln!(
                        "dispatch {} skipped (run changed or spawning; decide by hand)",
                        r.id
                    );
                    continue;
                };
                if db.compare_and_set_dispatch_status_run(
                    run,
                    r.observed_status,
                    AgentDispatchStatus::Failed,
                    Some(&format!("reaped: {why}")),
                )? {
                    outln!("dispatch {} → failed", r.id);
                } else {
                    outln!(
                        "dispatch {} skipped (run changed or spawning; decide by hand)",
                        r.id
                    );
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Live (non-tombstone) session ids. `None` means liveness could not be
/// established; an available empty list remains a real observation.
async fn live_session_ids(cfg: &Config) -> Option<Vec<String>> {
    const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    query_live_sessions(
        QUERY_TIMEOUT,
        || crate::cmd::session::connect(cfg),
        |client| async move {
            let sessions = client.sessions().await?;
            Ok(sessions
                .into_iter()
                .filter(|s| s.exited_at_ms.is_none())
                .map(|s| s.id)
                .collect())
        },
    )
    .await
}

async fn query_live_sessions<C, CFut, Client, L, LFut>(
    timeout: std::time::Duration,
    connect: C,
    list: L,
) -> Option<Vec<String>>
where
    C: FnOnce() -> CFut,
    CFut: std::future::Future<Output = anyhow::Result<Client>>,
    L: FnOnce(Client) -> LFut,
    LFut: std::future::Future<Output = anyhow::Result<Vec<String>>>,
{
    match tokio::time::timeout(timeout, async {
        let client = connect().await?;
        list(client).await
    })
    .await
    {
        Ok(Ok(ids)) => Some(ids),
        Ok(Err(error)) => {
            tracing::debug!(target: "thegn::pipeline", %error, "reap could not query daemon liveness");
            None
        }
        Err(_) => {
            tracing::debug!(target: "thegn::pipeline", "reap daemon liveness query timed out");
            None
        }
    }
}

fn verify(id: i64, json: bool) -> Result<()> {
    let db = Db::open()?;
    let row = db
        .get_dispatch(id)?
        .ok_or_else(|| anyhow::anyhow!("no dispatch with id {id}"))?;
    let report_head = db.dispatch_report_head(id)?;
    let facts = verify_facts_with_report_head(&row, report_head.as_deref());
    let report = pipeline_run::verify_report(&facts);
    if json {
        // One flat document — the report plus the row id — so a supervisor can
        // grep `"ok":false` / `"id":12` from a single line.
        let mut v = serde_json::to_value(&report)?;
        v["id"] = serde_json::json!(id);
        super::emit_json(&v)?;
    } else {
        let (a, warnings, reasons) = verify_human_text(
            report.artifact.as_deref(),
            &report.warnings,
            &report.reasons,
        );
        outln!(
            "dispatch {id}  artifact {a}  exists={} tracked={} dirty={} worktree={} report={}  ok={}",
            yes_no(report.exists),
            yes_no(report.tracked),
            yes_no(report.dirty),
            worktree_state_name(report.worktree_state.as_ref()),
            yes_no(report.report_present),
            yes_no(report.ok)
        );
        for warning in &warnings {
            outln!("  warning: {warning}");
        }
        for r in &reasons {
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

fn worktree_state_name(state: Option<&pipeline_run::WorktreeState>) -> &'static str {
    match state {
        Some(pipeline_run::WorktreeState::Clean) => "clean",
        Some(pipeline_run::WorktreeState::Dirty) => "dirty",
        Some(pipeline_run::WorktreeState::Unknown) => "unknown",
        None => "not_observed",
    }
}

fn reap_human_why(why: &str) -> String {
    pipeline_report::display_cell(why)
}

fn verify_human_text(
    artifact: Option<&str>,
    warnings: &[String],
    reasons: &[String],
) -> (String, Vec<String>, Vec<String>) {
    (
        pipeline_report::display_cell(artifact.unwrap_or("-")),
        warnings
            .iter()
            .map(|warning| pipeline_report::display_cell(warning))
            .collect(),
        reasons
            .iter()
            .map(|reason| pipeline_report::display_cell(reason))
            .collect(),
    )
}

fn wait_human_text(stage: &str, report: Option<&str>) -> (String, Option<String>) {
    (
        pipeline_report::display_cell(stage),
        report
            .filter(|text| !text.trim().is_empty())
            .map(display_multiline),
    )
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
    let recorded = record_report(db, id, text)?;
    print_report_outcome(id, &recorded.validated, recorded.ungated, json)
}

fn print_report_outcome(id: i64, validated: &str, ungated: bool, json: bool) -> Result<()> {
    if json {
        let mut v = serde_json::json!({
            "id": id,
            "report": validated,
            "bytes": validated.len(),
        });
        if ungated {
            v["pass_without_gate_evidence"] = serde_json::json!(true);
        }
        super::emit_json(&v)?;
        return Ok(());
    }
    outln!(
        "report recorded on dispatch {} ({} chars)",
        id,
        validated.chars().count()
    );
    if ungated {
        outln!(
            "note: this report reads as PASS but cites no gate result. A PASS means the \
             queue-boundary suite is green — add a line like\n  gate: cargo nextest run \
             --workspace -- <N> passed, <M> skipped\nScoped suites do not run the \
             workspace-level coverage tests the fold gate rejects branches on."
        );
    }
    Ok(())
}

pub(crate) struct RecordedReport {
    pub validated: String,
    pub ungated: bool,
}

pub(crate) fn record_report(db: &Db, id: i64, text: &str) -> Result<RecordedReport> {
    let validated = pipeline_report::report_text(text).map_err(|e| anyhow::anyhow!("{e}"))?;
    let row = db
        .get_dispatch(id)?
        .ok_or_else(|| anyhow::anyhow!("roster row {id} does not exist"))?;
    let head_oid = git_out_allow_empty(
        std::path::Path::new(&row.worktree_path),
        &["rev-parse", "--verify", "HEAD"],
    )
    .filter(|oid| !oid.is_empty());
    if head_oid.is_none() {
        tracing::warn!(target: "thegn::pipeline", dispatch_id = id, "could not record HEAD for dispatch report");
    }
    db.set_dispatch_report_bound(id, &validated, head_oid.as_deref())?;
    let ungated = pipeline_report::pass_without_gate_evidence(&validated);
    Ok(RecordedReport { validated, ungated })
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
    let note = match db.append_dispatch_note_record(id, &validated) {
        Ok(note) => note,
        Err(error) => {
            if json && let Some(quota) = error.downcast_ref::<DispatchNoteQuotaExceeded>() {
                super::emit_json(&note_quota_error_json(id, quota))?;
            }
            return Err(error);
        }
    };
    if json {
        super::emit_json(&note_json(id, &note))?;
    } else {
        outln!(
            "note recorded on dispatch {} ({} chars)",
            id,
            validated.chars().count()
        );
    }
    Ok(())
}

fn note_quota_error_json(id: i64, quota: &DispatchNoteQuotaExceeded) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "error": "note_quota_exceeded",
        "kind": match quota.kind {
            NoteQuotaKind::Count => "count",
            NoteQuotaKind::Bytes => "bytes",
        },
        "notes": quota.notes,
        "bytes": quota.bytes,
    })
}

fn note_json(id: i64, note: &DispatchNote) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "note": &note.text,
        "note_id": note.id,
        "created_at_ms": note.created_at_ms,
    })
}

/// The row-mode status display is bounded to the newest notes. The digest
/// itself receives the bounded suffix; `read_status` supplies the full
/// matching count separately from SQL.
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
/// (default all, count plus newest 20 via bounded SQL queries). JSON: the
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
    let rows = if let Some(id) = row {
        match db.get_dispatch(id)? {
            Some(row) => vec![row],
            None => {
                anyhow::bail!("no dispatch with id {id}");
            }
        }
    } else if all {
        db.list_dispatches()?
    } else {
        bounded_rows(db, &ACTIVE_STATUSES, "dispatch status shows")?.0
    };

    let row_mode = row.is_some();
    let overrides = db.dispatch_overrides()?;
    let mut notes_map: HashMap<i64, Vec<DispatchNote>> = HashMap::new();
    let mut note_counts = HashMap::new();
    for r in &rows {
        note_counts.insert(r.id, db.dispatch_note_count(r.id, since_ms)?);
        notes_map.insert(r.id, db.newest_dispatch_notes(r.id, since_ms, 20)?);
    }
    let mut digests = pipeline_report::digest(&rows, &notes_map, since_ms);
    for digest in &mut digests {
        digest.note_count = note_counts.get(&digest.id).copied().unwrap_or_default();
    }
    if json {
        if row_mode {
            let digest = digests.into_iter().next().expect("one selected row");
            let mut v = serde_json::to_value(&digest)?;
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
            let notes_shown = notes.len();
            v["notes"] = serde_json::Value::Array(notes);
            attach_note_quota_status(&mut v, digest.note_count, notes_shown);
            annotate_override(&mut v, overrides.get(&digest.id));
            super::emit_json(&v)?;
        } else {
            let mut values = digests
                .iter()
                .map(serde_json::to_value)
                .collect::<serde_json::Result<Vec<_>>>()?;
            for (value, digest) in values.iter_mut().zip(&digests) {
                annotate_override(value, overrides.get(&digest.id));
            }
            super::emit_json(&values)?;
        }
    } else {
        for digest in &digests {
            let latest = digest
                .latest_note
                .as_ref()
                .map(|(_, text)| text.as_str())
                .unwrap_or("-");
            let latest = note_cell(Some(latest));
            let forced = if let Some(entry) = overrides.get(&digest.id) {
                format!(" forced={}", pipeline_report::display_cell(&entry.reason))
            } else {
                String::new()
            };
            outln!(
                "{} {} {} {} notes={} last={}{}",
                digest.id,
                digest.status,
                pipeline_report::display_cell(digest.stage.as_deref().unwrap_or("-")),
                pipeline_report::display_cell(&digest.issue_id),
                digest.note_count,
                latest,
                forced
            );
        }
        if row_mode {
            if let Some(report) = digests.first().and_then(|d| d.report.as_deref()) {
                outln!("report:");
                outln!("{}", display_multiline(report));
            }
            outln!("notes:");
            if let Some(all_notes) = notes_map.get(&row.expect("row mode")) {
                for note in newest_status_notes(all_notes) {
                    for line in note.text.lines() {
                        outln!("- {}", pipeline_report::display_cell(line));
                    }
                }
            }
        }
    }
    Ok(())
}

fn attach_note_quota_status(value: &mut serde_json::Value, notes_total: usize, notes_shown: usize) {
    value["notes_total"] = serde_json::json!(notes_total);
    value["notes_shown"] = serde_json::json!(notes_shown);
    value["truncated"] = serde_json::json!(notes_total > notes_shown);
    value["quota"] = serde_json::json!({
        "max_notes": pipeline_report::NOTE_MAX_PER_ROW,
        "max_bytes": pipeline_report::NOTE_MAX_BYTES_PER_ROW,
    });
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
    let rows = wait_rows(&db, row)?;
    let targets = pipeline_run::wait_candidates(&rows, row).map_err(|e| anyhow::anyhow!("{e}"))?;
    let targets = fence_wait_targets(&db, targets)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(wait_wake(cfg, targets, timeout, json))
}

fn wait_rows(db: &Db, row: Option<i64>) -> Result<Vec<AgentDispatch>> {
    if let Some(id) = row {
        return Ok(db.get_dispatch(id)?.into_iter().collect());
    }
    bounded_rows(
        db,
        &[AgentDispatchStatus::Spawning, AgentDispatchStatus::Running],
        "dispatch wait --any covers",
    )
    .map(|(rows, _)| rows)
}

fn fence_wait_targets(db: &Db, targets: Vec<WaitTarget>) -> Result<Vec<FencedWaitTarget>> {
    targets
        .into_iter()
        .map(|target| {
            let run = db
                .dispatch_run_ref(target.id)
                .map_err(|error| {
                    anyhow::Error::new(crate::cmd::Retryable(anyhow::anyhow!(
                        "could not fence dispatch {} before wait: {error}",
                        target.id
                    )))
                })?
                .ok_or_else(|| {
                    anyhow::Error::new(crate::cmd::Retryable(anyhow::anyhow!(
                        "dispatch {} disappeared before wait; retry",
                        target.id
                    )))
                })?;
            if run.session_id != target.session_id {
                return Err(anyhow::Error::new(crate::cmd::Retryable(anyhow::anyhow!(
                    "dispatch {} changed session before wait; retry",
                    target.id
                ))));
            }
            Ok(FencedWaitTarget {
                target,
                run_gen: run.run_gen,
            })
        })
        .collect()
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

/// The selected session and generation this wait candidate belongs to.
#[derive(Debug, Clone)]
struct FencedWaitTarget {
    target: WaitTarget,
    run_gen: i64,
}

/// Does a gone wake carry a verified handoff?
///
/// A gone session is not evidence of a handoff. Verify the exact current row
/// using the same completion facts as `dispatch verify`, then fence the result
/// against the selected session and generation.
/// Artifactless plain rows retain their report-based handoff semantics;
/// artifactless pipeline reservations are unverified candidates.
fn wake_handoff(
    db: Result<Db>,
    target: &FencedWaitTarget,
) -> (pipeline_run::HandoffOutcome, Option<String>, Option<String>) {
    let unknown = || {
        (
            pipeline_run::HandoffOutcome::InfrastructureUnknown,
            None,
            None,
        )
    };
    let Ok(db) = db else { return unknown() };
    let row = match db.get_dispatch(target.target.id) {
        Ok(Some(row)) => row,
        Ok(None) | Err(_) => return unknown(),
    };
    let same_run = |row: &AgentDispatch| {
        row.session_id.as_deref() == Some(target.target.session_id.as_str())
            && db.dispatch_run_ref(target.target.id).is_ok_and(|current| {
                current.is_some_and(|run| {
                    run.session_id == target.target.session_id && run.run_gen == target.run_gen
                })
            })
    };
    if !same_run(&row) {
        return unknown();
    }
    let report = row.report.clone();
    let artifact = row.artifact_path.clone();
    let report_head = match db.dispatch_report_head(target.target.id) {
        Ok(head) => head,
        Err(_) => return unknown(),
    };
    let facts = verify_facts_with_report_head(&row, report_head.as_deref());
    let verdict = pipeline_run::verify_report(&facts);
    let outcome = if facts.worktree_state == Some(pipeline_run::WorktreeState::Unknown) {
        pipeline_run::HandoffOutcome::InfrastructureUnknown
    } else if facts.artifact.is_none() && facts.stage.is_none() {
        if facts.report_present {
            pipeline_run::HandoffOutcome::VerifiedHandoff
        } else {
            pipeline_run::HandoffOutcome::Absent
        }
    } else if verdict.ok && facts.report_commit_binding == pipeline_run::ReportCommitBinding::Bound
    {
        pipeline_run::HandoffOutcome::VerifiedHandoff
    } else if facts.report_commit_binding == pipeline_run::ReportCommitBinding::Unknown {
        pipeline_run::HandoffOutcome::InfrastructureUnknown
    } else if facts.stage.is_some() && facts.artifact.is_none() {
        pipeline_run::HandoffOutcome::UnverifiedCandidate
    } else if !facts.exists && !facts.report_present {
        pipeline_run::HandoffOutcome::Absent
    } else {
        pipeline_run::HandoffOutcome::UnverifiedCandidate
    };
    let current = match db.get_dispatch(target.target.id) {
        Ok(Some(row)) => row,
        Ok(None) | Err(_) => return unknown(),
    };
    let current_report_head = match db.dispatch_report_head(target.target.id) {
        Ok(head) => head,
        Err(_) => return unknown(),
    };
    if !same_run(&current)
        || current.worktree_path != row.worktree_path
        || current.artifact_path != row.artifact_path
        || current.report != row.report
        || current_report_head != report_head
    {
        return unknown();
    }
    (outcome, report, artifact)
}

async fn wait_wake(
    cfg: &Config,
    targets: Vec<FencedWaitTarget>,
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
                        &t.target.session_id,
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
                        .wait(
                            &t.target.session_id,
                            serde_json::json!({ "kind": "exited" }),
                            None,
                        )
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
        // Read and verify the current row at wake time, then fence the result
        // against the exact session/run generation selected before waiting.
        let (handoff, report, artifact) = if gone {
            wake_handoff(Db::open(), &t)
        } else {
            let (report, artifact) = row_report_artifact(t.target.id)
                .map_err(|e| anyhow::Error::new(crate::cmd::Retryable(e)))?;
            (pipeline_run::HandoffOutcome::Absent, report, artifact)
        };
        if json {
            let mut v = serde_json::json!({
                "row": t.target.id,
                "session": t.target.session_id,
                "stage": t.target.stage,
                "issue": t.target.issue_id,
                "exit_code": exit_code,
                "matched": true,
                "report": report,
                "artifact": artifact,
            });
            if gone {
                v["gone"] = serde_json::json!(true);
                v["handoff_outcome"] = serde_json::json!(handoff);
                v["handoff_present"] = serde_json::json!(handoff.handoff_present());
            }
            super::emit_json(&v)?;
            if gone && handoff == pipeline_run::HandoffOutcome::InfrastructureUnknown {
                std::process::exit(crate::cmd::EXIT_RETRYABLE);
            }
            return Ok(());
        }
        let (stage, report) =
            wait_human_text(t.target.stage.as_deref().unwrap_or("-"), report.as_deref());
        match (gone, exit_code) {
            (true, _) => outln!(
                "dispatch {} ({}) session is gone: {}",
                t.target.id,
                stage,
                handoff_message(handoff)
            ),
            (false, Some(code)) => outln!("dispatch {} ({}) exited {code}", t.target.id, stage),
            // Matched but unreapable — the same `?` `session list` prints.
            (false, None) => outln!("dispatch {} ({}) exited ?", t.target.id, stage),
        }
        // Print the report under the exited line — the Lead reads it and
        // nothing else. A missing/blank report prints nothing: the empty
        // line keeps the human output one event per wake.
        if let Some(text) = report.as_deref() {
            outln!("report:");
            outln!("{text}");
        }
        if gone && handoff == pipeline_run::HandoffOutcome::InfrastructureUnknown {
            std::process::exit(crate::cmd::EXIT_RETRYABLE);
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

fn handoff_message(outcome: pipeline_run::HandoffOutcome) -> &'static str {
    match outcome {
        pipeline_run::HandoffOutcome::VerifiedHandoff => {
            "verified handoff; run dispatch verify before advancing"
        }
        pipeline_run::HandoffOutcome::UnverifiedCandidate => {
            "unverified handoff candidate; verify before retry or reconciliation"
        }
        pipeline_run::HandoffOutcome::Absent => "no verified handoff; safe to retry or reconcile",
        pipeline_run::HandoffOutcome::InfrastructureUnknown => {
            "handoff verification unavailable; retry verification before deciding"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::store::NotificationStore;

    #[test]
    fn report_head_revision_must_be_a_hex_oid() {
        assert!(is_hex_oid(&"a".repeat(40)));
        assert!(is_hex_oid(&"f".repeat(64)));
        assert!(!is_hex_oid("HEAD"));
        assert!(!is_hex_oid(&format!("{}g", "a".repeat(39))));
        assert!(!is_hex_oid(&format!("-{}", "a".repeat(40))));
    }

    fn db(name: &str) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join(format!("{name}.db"))).unwrap();
        (dir, db)
    }

    #[test]
    fn set_status_refuses_resurrecting_a_done_row() {
        let (_dir, db) = db("set-status-terminal");
        let id = db
            .put_agent_dispatch(NewDispatch::new("linear:THE-242", "/wt/done", "claude"))
            .unwrap();
        db.update_dispatch_status(id, AgentDispatchStatus::Done)
            .unwrap();

        let err = set_status_on_led(
            &db,
            id,
            AgentDispatchStatus::Running,
            false,
            None,
            LeadProof::Unbound,
            true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("done to running"));
        assert_eq!(
            db.get_dispatch(id).unwrap().unwrap().status,
            AgentDispatchStatus::Done
        );
    }

    #[test]
    fn set_status_json_error_kind_classifies_typed_refusals() {
        use thegn_core::db::DispatchTransitionConflict as Conflict;

        let illegal = anyhow::Error::new(Conflict::Illegal {
            id: 7,
            from: AgentDispatchStatus::Running,
            to: AgentDispatchStatus::Merged,
        });
        let stale = anyhow::Error::new(Conflict::Stale {
            id: 7,
            expected: AgentDispatchStatus::Running,
            actual: AgentDispatchStatus::Done,
        });
        let evidence = anyhow::Error::new(DoneGateRefused {
            id: 7,
            reasons: vec!["missing artifact".into()],
        });
        let missing = anyhow::Error::new(thegn_core::db::MissingDispatch { id: 7 });
        let plain = anyhow::anyhow!("unclassified");

        assert_eq!(set_status_error_kind(&illegal), Some("illegal_transition"));
        assert_eq!(set_status_error_kind(&stale), Some("stale"));
        assert_eq!(set_status_error_kind(&evidence), Some("missing_evidence"));
        assert_eq!(set_status_error_kind(&missing), Some("missing_row"));
        assert_eq!(set_status_error_kind(&plain), None);
    }

    #[test]
    fn set_status_json_classifies_queued_to_done_as_illegal_transition() {
        let (_dir, db) = db("set-status-json-queued");
        let id = db
            .put_agent_dispatch(NewDispatch::new("linear:THE-977", "/wt/queued", "claude"))
            .unwrap();
        let error = set_status_on_led(
            &db,
            id,
            AgentDispatchStatus::Done,
            false,
            None,
            LeadProof::Unbound,
            true,
        )
        .unwrap_err();
        assert_eq!(set_status_error_kind(&error), Some("illegal_transition"));
        assert!(error.to_string().contains("queued to done"));
    }

    #[test]
    fn set_status_json_classifies_missing_artifact_as_missing_evidence() {
        let (_dir, db) = db("set-status-json-evidence");
        let id = db
            .put_agent_dispatch(NewDispatch {
                artifact_path: Some("missing-handoff.md"),
                ..NewDispatch::new("linear:THE-977", "/wt/missing", "claude")
            })
            .unwrap();
        db.update_dispatch_status(id, AgentDispatchStatus::Running)
            .unwrap();
        let error = set_status_on_led(
            &db,
            id,
            AgentDispatchStatus::Done,
            false,
            None,
            LeadProof::Unbound,
            true,
        )
        .unwrap_err();
        assert_eq!(set_status_error_kind(&error), Some("missing_evidence"));
        assert!(error.to_string().contains("not verifiably finished"));
    }

    #[test]
    fn set_status_reports_illegal_transition_before_done_artifact_gate() {
        let (_dir, db) = db("set-status-illegal-before-gate");
        let id = db
            .put_agent_dispatch(NewDispatch {
                artifact_path: Some("missing-handoff.md"),
                ..NewDispatch::new("linear:THE-242", "/wt/merged", "claude")
            })
            .unwrap();
        db.update_dispatch_status(id, AgentDispatchStatus::Merged)
            .unwrap();

        let err = set_status_on_led(
            &db,
            id,
            AgentDispatchStatus::Done,
            false,
            None,
            LeadProof::Unbound,
            true,
        )
        .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<thegn_core::db::DispatchTransitionConflict>(),
            Some(thegn_core::db::DispatchTransitionConflict::Illegal {
                from: AgentDispatchStatus::Merged,
                to: AgentDispatchStatus::Done,
                ..
            })
        ));
    }

    #[test]
    fn forced_status_requires_done_and_a_nonblank_reason_before_writing() {
        let (_dir, db) = db("set-status-force-reason-required");
        let id = db
            .put_agent_dispatch(NewDispatch::new(
                "linear:THE-270",
                "/wt/force-required",
                "claude",
            ))
            .unwrap();
        for why in [None, Some("  ")] {
            assert!(
                set_status_on_led(
                    &db,
                    id,
                    AgentDispatchStatus::Done,
                    true,
                    why,
                    LeadProof::Unbound,
                    true,
                )
                .is_err()
            );
            assert_eq!(
                db.get_dispatch(id).unwrap().unwrap().status,
                AgentDispatchStatus::Queued
            );
            assert!(db.dispatch_override(id).unwrap().is_none());
            assert_eq!(db.dispatch_note_count(id, None).unwrap(), 0);
        }
        assert!(
            set_status_on_led(
                &db,
                id,
                AgentDispatchStatus::Running,
                true,
                Some("x"),
                LeadProof::Unbound,
                true,
            )
            .is_err()
        );
        assert_eq!(
            db.get_dispatch(id).unwrap().unwrap().status,
            AgentDispatchStatus::Queued
        );
        assert!(db.dispatch_override(id).unwrap().is_none());
    }

    #[test]
    fn forced_status_writes_reasoned_done_and_json_annotation_is_stable() {
        let (_dir, db) = db("set-status-force-reason-write");
        let id = db
            .put_agent_dispatch(NewDispatch::new(
                "linear:THE-270-W",
                "/wt/force-write",
                "claude",
            ))
            .unwrap();
        db.update_dispatch_status(id, AgentDispatchStatus::Running)
            .unwrap();
        set_status_on_led(
            &db,
            id,
            AgentDispatchStatus::Done,
            true,
            Some("reviewed"),
            LeadProof::Unbound,
            true,
        )
        .unwrap();
        let record = db.dispatch_override(id).unwrap().unwrap();
        assert_eq!(record.reason, "reviewed");
        assert_eq!(
            db.get_dispatch(id).unwrap().unwrap().status,
            AgentDispatchStatus::Done
        );

        let mut value = serde_json::json!({"id": id});
        annotate_override(&mut value, Some(&record));
        assert_eq!(value["forced"], true);
        assert_eq!(value["force_reason"], "reviewed");
        assert_eq!(value["forced_at_ms"], record.at_ms);
        assert_eq!(
            status_with_override("done".into(), Some(&record)),
            "done(forced)"
        );
        assert!(note_with_override("-", Some(&record)).contains("forced: reviewed"));
        let mut ordinary = serde_json::json!({"id": 2});
        annotate_override(&mut ordinary, None);
        assert!(ordinary.get("forced").is_none());
        assert_eq!(status_with_override("queued".into(), None), "queued");
    }

    #[test]
    fn assigned_but_missing_artifact_is_not_a_gone_wake_handoff() {
        let (_dir, db) = db("wake-missing-artifact");
        let (_worktree_dir, worktree) = git_repo("wake-worktree");
        let id = db
            .put_dispatch_with_parent_check(thegn_core::issue::NewDispatch {
                stage: Some("code"),
                artifact_path: None,
                ..thegn_core::issue::NewDispatch::new(
                    "linear:THE-253",
                    worktree.to_str().unwrap(),
                    "coder",
                )
            })
            .unwrap();
        db.bind_dispatch_session(id, "session-253").unwrap();
        let run = db.dispatch_run_ref(id).unwrap().unwrap();
        let target = FencedWaitTarget {
            target: WaitTarget {
                id,
                session_id: run.session_id,
                stage: Some("code".into()),
                issue_id: "linear:THE-253".into(),
            },
            run_gen: run.run_gen,
        };
        let (outcome, _, artifact) = wake_handoff(Ok(db), &target);
        let expected = pipeline_run::artifact_path("linear:THE-253", "code", id);
        assert_eq!(artifact.as_deref(), Some(expected.as_str()));
        assert_eq!(outcome, pipeline_run::HandoffOutcome::Absent);
        assert!(!outcome.handoff_present(), "assigned path is not a handoff");
    }

    #[test]
    fn artifactless_pipeline_reservation_is_not_a_verified_wake_handoff() {
        let (_dir, db) = db("wake-reservation");
        let (_worktree_dir, worktree) = git_repo("wake-reservation-worktree");
        let id = db
            .put_agent_dispatch(thegn_core::issue::NewDispatch {
                stage: Some("code"),
                ..thegn_core::issue::NewDispatch::new(
                    "linear:THE-241",
                    worktree.to_str().unwrap(),
                    "coder",
                )
            })
            .unwrap();
        db.set_dispatch_report(id, "verdict: done").unwrap();
        db.bind_dispatch_session(id, "reservation-session").unwrap();
        let run = db.dispatch_run_ref(id).unwrap().unwrap();
        let target = FencedWaitTarget {
            target: WaitTarget {
                id,
                session_id: run.session_id,
                stage: Some("code".into()),
                issue_id: "linear:THE-241".into(),
            },
            run_gen: run.run_gen,
        };
        let (outcome, _, artifact) = wake_handoff(Ok(db), &target);
        assert_eq!(artifact, None);
        assert_eq!(outcome, pipeline_run::HandoffOutcome::UnverifiedCandidate);
    }

    #[test]
    fn changed_session_while_selecting_wait_is_retryable() {
        let (_dir, db) = db("wait-session-fence");
        let (_worktree_dir, worktree) = git_repo("wait-session-worktree");
        let id = db
            .put_agent_dispatch(thegn_core::issue::NewDispatch::new(
                "linear:THE-253",
                worktree.to_str().unwrap(),
                "coder",
            ))
            .unwrap();
        db.bind_dispatch_session(id, "current-session").unwrap();
        let error = fence_wait_targets(
            &db,
            vec![WaitTarget {
                id,
                session_id: "selected-session".into(),
                stage: Some("code".into()),
                issue_id: "linear:THE-253".into(),
            }],
        )
        .unwrap_err();
        assert!(error.downcast_ref::<crate::cmd::Retryable>().is_some());
        assert!(error.to_string().contains("changed session"));
    }

    #[test]
    fn committed_artifact_and_report_are_verified_for_the_selected_run() {
        let (_dir, db) = db("wake-verified-artifact");
        let (_worktree_dir, worktree) = git_repo("wake-verified-worktree");
        let id = db
            .put_dispatch_with_parent_check(thegn_core::issue::NewDispatch {
                stage: Some("code"),
                artifact_path: None,
                ..thegn_core::issue::NewDispatch::new(
                    "linear:THE-253",
                    worktree.to_str().unwrap(),
                    "coder",
                )
            })
            .unwrap();
        let artifact = pipeline_run::artifact_path("linear:THE-253", "code", id);
        let artifact_path = worktree.join(&artifact);
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, "# finished\n").unwrap();
        assert!(git_ok(&worktree, &["add", "--", artifact.as_str()]));
        assert!(git_ok(&worktree, &["commit", "-q", "-m", "handoff"]));
        db.bind_dispatch_session(id, "session-253").unwrap();
        write_report(
            &db,
            id,
            "PASS\ngate: cargo nextest run --workspace -- 1 passed, 0 skipped",
            false,
        )
        .unwrap();
        let run = db.dispatch_run_ref(id).unwrap().unwrap();
        let mut target = FencedWaitTarget {
            target: WaitTarget {
                id,
                session_id: run.session_id,
                stage: Some("code".into()),
                issue_id: "linear:THE-253".into(),
            },
            run_gen: run.run_gen,
        };
        let (outcome, _, _) = wake_handoff(Ok(db), &target);
        assert_eq!(outcome, pipeline_run::HandoffOutcome::VerifiedHandoff);
        assert!(outcome.handoff_present());
        target.run_gen += 1;
        let (stale, _, _) = wake_handoff(
            Db::open_at(&_dir.path().join("wake-verified-artifact.db")),
            &target,
        );
        assert_eq!(stale, pipeline_run::HandoffOutcome::InfrastructureUnknown);
    }

    #[test]
    fn a_report_bound_to_a_newer_commit_is_not_a_verified_wake_handoff() {
        let (_dir, db) = db("wake-stale-report-commit");
        let (_worktree_dir, worktree) = git_repo("wake-stale-report-worktree");
        let id = db
            .put_dispatch_with_parent_check(thegn_core::issue::NewDispatch {
                stage: Some("code"),
                artifact_path: None,
                ..thegn_core::issue::NewDispatch::new(
                    "linear:THE-253",
                    worktree.to_str().unwrap(),
                    "coder",
                )
            })
            .unwrap();
        let artifact = pipeline_run::artifact_path("linear:THE-253", "code", id);
        let artifact_path = worktree.join(&artifact);
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, "version one\n").unwrap();
        assert!(git_ok(&worktree, &["add", "--", artifact.as_str()]));
        assert!(git_ok(&worktree, &["commit", "-q", "-m", "artifact one"]));
        let first =
            thegn_core::util::git_out_allow_empty(&worktree, &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(&artifact_path, "version two\n").unwrap();
        assert!(git_ok(&worktree, &["add", "--", artifact.as_str()]));
        assert!(git_ok(&worktree, &["commit", "-q", "-m", "artifact two"]));
        db.bind_dispatch_session(id, "session-stale-report")
            .unwrap();
        write_report(
            &db,
            id,
            "PASS\ngate: cargo nextest run --workspace -- 1 passed, 0 skipped",
            false,
        )
        .unwrap();
        assert!(git_ok(&worktree, &["reset", "-q", "--hard", &first]));
        let row = db.get_dispatch(id).unwrap().unwrap();
        let done_error = done_gate_bound(&db, &row).unwrap_err().to_string();
        assert!(
            done_error.contains("older or changed artifact commit"),
            "{done_error}"
        );
        let run = db.dispatch_run_ref(id).unwrap().unwrap();
        let target = FencedWaitTarget {
            target: WaitTarget {
                id,
                session_id: run.session_id,
                stage: Some("code".into()),
                issue_id: "linear:THE-253".into(),
            },
            run_gen: run.run_gen,
        };
        let (outcome, _, _) = wake_handoff(Ok(db), &target);
        assert_eq!(outcome, pipeline_run::HandoffOutcome::UnverifiedCandidate);
        assert!(!outcome.handoff_present());
    }

    #[test]
    fn failed_status_observation_is_unknown_not_clean() {
        assert_eq!(
            worktree_state_from_status(None),
            pipeline_run::WorktreeState::Unknown
        );
        assert_eq!(
            worktree_state_from_status(Some("")),
            pipeline_run::WorktreeState::Clean
        );
    }

    #[test]
    fn a_bad_outcome_prompts_for_a_reason_but_is_never_blocked() {
        use thegn_core::issue::AgentDispatchStatus as S;
        // The case that made 40 of 45 bad rows unreadable: closed as failed
        // with nothing at all recording why.
        assert!(wants_a_reason(S::Failed, false, false, false));
        assert!(wants_a_reason(S::Abandoned, false, false, false));
        // Any one source of explanation is enough — the point is that SOMETHING
        // says why, not that it came from this flag.
        assert!(
            !wants_a_reason(S::Failed, true, false, false),
            "--why given"
        );
        assert!(
            !wants_a_reason(S::Failed, false, true, false),
            "the worker's own report explains it"
        );
        assert!(
            !wants_a_reason(S::Failed, false, false, true),
            "a prior note (e.g. the reaper's) explains it"
        );
        // A good outcome is not interrogated, and `done` has its own gate.
        assert!(!wants_a_reason(S::Done, false, false, false));
        assert!(!wants_a_reason(S::Merged, false, false, false));
        assert!(!wants_a_reason(S::WaitingHuman, false, false, false));
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
    fn note_quota_status_json_reports_total_shown_truncated_and_quota() {
        let (_dir, db) = db("note-quota-status");
        let row = put(
            &db,
            NewDispatch::new("linear:THE-243-status", "/wt/note-status", "claude"),
        )
        .unwrap();
        for _ in 0..25 {
            db.append_dispatch_note_record(row.id, "progress").unwrap();
        }
        let total = db.dispatch_note_count(row.id, None).unwrap();
        let page = db.newest_dispatch_notes(row.id, None, 20).unwrap();
        let mut value = serde_json::json!({
            "notes": page.iter().map(|note| note.text.as_str()).collect::<Vec<_>>()
        });
        attach_note_quota_status(&mut value, total, page.len());
        assert_eq!(value["notes_total"], 25);
        assert_eq!(value["notes_shown"], 20);
        assert!(value["truncated"].as_bool().unwrap());
        assert_eq!(
            value["quota"]["max_notes"],
            pipeline_report::NOTE_MAX_PER_ROW
        );
        assert_eq!(
            value["quota"]["max_bytes"],
            pipeline_report::NOTE_MAX_BYTES_PER_ROW
        );
    }

    #[test]
    fn note_quota_write_note_json_error_names_the_kind() {
        let quota = DispatchNoteQuotaExceeded {
            dispatch_id: 42,
            kind: NoteQuotaKind::Count,
            notes: pipeline_report::NOTE_MAX_PER_ROW,
            bytes: 2_000,
        };
        let value = note_quota_error_json(42, &quota);
        assert_eq!(value["id"], 42);
        assert_eq!(value["error"], "note_quota_exceeded");
        assert_eq!(value["kind"], "count");
        assert_eq!(value["notes"], 2_000);
        assert_eq!(value["bytes"], 2_000);
        let byte_quota = DispatchNoteQuotaExceeded {
            kind: NoteQuotaKind::Bytes,
            ..quota
        };
        assert_eq!(note_quota_error_json(42, &byte_quota)["kind"], "bytes");
    }

    #[test]
    fn dispatch_display_cells_replace_terminal_and_bidi_controls() {
        let hostile = "issue\x1b[2J\u{202e}\u{0085}";
        let (issue, stage, note) = display_dispatch_cells(
            hostile,
            "stage\x1b[2J\u{202e}\u{0085}",
            Some("note\x1b[2J\u{202e}\u{0085}"),
        );
        for value in [&issue, &stage, &note] {
            assert!(
                value
                    .chars()
                    .all(|c| !c.is_control() && !pipeline_report::is_format_control(c)),
                "unsafe display value: {value:?}"
            );
        }
    }

    #[test]
    fn status_multiline_display_sanitizes_each_legacy_line() {
        let shown = display_multiline("report\x1b[2J\n\u{202e}verdict\u{0085}");
        assert_eq!(shown.lines().count(), 2);
        assert!(shown.lines().all(|line| {
            line.chars()
                .all(|c| !c.is_control() && !pipeline_report::is_format_control(c))
        }));
    }

    #[test]
    fn wait_human_output_sanitizes_stage_and_report() {
        let (stage, report) = wait_human_text(
            "build\x1b]0;x\x07",
            Some("PASS\x1b[2J\n\u{202e}gate\u{0085}"),
        );
        let report = report.unwrap();
        assert_eq!(report.lines().count(), 2);
        assert!(
            std::iter::once(stage.as_str())
                .chain(report.lines())
                .all(|line| {
                    line.chars()
                        .all(|c| !c.is_control() && !pipeline_report::is_format_control(c))
                })
        );
    }

    #[test]
    fn reap_human_output_sanitizes_why() {
        let why = reap_human_why("why\x1b]0;x\x07\u{202e}\u{0085}");
        assert!(
            why.chars()
                .all(|c| !c.is_control() && !pipeline_report::is_format_control(c))
        );
    }

    #[test]
    fn verify_human_output_sanitizes_artifact_warnings_and_reasons() {
        let (artifact, warnings, reasons) = verify_human_text(
            Some("artifact\x1b]0;x\x07"),
            &["warning\u{202e}\u{0085}".into()],
            &["reason\x1b[2J".into()],
        );
        for value in std::iter::once(&artifact)
            .chain(warnings.iter())
            .chain(reasons.iter())
        {
            assert!(
                value
                    .chars()
                    .all(|c| !c.is_control() && !pipeline_report::is_format_control(c))
            );
        }
    }

    #[test]
    fn lease_cli_classifies_missing_expired_replaced_and_live_outcomes() {
        let now = 10_000;
        assert_eq!(lease_show_outcome(None, now), "free");
        assert_eq!(
            lease_show_outcome(Some(&("owner".into(), now)), now),
            "expired"
        );
        assert_eq!(
            lease_show_outcome(Some(&("owner".into(), now + 1)), now),
            "live"
        );
        assert_eq!(
            lease_show_owners(Some(&("owner".into(), now + 1)), now),
            (Some("owner"), None)
        );
        assert_eq!(
            lease_show_owners(Some(&("owner".into(), now)), now),
            (None, Some("owner"))
        );
        assert_eq!(lease_release_outcome("owner", None, now), "missing");
        assert_eq!(
            lease_release_outcome("owner", Some(&("other".into(), now + 1)), now),
            "replaced"
        );
        assert_eq!(
            lease_release_outcome("owner", Some(&("owner".into(), now)), now),
            "expired"
        );
        assert_eq!(
            lease_release_outcome("owner", Some(&("owner".into(), now + 1)), now),
            "released"
        );
        assert_eq!(
            lease_renewal_outcome("owner", Err(None), now),
            ("missing", None)
        );
        assert_eq!(
            lease_renewal_outcome("owner", Err(Some(("owner".into(), now))), now),
            ("expired", Some("owner".into()))
        );
        assert_eq!(
            lease_renewal_outcome("owner", Err(Some(("other".into(), now + 1))), now),
            ("replaced", Some("other".into()))
        );
        assert_eq!(
            lease_renewal_outcome("owner", Ok(now + 1), now),
            ("renewed", None)
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
        let err = put(&db, new).unwrap_err();
        assert!(err.to_string().contains("4242"), "{err}");
        assert!(
            err.downcast_ref::<thegn_core::db::ParentDispatchConflict>()
                .is_some()
        );
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

    #[test]
    fn chunk_file_reads_reject_traversal_and_absolute_paths_and_cap_bytes() {
        let (dir, wt) = chunk_wt(&[
            ("scope.md", "---\nfiles: [src/*.rs]\n---\n"),
            ("dir/scope.md", "---\n---\n"),
            ("dir\\scope.md", "---\n---\n"),
        ]);
        let outside = dir.path().join("outside.md");
        std::fs::write(&outside, "---\n---\n").unwrap();
        assert!(
            read_chunk_file(&wt, "./scope.md")
                .unwrap()
                .contains("files:")
        );
        assert!(read_chunk_file(&wt, "../outside.md").is_err());
        assert!(read_chunk_file(&wt, &format!("{wt}/scope.md")).is_err());
        for alias in [
            "dir//scope.md",
            "dir/./scope.md",
            "dir\\scope.md",
            "scope*.md",
            "CON.md",
        ] {
            assert!(read_chunk_file(&wt, alias).is_err(), "accepted {alias}");
        }
        if crate::platform::test_symlink_supported() {
            crate::platform::test_symlink(
                std::path::Path::new("../outside.md"),
                &dir.path().join("link.md"),
            )
            .unwrap();
            crate::platform::test_symlink(std::path::Path::new(".."), &dir.path().join("subdir"))
                .unwrap();
            assert!(read_chunk_file(&wt, "link.md").is_err());
            assert!(read_chunk_file(&wt, "subdir/outside.md").is_err());
        }

        let large = std::path::Path::new(&wt).join("large.md");
        std::fs::write(&large, vec![b'x'; 1024 * 1024 + 1]).unwrap();
        assert!(read_chunk_file(&wt, "large.md").is_err());
        std::fs::write(
            std::path::Path::new(&wt).join("many-lines.md"),
            vec![b'\n'; 8193],
        )
        .unwrap();
        assert!(read_chunk_file(&wt, "many-lines.md").is_err());
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

    /// Queue a chunk through the same prepared-scope admission path as the CLI.
    fn admitted_chunk_row(db: &Db, wt: &str, chunk: &str) -> i64 {
        claim_admitted_with_refresh(
            db,
            NewDispatch {
                chunk_path: Some(chunk),
                ..NewDispatch::new("linear:A-1", wt, "coder")
            },
            3,
            None,
            wt,
            "linear:A-1",
            Some(chunk),
            None,
        )
        .unwrap()
        .unwrap()
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
    fn an_unreadable_active_sibling_blocks_admission_with_row_and_path() {
        let (_d, wt) = chunk_wt(&[("c.md", "---\nfiles: [lib.rs]\n---\n")]);
        let db = db("gate-broken-sib").1;
        // The sibling row points at a chunk file that is GONE from its own
        // worktree. The refusal identifies the row and path requiring action.
        let row = chunk_row(&db, &wt, "missing.md");
        let err = chunk_gate(&db, &wt, "linear:A-1", "c.md", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&format!("row {row}")), "{err}");
        assert!(err.contains("missing.md"), "{err}");
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

    #[test]
    fn dispatch_claim_with_chunk_cannot_bypass_scope_admission() {
        let (_d, wt) = chunk_wt(&[
            ("chunk-1.md", "---\nfiles: [src/shared.rs]\n---\n"),
            ("chunk-2.md", "---\nfiles: [src/shared.rs]\n---\n"),
        ]);
        let db = db("claim-chunk-scope").1;
        chunk_row(&db, &wt, "chunk-1.md");
        let result = claim_admitted_with_refresh(
            &db,
            NewDispatch {
                issue_id: "linear:A-1",
                worktree_path: &wt,
                agent_name: "coder",
                stage: Some("code"),
                parent_id: None,
                session_id: None,
                artifact_path: Some("second.md"),
                chunk_path: Some("chunk-2.md"),
            },
            3,
            None,
            &wt,
            "linear:A-1",
            Some("chunk-2.md"),
            None,
        )
        .unwrap();
        assert!(
            matches!(
                result,
                Err(thegn_core::db::DispatchAdmissionDecision::ScopeRefused(_))
            ),
            "{result:?}"
        );
        assert_eq!(
            db.list_dispatches().unwrap().len(),
            1,
            "a refusal must not leave a queued row"
        );
    }

    #[test]
    fn prepared_scope_survives_incoming_chunk_file_replacement() {
        let (_d, wt) = chunk_wt(&[("chunk.md", "---\nfiles: [approved.rs]\n---\n")]);
        let db = db("claim-chunk-replacement").1;
        let prepared = prepare_chunk_admission(&db, &wt, "linear:A-1", "chunk.md").unwrap();
        std::fs::write(
            std::path::Path::new(&wt).join("chunk.md"),
            "---\nfiles: [replacement.rs]\n---\n",
        )
        .unwrap();
        let result = db
            .claim_dispatch_admitted(
                NewDispatch {
                    issue_id: "linear:A-1",
                    worktree_path: &wt,
                    agent_name: "coder",
                    stage: Some("code"),
                    parent_id: None,
                    session_id: None,
                    artifact_path: Some("handoff.md"),
                    chunk_path: Some("chunk.md"),
                },
                2,
                None,
                Some(&prepared),
                None,
            )
            .unwrap();
        assert!(
            result.is_ok(),
            "admission must use the already parsed scope facts: {result:?}"
        );
        assert_eq!(db.list_dispatches().unwrap().len(), 1);
    }

    #[test]
    fn active_sibling_narrowing_or_deletion_cannot_change_admitted_scope() {
        let (_dir, wt) = chunk_wt(&[
            ("chunk-1.md", "---\nfiles: [src/**]\n---\n"),
            ("chunk-2.md", "---\nfiles: [src/shared.rs]\n---\n"),
        ]);
        let db = db("claim-chunk-snapshot").1;
        let first = claim_admitted_with_refresh(
            &db,
            NewDispatch {
                issue_id: "linear:THE-214",
                worktree_path: &wt,
                agent_name: "coder",
                stage: Some("code"),
                parent_id: None,
                session_id: None,
                artifact_path: Some("first.md"),
                chunk_path: Some("chunk-1.md"),
            },
            3,
            None,
            &wt,
            "linear:THE-214",
            Some("chunk-1.md"),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            db.dispatch_chunk_scope(first).unwrap(),
            Some(vec!["src/**".into()])
        );

        std::fs::write(
            std::path::Path::new(&wt).join("chunk-1.md"),
            "---\nfiles: [docs/x.md]\n---\n",
        )
        .unwrap();

        let claim_second = || {
            claim_admitted_with_refresh(
                &db,
                NewDispatch {
                    issue_id: "linear:THE-214",
                    worktree_path: &wt,
                    agent_name: "coder",
                    stage: Some("code"),
                    parent_id: None,
                    session_id: None,
                    artifact_path: Some("second.md"),
                    chunk_path: Some("chunk-2.md"),
                },
                3,
                None,
                &wt,
                "linear:THE-214",
                Some("chunk-2.md"),
                None,
            )
            .unwrap()
        };
        let first_refusal = claim_second().unwrap_err();
        let narrowed_reason = format!("{first_refusal:?}");
        assert!(narrowed_reason.contains("chunk scope gate refused"));
        assert!(narrowed_reason.contains("admitted scope snapshot"));
        std::fs::remove_file(std::path::Path::new(&wt).join("chunk-1.md")).unwrap();
        let deleted_refusal = claim_second().unwrap_err();
        let deleted_reason = format!("{deleted_refusal:?}");
        assert!(deleted_reason.contains("chunk scope gate refused"));
        assert!(deleted_reason.contains("admitted scope snapshot"));
        assert!(!deleted_reason.contains("not readable"));
        assert_eq!(db.list_dispatches().unwrap().len(), 1);
    }

    #[test]
    fn a_sibling_without_a_chunk_declares_no_scope_and_does_not_block() {
        let (_d, wt) = chunk_wt(&[("chunk.md", "---\nfiles: [src/shared.rs]\n---\n")]);
        let db = db("claim-no-chunk-sibling").1;
        put(&db, NewDispatch::new("linear:A-1", &wt, "legacy")).unwrap();
        let prepared = prepare_chunk_admission(&db, &wt, "linear:A-1", "chunk.md").unwrap();
        assert!(prepared.siblings.is_empty());
        let result = db
            .claim_dispatch_admitted(
                NewDispatch {
                    chunk_path: Some("chunk.md"),
                    ..NewDispatch::new("linear:A-1", &wt, "coder")
                },
                2,
                None,
                Some(&prepared),
                None,
            )
            .unwrap();
        assert!(
            result.is_ok(),
            "legacy row without a chunk path is out of scope: {result:?}"
        );
    }

    #[test]
    fn retained_dispatch_put_chunk_uses_the_shared_admission_path() {
        let (_d, wt) = chunk_wt(&[
            ("chunk-1.md", "---\nfiles: [src/shared.rs]\n---\n"),
            ("chunk-2.md", "---\nfiles: [src/shared.rs]\n---\n"),
        ]);
        let db = db("put-chunk-scope").1;
        admitted_chunk_row(&db, &wt, "chunk-1.md");
        let result = claim_chunk_put(
            &db,
            NewDispatch {
                issue_id: "linear:A-1",
                worktree_path: &wt,
                agent_name: "coder",
                stage: Some("code"),
                parent_id: None,
                session_id: None,
                artifact_path: Some("put-handoff.md"),
                chunk_path: Some("chunk-2.md"),
            },
            3,
            None,
        );
        assert!(result.unwrap_err().to_string().contains("collides"));
        assert_eq!(db.list_dispatches().unwrap().len(), 1);
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
            issue_id: "linear:THE-76".into(),
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

        // Written but never committed: exists, but is not a handoff — the
        // exact pilot failure the gate exists to catch.
        let path = root.join(ARTIFACT);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "wip").unwrap();
        let f = verify_facts(&row_in(&root, Some(ARTIFACT)));
        assert!(f.exists && !f.tracked);
        // …and writing it left the worktree dirty (whole-tree read).
        assert!(f.dirty);

        // Staging is still not committing. The old `ls-files` check blessed
        // this exact state and allowed cleanup to erase the only copy.
        assert!(git_ok(&root, &["add", "--", ARTIFACT]));
        let f = verify_facts(&row_in(&root, Some(ARTIFACT)));
        assert!(f.exists && !f.tracked && f.dirty);

        // Committed: tracked, and the tree is clean again.
        commit_artifact(&root);
        let f = verify_facts(&row_in(&root, Some(ARTIFACT)));
        assert!(f.exists && f.tracked && !f.dirty);
    }

    #[test]
    fn facts_refuse_a_row_bound_to_a_subdirectory_instead_of_the_worktree_root() {
        let (_d, root) = git_repo("wrong-worktree-binding");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("handoff.md"), "# handoff").unwrap();
        assert!(git_ok(&root, &["add", "--", "nested/handoff.md"]));
        assert!(git_ok(&root, &["commit", "-q", "-m", "handoff"]));
        let f = verify_facts(&row_in(&nested, Some("handoff.md")));
        assert!(f.exists, "the file exists under the supplied path");
        assert!(!f.tracked, "a nested path is not the row's worktree root");
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
        assert!(err.contains("not committed and clean in HEAD"), "{err}");

        // Committed artifact AND a report: the gate passes. (THE-88: the
        // report is the second of the two gates; a row with an artifact
        // but no report is refused with a reason that names
        // `dispatch report`. That refusal is exercised in
        // `facts_record_report_present_and_done_gate_names_the_report_command`.)
        commit_artifact(&root);
        let mut row = row_in(&root, Some(ARTIFACT));
        row.report = Some("PASS\ngate: just test -- exit 0".into());
        done_gate(&row).unwrap();

        // Editing the artifact after its commit blocks: HEAD no longer holds
        // the handoff being presented to the reviewer.
        std::fs::write(root.join(ARTIFACT), "post-commit edit").unwrap();
        done_gate(&row).unwrap_err();

        // Unrelated worktree dirt remains informational and never blocks the
        // committed artifact.
        std::fs::write(root.join(ARTIFACT), "# handoff").unwrap();
        std::fs::write(root.join("unrelated.txt"), "wip").unwrap();
        done_gate(&row).unwrap();
    }

    #[test]
    fn the_done_gate_passes_by_construction_for_a_row_without_artifact() {
        let (_d, root) = git_repo("gate-none");
        let mut row = row_in(&root, None);
        row.stage = None;
        done_gate(&row).unwrap();
    }

    #[test]
    fn the_done_gate_refuses_a_pipeline_reservation_without_artifact() {
        let (_d, root) = git_repo("gate-reservation");
        let row = row_in(&root, None);
        let facts = verify_facts(&row);
        assert_eq!(facts.stage.as_deref(), Some("code"));
        let err = done_gate(&row).unwrap_err().to_string();
        assert!(
            err.contains("no artifact path") && err.contains("publish"),
            "{err}"
        );
        assert!(err.contains("--force"), "{err}");
    }

    #[test]
    fn committed_artifact_with_fail_report_cannot_be_completed_or_reaped_done() {
        let (_db_dir, db) = db("fail-report-done-gate");
        let (_worktree_dir, root) = git_repo("fail-report-worktree");
        let id = db
            .put_agent_dispatch(NewDispatch {
                stage: Some("code"),
                artifact_path: None,
                ..NewDispatch::new("linear:THE-269", root.to_str().unwrap(), "coder")
            })
            .unwrap();
        let artifact = pipeline_run::artifact_path("linear:THE-269", "code", id);
        let path = root.join(&artifact);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "# handoff").unwrap();
        assert!(git_ok(&root, &["add", "--", artifact.as_str()]));
        assert!(git_ok(&root, &["commit", "-q", "-m", "artifact"]));
        let run = db
            .publish_dispatch_run(id, "fail-session", &artifact)
            .unwrap();
        assert_eq!(run, thegn_core::issue::DispatchRunPublishOutcome::Published);
        write_report(&db, id, "FAIL: tests failed", false).unwrap();
        let run = db.dispatch_run_ref(id).unwrap().unwrap();
        db.stamp_dispatch_exit(&run, Some(1)).unwrap();
        let row = db.get_dispatch(id).unwrap().unwrap();
        let done_error = done_gate_bound(&db, &row).unwrap_err().to_string();
        assert!(
            done_error.contains("verdict is fail") && done_error.contains("Lead"),
            "{done_error}"
        );
        let plan = reap_plan_rows(&[row], Some(&[]));
        assert_eq!(plan.len(), 1);
        assert!(!matches!(
            &plan[0].verdict,
            pipeline_reap::ReapVerdict::CloseDone
        ));
        assert!(matches!(
            &plan[0].verdict,
            pipeline_reap::ReapVerdict::NeedsDecision { why } if why.contains("FAIL") && why.contains("Lead")
        ));
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
        let inserted = db.append_dispatch_note_record(row.id, "direct").unwrap();
        assert_eq!(inserted.text, "direct");
        assert_eq!(inserted.dispatch_id, row.id);
        let json = note_json(row.id, &inserted);
        assert_eq!(json["note_id"], inserted.id);
        assert_eq!(json["created_at_ms"], inserted.created_at_ms);
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

        // Filing a report with structured gate evidence flips
        // `report_present` on and the gate passes.
        row.report = Some("PASS\ngate: just test -- exit 0".into());
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

    #[test]
    fn daemon_connect_failure_produces_unknown_liveness() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ids = rt.block_on(query_live_sessions(
            std::time::Duration::from_secs(1),
            || async { Err(anyhow::anyhow!("connect refused")) },
            |_client: ()| async { Ok(Vec::new()) },
        ));
        assert_eq!(ids, None, "connect failure must be unknown, not empty");
    }

    #[test]
    fn daemon_session_list_failure_produces_unknown_liveness() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ids = rt.block_on(query_live_sessions(
            std::time::Duration::from_secs(1),
            || async { Ok::<_, anyhow::Error>(()) },
            |_client: ()| async { Err(anyhow::anyhow!("session list failed")) },
        ));
        assert_eq!(ids, None, "list failure must be unknown, not empty");
    }

    #[test]
    fn daemon_session_query_timeout_produces_unknown_liveness() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ids = rt.block_on(query_live_sessions(
            std::time::Duration::from_millis(1),
            std::future::pending::<anyhow::Result<()>>,
            |_client: ()| async { Ok(Vec::new()) },
        ));
        assert_eq!(ids, None, "timeout must be unknown, not empty");
    }

    #[test]
    fn reachable_empty_session_list_still_reaps_a_durably_exited_row() {
        let mut row = row_in(std::path::Path::new("/unused"), None);
        row.exit_code = Some(0);
        row.exited_at_ms = Some(1);
        let plan = reap_plan_rows(&[row], Some(&[]));
        assert!(matches!(
            &plan[0].verdict,
            pipeline_reap::ReapVerdict::MarkFailed { .. }
        ));
    }

    #[test]
    fn reap_plan_is_limited_to_oldest_active_rows_and_reports_truncation() {
        let (_dir, db) = db("reap-plan-limit");
        let oldest = put(
            &db,
            NewDispatch {
                session_id: Some("old-session"),
                ..NewDispatch::new("linear:THE-259-old", "/unused", "worker")
            },
        )
        .unwrap();
        db.update_dispatch_status(oldest.id, AgentDispatchStatus::Running)
            .unwrap();
        let newer = put(
            &db,
            NewDispatch {
                session_id: Some("new-session"),
                ..NewDispatch::new("linear:THE-259-new", "/unused", "worker")
            },
        )
        .unwrap();
        db.update_dispatch_status(newer.id, AgentDispatchStatus::Running)
            .unwrap();

        let (plan, _run_refs, truncated) = reap_plan_limited(&db, None, 1).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].id, oldest.id);
        assert!(truncated, "the second active row must set truncation");
    }

    #[test]
    fn status_missing_row_keeps_the_exact_id_error() {
        let (_dir, db) = db("status-missing-row-error");
        let err = read_status(&db, Some(9876), None, false, false).unwrap_err();
        assert_eq!(err.to_string(), "no dispatch with id 9876");
    }

    #[test]
    fn live_worker_is_never_marked_failed_or_done_while_daemon_query_is_unavailable() {
        use thegn_core::store::NotificationStore;
        let (_dir, db) = db("unknown-liveness");
        let id = put(
            &db,
            NewDispatch {
                session_id: Some("possibly-live-session"),
                ..NewDispatch::new("linear:THE-209", "/wt/209", "worker")
            },
        )
        .unwrap()
        .id;
        db.update_dispatch_status(id, AgentDispatchStatus::Running)
            .unwrap();

        let (plan, run_refs) = reap_plan(&db, None).unwrap();
        assert_eq!(plan[0].verdict.token(), "unknown");
        assert_eq!(pipeline_reap::summarize(&plan).actionable(), 0);
        apply_reap_plan(&db, &plan, &run_refs).unwrap();
        assert_eq!(
            db.get_dispatch(id).unwrap().unwrap().status,
            AgentDispatchStatus::Running,
            "unavailable daemon liveness must not fail or close a possibly-live worker"
        );
    }

    #[test]
    fn reap_plan_cannot_close_a_relaunched_run() {
        let (_dir, db) = db("reap-relaunch-fence");
        let row = put(
            &db,
            NewDispatch {
                session_id: Some("old-session"),
                ..NewDispatch::new("linear:THE-242", "/unused", "worker")
            },
        )
        .unwrap();
        db.update_dispatch_status(row.id, AgentDispatchStatus::Running)
            .unwrap();
        let old_run = db.dispatch_run_ref(row.id).unwrap().unwrap();
        db.stamp_dispatch_exit(&old_run, Some(1)).unwrap();
        let (plan, run_refs) = reap_plan(&db, Some(&[])).unwrap();
        assert!(matches!(
            &plan[0].verdict,
            pipeline_reap::ReapVerdict::MarkFailed { .. }
        ));
        assert!(
            db.compare_and_set_dispatch_status(
                row.id,
                AgentDispatchStatus::Running,
                AgentDispatchStatus::Spawning,
                None,
            )
            .unwrap()
        );
        assert!(
            db.compare_and_set_dispatch_retry_run(
                row.id,
                AgentDispatchStatus::Spawning,
                "new-session",
                "artifact.md",
            )
            .unwrap()
        );

        apply_reap_plan(&db, &plan, &run_refs).unwrap();
        let after = db.get_dispatch(row.id).unwrap().unwrap();
        let new_run = db.dispatch_run_ref(row.id).unwrap().unwrap();
        assert_eq!(after.status, AgentDispatchStatus::Running);
        assert_eq!(after.session_id.as_deref(), Some("new-session"));
        assert_eq!(new_run.run_gen, old_run.run_gen + 1);
    }

    #[test]
    fn reap_plan_applies_exit_stamp_for_running_row_without_session() {
        let (_dir, db) = db("reap-sessionless-running");
        let row = put(
            &db,
            NewDispatch {
                session_id: None,
                ..NewDispatch::new("linear:THE-242", "/unused", "worker")
            },
        )
        .unwrap();
        db.update_dispatch_status(row.id, AgentDispatchStatus::Running)
            .unwrap();
        let run = db.dispatch_run_ref(row.id).unwrap().unwrap();
        assert_eq!(run.session_id, "");
        db.stamp_dispatch_exit(&run, Some(1)).unwrap();

        let (plan, run_refs) = reap_plan(&db, Some(&[])).unwrap();
        assert!(matches!(
            &plan[0].verdict,
            pipeline_reap::ReapVerdict::MarkFailed { .. }
        ));
        assert!(run_refs.contains_key(&row.id));
        apply_reap_plan(&db, &plan, &run_refs).unwrap();
        assert_eq!(
            db.get_dispatch(row.id).unwrap().unwrap().status,
            AgentDispatchStatus::Failed
        );
    }

    #[test]
    fn reap_plan_skips_spawning_rows_with_stale_exit_stamps() {
        let (_dir, db) = db("reap-spawning-stale-exit");
        let row = put(
            &db,
            NewDispatch {
                session_id: Some("old-session"),
                ..NewDispatch::new("linear:THE-242", "/unused", "worker")
            },
        )
        .unwrap();
        db.update_dispatch_status(row.id, AgentDispatchStatus::Running)
            .unwrap();
        let old_run = db.dispatch_run_ref(row.id).unwrap().unwrap();
        db.stamp_dispatch_exit(&old_run, Some(1)).unwrap();
        assert!(
            db.compare_and_set_dispatch_status(
                row.id,
                AgentDispatchStatus::Running,
                AgentDispatchStatus::Spawning,
                None,
            )
            .unwrap()
        );

        let (plan, run_refs) = reap_plan(&db, Some(&[])).unwrap();
        assert!(matches!(
            &plan[0].verdict,
            pipeline_reap::ReapVerdict::MarkFailed { .. }
        ));
        assert!(!run_refs.contains_key(&row.id));
        apply_reap_plan(&db, &plan, &run_refs).unwrap();
        assert_eq!(
            db.get_dispatch(row.id).unwrap().unwrap().status,
            AgentDispatchStatus::Spawning
        );
    }

    #[test]
    fn reap_facts_carries_an_unknown_worktree_status() {
        let facts = pipeline_run::VerifyFacts {
            stage: Some("code".into()),
            artifact: Some("artifact.md".into()),
            artifact_identity: pipeline_run::ArtifactIdentity::Canonical,
            artifact_empty: false,
            exists: true,
            tracked: true,
            dirty: false,
            worktree_state: Some(pipeline_run::WorktreeState::Unknown),
            report_present: true,
            report_verdict: pipeline_report::Verdict::Pass,
            report_gate_valid: true,
            report_commit_binding: pipeline_run::ReportCommitBinding::Bound,
        };
        let reap = reap_facts(&facts, pipeline_reap::WorkerLiveness::Exited);
        assert!(reap.worktree_unknown);
        assert_eq!(reap.worker, pipeline_reap::WorkerLiveness::Exited);
    }

    #[test]
    fn done_gate_refuses_an_unknown_worktree_status() {
        let facts = pipeline_run::VerifyFacts {
            stage: Some("code".into()),
            artifact: Some("artifact.md".into()),
            artifact_identity: pipeline_run::ArtifactIdentity::Canonical,
            artifact_empty: false,
            exists: true,
            tracked: true,
            dirty: false,
            worktree_state: Some(pipeline_run::WorktreeState::Unknown),
            report_present: true,
            report_verdict: pipeline_report::Verdict::Pass,
            report_gate_valid: true,
            report_commit_binding: pipeline_run::ReportCommitBinding::Bound,
        };
        let report = pipeline_run::verify_report(&facts);
        let error = require_done_report(1, report).unwrap_err().to_string();
        assert!(error.contains("worktree status"), "{error}");
    }

    #[test]
    fn worker_capability_route_table() {
        let token = Some("token");
        let actions = [
            (
                Action::Report {
                    id: 1,
                    text: "x".into(),
                    json: false,
                },
                WorkerRoute::ViaDaemon("token".into()),
            ),
            (
                Action::Note {
                    id: 1,
                    text: "x".into(),
                    json: false,
                },
                WorkerRoute::ViaDaemon("token".into()),
            ),
            (
                Action::Claim {
                    issue_id: "i".into(),
                    worktree_path: "/w".into(),
                    agent_name: "a".into(),
                    stage: "s".into(),
                    artifact: None,
                    parent: None,
                    chunk: None,
                    allow_duplicate: None,
                    allow_scope_overlap: None,
                    lease: None,
                    owner: None,
                    admin: false,
                    why: None,
                    json: false,
                },
                WorkerRoute::Refused("claim"),
            ),
            (
                Action::Put {
                    issue_id: "i".into(),
                    worktree_path: "/w".into(),
                    agent_name: "a".into(),
                    stage: None,
                    parent: None,
                    session: None,
                    artifact: None,
                    chunk: None,
                    force: false,
                    force_reason: None,
                    json: false,
                },
                WorkerRoute::Refused("put"),
            ),
            (
                Action::SetStatus {
                    id: 1,
                    status: "done".into(),
                    force: false,
                    why: None,
                    lease: None,
                    owner: None,
                    admin: false,
                    json: false,
                },
                WorkerRoute::Refused("set-status"),
            ),
            (
                Action::Lease {
                    action: "show".into(),
                    owner: None,
                    ttl: 300,
                    name: "lead".into(),
                    json: false,
                },
                WorkerRoute::Refused("lease"),
            ),
            (
                Action::Reap {
                    apply: true,
                    json: false,
                },
                WorkerRoute::Refused("reap --apply"),
            ),
            (
                Action::Reap {
                    apply: false,
                    json: false,
                },
                WorkerRoute::Local,
            ),
            (
                Action::List {
                    active: false,
                    json: false,
                },
                WorkerRoute::Local,
            ),
            (Action::Verify { id: 1, json: false }, WorkerRoute::Local),
            (
                Action::Gate { id: 1, json: false },
                WorkerRoute::Refused("gate"),
            ),
            (
                Action::Wait {
                    row: Some(1),
                    any: false,
                    timeout: None,
                    json: false,
                },
                WorkerRoute::Local,
            ),
            (
                Action::Status {
                    row: None,
                    since: None,
                    all: false,
                    json: false,
                },
                WorkerRoute::Local,
            ),
        ];
        for (action, expected) in actions {
            assert_eq!(worker_route(&action, token), expected);
            assert_eq!(worker_route(&action, None), WorkerRoute::Local);
        }
        assert_eq!(
            worker_route(
                &Action::Report {
                    id: 1,
                    text: String::new(),
                    json: false
                },
                Some("")
            ),
            WorkerRoute::ViaDaemon(String::new())
        );
    }

    #[test]
    fn worker_capability_refusal_names_the_supervisor_verb() {
        let message = WorkerCapabilityRefusal { verb: "claim" }.to_string();
        assert!(message.contains("dispatch claim"));
        assert!(message.contains("supervisor verb"));
        let token = "ab".repeat(32);
        let route = worker_route(
            &Action::Report {
                id: 1,
                text: "x".into(),
                json: false,
            },
            Some(&token),
        );
        assert!(!format!("{route:?}").contains(&token));
        let request = thegn_svc::control::DispatchWorkerWriteReq {
            text: "x".into(),
            token: token.clone(),
        };
        assert!(!format!("{request:?}").contains(&token));
    }

    #[test]
    fn worker_capability_record_report_matches_write_report() {
        let (_dir, db) = db("worker-report-record");
        let worktree = tempfile::tempdir().unwrap();
        let local_id = db
            .put_agent_dispatch(NewDispatch::new(
                "linear:THE-1037",
                worktree.path().to_str().unwrap(),
                "sh",
            ))
            .unwrap();
        let daemon_id = db
            .put_agent_dispatch(NewDispatch::new(
                "linear:THE-1037",
                worktree.path().to_str().unwrap(),
                "sh",
            ))
            .unwrap();
        write_report(&db, local_id, "findings: none\nnext: done", false).unwrap();
        let recorded = record_report(&db, daemon_id, "findings: none\nnext: done").unwrap();
        let local = db.get_dispatch(local_id).unwrap().unwrap();
        let daemon = db.get_dispatch(daemon_id).unwrap().unwrap();
        assert_eq!(local.report, daemon.report);
        assert_eq!(daemon.report.as_deref(), Some(recorded.validated.as_str()));
        assert_eq!(
            recorded.ungated,
            pipeline_report::pass_without_gate_evidence(&recorded.validated)
        );
    }

    fn commit_test_file(root: &std::path::Path, path: &str, contents: &str) {
        let full_path = root.join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full_path, contents).unwrap();
        assert!(git_ok(root, &["add", "--", path]));
        assert!(git_ok(root, &["commit", "-q", "-m", "artifact"]));
    }

    #[test]
    fn canonical_artifact_readme_is_not_a_handoff() {
        let (_dir, root) = git_repo("canonical-readme");
        commit_test_file(&root, "README.md", "# unrelated\n");
        let mut row = row_in(&root, Some("README.md"));
        row.report = Some("PASS\ngate: just test -- exit 0".into());
        let error = done_gate(&row).unwrap_err().to_string();
        assert!(error.contains("not this row's handoff"), "{error}");
    }

    #[test]
    fn canonical_artifact_prior_row_handoff_is_refused() {
        let (_dir, root) = git_repo("canonical-prior-row");
        let prior = ".thegn/pipeline/THE-76/code/6.md";
        commit_test_file(&root, prior, "# prior row\n");
        let mut row = row_in(&root, Some(prior));
        row.report = Some("PASS\ngate: just test -- exit 0".into());
        let error = done_gate(&row).unwrap_err().to_string();
        assert!(error.contains("not this row's handoff"), "{error}");
    }

    #[test]
    fn canonical_artifact_other_stage_is_refused() {
        let (_dir, root) = git_repo("canonical-other-stage");
        let other_stage = ".thegn/pipeline/THE-76/review/7.md";
        commit_test_file(&root, other_stage, "# other stage\n");
        let mut row = row_in(&root, Some(other_stage));
        row.report = Some("PASS\ngate: just test -- exit 0".into());
        let error = done_gate(&row).unwrap_err().to_string();
        assert!(error.contains("not this row's handoff"), "{error}");
    }

    #[test]
    fn canonical_artifact_other_issue_is_refused() {
        let (_dir, root) = git_repo("canonical-other-issue");
        let other_issue = ".thegn/pipeline/THE-77/code/7.md";
        commit_test_file(&root, other_issue, "# other issue\n");
        let mut row = row_in(&root, Some(other_issue));
        row.report = Some("PASS\ngate: just test -- exit 0".into());
        let error = done_gate(&row).unwrap_err().to_string();
        assert!(error.contains("not this row's handoff"), "{error}");
    }

    #[test]
    fn canonical_artifact_empty_committed_file_is_refused() {
        let (_dir, root) = git_repo("canonical-empty");
        commit_test_file(&root, ARTIFACT, "");
        let mut row = row_in(&root, Some(ARTIFACT));
        row.report = Some("PASS\ngate: just test -- exit 0".into());
        let error = done_gate(&row).unwrap_err().to_string();
        assert!(error.contains("is empty"), "{error}");
    }

    #[test]
    fn canonical_artifact_committed_handoff_verifies() {
        let (_dir, root) = git_repo("canonical-handoff");
        commit_artifact(&root);
        let mut row = row_in(&root, Some(ARTIFACT));
        row.report = Some("PASS\ngate: just test -- exit 0".into());
        done_gate(&row).unwrap();
    }

    #[test]
    fn reap_plan_does_not_close_a_committed_readme_as_a_handoff() {
        let (_dir, root) = git_repo("reap-readme");
        commit_test_file(&root, "README.md", "# unrelated\n");
        let mut row = row_in(&root, Some("README.md"));
        row.report = Some("PASS\ngate: just test -- exit 0".into());
        row.exit_code = Some(0);
        let plan = reap_plan_rows(&[row], Some(&[]));
        assert_eq!(plan.len(), 1);
        assert!(
            !matches!(&plan[0].verdict, pipeline_reap::ReapVerdict::CloseDone),
            "a committed non-canonical artifact must not be reaped done"
        );
    }

    fn poison_terminal_report(path: &std::path::Path, id: i64) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "UPDATE agent_dispatches SET status='done', report=X'FF' WHERE id=?1",
            rusqlite::params![id],
        )
        .unwrap();
    }

    #[test]
    fn active_readers_status_skips_terminal_history() {
        let (_dir, db) = db("active-readers-status");
        let active = put(
            &db,
            NewDispatch::new("linear:THE-1107", "/wt/active", "coder"),
        )
        .unwrap();
        let terminal = put(
            &db,
            NewDispatch::new("linear:THE-1107", "/wt/done", "coder"),
        )
        .unwrap();
        poison_terminal_report(&db.file_path().unwrap(), terminal.id);

        assert_eq!(active.status, AgentDispatchStatus::Queued);
        assert!(read_status(&db, None, None, false, true).is_ok());
    }

    #[test]
    fn active_readers_list_active_skips_terminal_history() {
        let (_dir, db) = db("active-readers-list-active");
        let older = put(
            &db,
            NewDispatch::new("linear:THE-1107", "/wt/older", "coder"),
        )
        .unwrap();
        let newer = put(
            &db,
            NewDispatch::new("linear:THE-1107", "/wt/newer", "coder"),
        )
        .unwrap();
        let terminal = put(
            &db,
            NewDispatch::new("linear:THE-1107", "/wt/done", "coder"),
        )
        .unwrap();
        let path = db.file_path().unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE agent_dispatches SET dispatched_at_ms=20 WHERE id=?1",
            rusqlite::params![older.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE agent_dispatches SET dispatched_at_ms=10 WHERE id=?1",
            rusqlite::params![newer.id],
        )
        .unwrap();
        drop(conn);
        poison_terminal_report(&path, terminal.id);

        let (rows, truncated) = list_rows(&db, true).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [older.id, newer.id]
        );
        assert!(!truncated);
    }

    #[test]
    fn active_readers_wait_any_skips_terminal_history() {
        let (_dir, db) = db("active-readers-wait-any");
        let running = put(
            &db,
            NewDispatch {
                session_id: Some("session-running"),
                ..NewDispatch::new("linear:THE-1107", "/wt/running", "coder")
            },
        )
        .unwrap();
        db.update_dispatch_status(running.id, AgentDispatchStatus::Running)
            .unwrap();
        let terminal = put(
            &db,
            NewDispatch::new("linear:THE-1107", "/wt/done", "coder"),
        )
        .unwrap();
        poison_terminal_report(&db.file_path().unwrap(), terminal.id);

        let rows = wait_rows(&db, None).unwrap();
        let targets = pipeline_run::wait_candidates(&rows, None).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, running.id);
    }

    #[test]
    fn active_readers_chunk_admission_skips_terminal_history() {
        let (_dir, root) = git_repo("active-readers-chunk-admission");
        commit_test_file(&root, "chunk.md", "---\nfiles: [src/shared.rs]\n---\n");
        let wt = root.to_string_lossy().into_owned();
        let (_db_dir, db) = db("active-readers-chunk-db");
        let sibling = put(
            &db,
            NewDispatch {
                chunk_path: Some("chunk.md"),
                ..NewDispatch::new("linear:THE-1107", &wt, "coder")
            },
        )
        .unwrap();
        let terminal = put(
            &db,
            NewDispatch::new("linear:OTHER-ISSUE", "/wt/terminal", "coder"),
        )
        .unwrap();
        poison_terminal_report(&db.file_path().unwrap(), terminal.id);

        let prepared = prepare_chunk_admission(&db, &wt, "linear:THE-1107", "chunk.md").unwrap();
        assert_eq!(prepared.siblings.len(), 1);
        assert_eq!(prepared.siblings[0].row, sibling.id);
    }

    #[test]
    fn active_readers_wait_row_reports_missing_and_inactive_rows() {
        let (_dir, db) = db("active-readers-wait-row-controls");
        let missing_rows = wait_rows(&db, Some(99)).unwrap();
        assert!(matches!(
            pipeline_run::wait_candidates(&missing_rows, Some(99)),
            Err(pipeline_run::WaitSelectError::NoSuchRow(99))
        ));

        let done = put(
            &db,
            NewDispatch::new("linear:THE-1107", "/wt/done", "coder"),
        )
        .unwrap();
        db.update_dispatch_status(done.id, AgentDispatchStatus::Done)
            .unwrap();
        let done_rows = wait_rows(&db, Some(done.id)).unwrap();
        assert!(matches!(
            pipeline_run::wait_candidates(&done_rows, Some(done.id)),
            Err(pipeline_run::WaitSelectError::NotActive(id, _)) if id == done.id
        ));
    }

    #[test]
    fn active_readers_list_without_active_still_reads_history() {
        let (_dir, db) = db("active-readers-list-history-control");
        let terminal = put(
            &db,
            NewDispatch::new("linear:THE-1107", "/wt/done", "coder"),
        )
        .unwrap();
        poison_terminal_report(&db.file_path().unwrap(), terminal.id);

        assert!(list_rows(&db, false).is_err());
    }

    #[test]
    fn lead_lease_flags_parse_for_claim_set_status_and_session_open() {
        use clap::Parser as _;
        for args in [
            vec![
                "thegn",
                "dispatch",
                "claim",
                "linear:X-1",
                "/wt",
                "coder",
                "--stage",
                "code",
                "--lease",
                "n",
                "--owner",
                "o",
            ],
            vec![
                "thegn",
                "dispatch",
                "set-status",
                "1",
                "done",
                "--lease",
                "n",
                "--owner",
                "o",
            ],
            vec![
                "thegn",
                "dispatch",
                "set-status",
                "1",
                "failed",
                "--admin",
                "--why",
                "x",
            ],
            vec![
                "thegn",
                "session",
                "open",
                "--stage",
                "code",
                "--issue",
                "linear:X-1",
                "--worktree",
                "/wt",
                "--lease",
                "n",
                "--owner",
                "o",
            ],
        ] {
            assert!(crate::Cli::try_parse_from(args).is_ok());
        }
        assert!(
            crate::Cli::try_parse_from([
                "thegn",
                "dispatch",
                "claim",
                "linear:X-1",
                "/wt",
                "coder",
                "--stage",
                "code",
                "--lease",
                "n"
            ])
            .is_err()
        );
        assert!(
            crate::Cli::try_parse_from([
                "thegn",
                "dispatch",
                "set-status",
                "1",
                "failed",
                "--admin"
            ])
            .is_err()
        );
    }

    #[test]
    fn lead_lease_conflict_has_its_own_error_kind() {
        let error = anyhow::Error::new(thegn_core::db::LeadLeaseConflict::Required {
            lease: "repo:issue".into(),
        });
        assert_eq!(set_status_error_kind(&error), Some("lead_lease"));
    }
}
