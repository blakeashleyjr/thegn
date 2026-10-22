//! The **worktree-aux** seam: assorted per-worktree local state — vim
//! registers, share/forward port bindings, the local merge queue, disk-usage
//! cache, worktree↔issue links, undo marks, and the sandbox audit trail.

use crate::db::{ForwardRow, MergeQueueRow, PrQueueRow, ShareRow};
use crate::models::ContainerEvent;
use anyhow::Result;

pub const MERGE_CLAIM_MAX_LEASE_MS: i64 = 5 * 60 * 1000;
pub const MERGE_CLAIM_OWNER_HINT_MAX: usize = 64;
pub const MERGE_CLAIM_LOST: &str = "merge claim lost";
pub const MERGE_CLAIM_ACTIVE: &str = "merge row is claimed by an active drain";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeRetryOutcome {
    Rearmed,
    NotQueued,
    NotRetryable { status: String },
    Claimed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeClaimPhase {
    ClaimedNoChild,
    Settled,
}

impl MergeClaimPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaimedNoChild => "claimed_no_child",
            Self::Settled => "settled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claimed_no_child" => Some(Self::ClaimedNoChild),
            "settled" => Some(Self::Settled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MergeClaimRequest {
    pub worktree: String,
    pub branch: String,
    pub repo_root: String,
    pub target_branch: String,
    pub location: String,
    pub expected_status: String,
    pub lease_until_ms: i64,
    pub owner_hint: Option<String>,
}

pub struct MergeClaim {
    pub worktree: String,
    pub branch: String,
    pub repo_root: String,
    pub target_branch: String,
    pub location: String,
    pub generation: i64,
    pub phase: MergeClaimPhase,
    pub lease_until_ms: i64,
    pub owner_hint: Option<String>,
    pub(crate) owner_token: String,
}

impl std::fmt::Debug for MergeClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeClaim")
            .field("worktree", &self.worktree)
            .field("branch", &self.branch)
            .field("repo_root", &self.repo_root)
            .field("target_branch", &self.target_branch)
            .field("location", &self.location)
            .field("generation", &self.generation)
            .field("phase", &self.phase)
            .field("lease_until_ms", &self.lease_until_ms)
            .field("owner_hint", &self.owner_hint)
            .field("owner_token", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeClaimView {
    pub worktree: String,
    pub generation: i64,
    pub phase: String,
    pub lease_until_ms: Option<i64>,
    pub owner_hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeAttemptResult {
    Consumed,
    BudgetExhausted,
    ClaimLost,
}

/// Exact replacement of a queue transition's nullable metadata. `None` clears
/// the column; these values confer no ownership or concurrency authority.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MergeStatusFields {
    pub result_oid: Option<String>,
    pub conflict_paths: Option<String>,
    pub error_detail: Option<String>,
}

/// Settled fold outcomes only. This vocabulary is not evidence that Git advanced;
/// the caller must establish advancement before selecting `Landed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeFinalStatus {
    Landed,
    Deferred,
    GateFailed,
    GateError,
}

impl MergeFinalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Landed => "landed",
            Self::Deferred => "deferred",
            Self::GateFailed => "gate_failed",
            Self::GateError => "gate_error",
        }
    }
}

/// Complete replacement of the outcome columns. `None` means SQL NULL, unlike
/// `update_merge_status`. Finalization preserves an existing row's nomination
/// time and attempt budget; it is not an enqueue/retry gesture.
pub struct MergeFinalOutcome<'a> {
    pub worktree: &'a str,
    pub branch: &'a str,
    pub repo_root: &'a str,
    /// The target actually requested for this fold, including explicit overrides.
    pub target_branch: &'a str,
    /// Expected execution location. Empty and `local` denote local placement;
    /// remote descriptors must match exactly, never by filesystem path alone.
    pub location: &'a str,
    pub status: MergeFinalStatus,
    pub result_oid: Option<&'a str>,
    pub conflict_paths: Option<&'a str>,
    pub error_detail: Option<&'a str>,
}

/// Branch identity captured by a deliberate local merge-queue enqueue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeCandidate {
    pub branch_ref: String,
    pub candidate_oid: String,
}

/// Registry facts relevant to outcome routing. NULL is retained, not fabricated
/// into a repository/branch identity. These strings are cache metadata, not Git
/// or filesystem identity proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRegistryIdentity {
    pub branch: Option<String>,
    pub repo_path: Option<String>,
    pub location: Option<String>,
}

/// One consistent pre-fold snapshot. Fields cannot be constructed by consumers;
/// obtain it through `observe_merge_outcome`, before external work starts.
/// Equality detects current-state changes, NOT same-value ABA or durable leases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcomeObservation {
    pub(crate) worktree: String,
    pub(crate) registry: Option<MergeRegistryIdentity>,
    pub(crate) queue: Option<MergeQueueRow>,
    // MergeQueueRow presents SQL NULL as empty. Retain the raw value too so a
    // writer changing that representation is still visible to the comparison.
    pub(crate) queue_location: Option<String>,
}

impl MergeOutcomeObservation {
    /// Borrow the exact captured routing facts without fabricating a fresh guard.
    /// This is metadata, not a lease or authority to access a remote filesystem.
    pub fn registry_identity(&self) -> Option<&MergeRegistryIdentity> {
        self.registry.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcomeWrite {
    Written,
    RegistryChanged,
    QueueChanged,
    /// The row is claimed by an active drain; nothing was written.
    Claimed,
}

/// Object-safe (`&self` + concrete args), so `&dyn WorktreeAuxStore` works for
/// backend-agnostic consumers. [`crate::db::Db`] is the embedded-SQLite impl.
pub trait WorktreeAuxStore {
    /// Persist a register's value (upsert). The single-char `name` is the
    /// register id; the volatile `+` clipboard register is never stored here.
    fn put_register(&self, name: char, value: &str) -> Result<()>;

    /// Load every persisted register as `(name, value)` pairs.
    fn all_registers(&self) -> Result<Vec<(char, String)>>;

    /// Insert or update the share record for `(worktree, local_port)`.
    fn upsert_share(
        &self,
        worktree: &str,
        local_port: u16,
        generation: u64,
        provider: &str,
        public_url: Option<&str>,
        state: &str,
    ) -> Result<()>;

    /// All persisted shares, newest first (restore + panel listing).
    fn list_shares(&self) -> Result<Vec<ShareRow>>;

    /// Remove a share only when this run owns it (or it is a legacy generation-0 row).
    fn delete_share_generation(
        &self,
        worktree: &str,
        local_port: u16,
        generation: u64,
    ) -> Result<bool>;

    /// Change a legacy/resurrected share's generation only if `from` still owns it.
    fn rebind_share_generation(
        &self,
        worktree: &str,
        local_port: u16,
        from: u64,
        to: u64,
    ) -> Result<bool>;

    /// Unconditionally remove a share record. Reserved for explicit CLI `share stop`.
    fn delete_share(&self, worktree: &str, local_port: u16) -> Result<()>;

    /// Insert or update the forward record for `(worktree, container_port)`.
    fn upsert_forward(
        &self,
        worktree: &str,
        container_port: u16,
        host_port: u16,
        url: &str,
    ) -> Result<()>;

    /// All persisted forwards, newest first (restore + panel listing).
    fn list_forwards(&self) -> Result<Vec<ForwardRow>>;

    /// Remove the forward record for `(worktree, container_port)`.
    fn delete_forward(&self, worktree: &str, container_port: u16) -> Result<()>;

    /// Enqueue (or re-enqueue) an unbound worktree branch for the next fold. Re-enqueueing
    /// resets the row to `queued` and clears any prior result/conflict/error. The `location`
    /// value mirrors `worktrees.location` through a correlated subquery; NULL when unregistered
    /// is treated as local. This clears any previously captured candidate pair.
    fn enqueue_merge(&self, worktree: &str, branch: &str, target_branch: &str) -> Result<()>;

    /// Enqueue a deliberate local branch and capture its full ref and tip OID.
    fn enqueue_merge_candidate(
        &self,
        worktree: &str,
        branch: &str,
        target_branch: &str,
        candidate: &MergeCandidate,
    ) -> Result<()>;

    /// Read a captured candidate. `None` means the row is unbound (legacy,
    /// remote, or enqueued through `enqueue_merge`); incomplete pairs are also
    /// treated as unbound.
    fn merge_candidate(&self, worktree: &str) -> Result<Option<MergeCandidate>>;

    /// Adopt a verified fast-forward repair for an already held merge candidate.
    /// The expected pair makes this a compare-and-swap against concurrent edits.
    fn adopt_merge_candidate(
        &self,
        worktree: &str,
        expected: &MergeCandidate,
        candidate_oid: &str,
    ) -> Result<bool>;

    /// Read registry and queue state consistently BEFORE starting a fold. No
    /// reservation is created and no lock may be retained across external work.
    fn observe_merge_outcome(&self, worktree: &str) -> Result<MergeOutcomeObservation>;

    /// Commit a final status directly, only while the pre-fold observation still
    /// matches. Refusal/error changes nothing. A caller may apply lifecycle only
    /// after `Written`; that subsequent filesystem work is not part of this
    /// transaction. Implementations must never publish an intermediate `queued`.
    fn persist_merge_outcome(
        &self,
        observed: &MergeOutcomeObservation,
        outcome: &MergeFinalOutcome<'_>,
    ) -> Result<MergeOutcomeWrite>;

    /// Update a queued worktree's status and (optionally) its result oid,
    /// conflicted paths (newline-joined), and error detail. Passing `None` leaves
    /// the corresponding column unchanged.
    fn update_merge_status(
        &self,
        worktree: &str,
        status: &str,
        result_oid: Option<&str>,
        conflict_paths: Option<&str>,
        error_detail: Option<&str>,
    ) -> Result<()>;

    /// Replace all transition metadata on exactly one existing row. Missing
    /// rows are errors. Preserve routing, nomination time and attempt budget.
    /// This is not a claim/CAS: concurrent reassignment needs a separate guard.
    fn replace_merge_status(
        &self,
        worktree: &str,
        status: &str,
        fields: &MergeStatusFields,
    ) -> Result<()>;

    /// Fill a missing landed identity only when the complete observed queue
    /// identity is still current. `true` means the row was updated; `false`
    /// means another writer changed or revoked it.
    fn backfill_landed_result_oid(
        &self,
        expected: &MergeQueueRow,
        result_oid: &str,
    ) -> Result<bool>;

    /// Re-stamp a queued row's target branch to the one a run is actually
    /// folding into.
    ///
    /// `enqueue_merge` freezes `target_branch` at enqueue time, but the
    /// effective target is resolved per run (`--set
    /// merge_queue.target_branch=…`, or `"auto"` following the repo's default
    /// branch as it moves). Without this the queue keeps reporting the stale
    /// value while the drain folds somewhere else — two different targets shown
    /// for one operation.
    fn set_merge_target(&self, worktree: &str, target_branch: &str) -> Result<()>;

    /// Record how many agent-dispatch cycles have been spent on a row, so the
    /// `agent_max_attempts` budget belongs to the branch rather than to one
    /// invocation of the drain.
    fn set_merge_agent_attempts(&self, worktree: &str, attempts: u32) -> Result<()>;

    /// Re-arm a blocked row for another drain: back to `queued`, attempts reset,
    /// prior failure detail cleared. The "I fixed it, try again" gesture.
    /// Returns whether a row was actually reset. Production retry uses
    /// [`Self::retry_held_merge_entry`] to refuse non-held states.
    fn retry_merge_entry(&self, worktree: &str) -> Result<bool>;

    /// Re-arm only a held merge row: deferred, gate_failed, gate_error,
    /// agent_blocked, or needs_human. Other row states and active claims are
    /// returned as typed refusals.
    fn retry_held_merge_entry(&self, worktree: &str) -> Result<MergeRetryOutcome>;

    /// Drop a worktree's merge-queue row (e.g. after a clean land is recorded
    /// elsewhere, or the worktree is removed).
    fn remove_merge_entry(&self, worktree: &str) -> Result<()>;

    /// Install only the fixed branch-retained cleanup marker if every decoded
    /// field still matches this landed observation. Preserve timestamps and
    /// raw nullable location/attempt aliases; false means revoked/already held.
    fn hold_merge_cleanup(&self, expected: &MergeQueueRow) -> Result<bool>;

    /// The whole queue, oldest-queued first (the fold order + UI feed).
    fn list_merge_queue(&self) -> Result<Vec<MergeQueueRow>>;

    /// Atomically acquire one eligible queue row and its fencing generation.
    fn admit_merge_claim(&self, request: &MergeClaimRequest) -> Result<Option<MergeClaim>>;

    /// Atomically acquire an eligible local queue row only when the worktree
    /// registry has no row for it. The caller must first prove that the path
    /// belongs to `repo_root` through Git (for example, `git worktree list`).
    /// This is THE-220 slice b; it does not replace the registered admission.
    fn admit_unregistered_local_merge_claim(
        &self,
        request: &MergeClaimRequest,
    ) -> Result<Option<MergeClaim>>;

    /// Observe durable claim state without exposing its owner token.
    fn observe_merge_claim(&self, worktree: &str) -> Result<Option<MergeClaimView>>;

    /// Extend a live claim's lease while retaining its generation.
    fn renew_merge_claim(&self, claim: &MergeClaim, lease_until_ms: i64) -> Result<MergeClaim>;

    /// Mark a no-child claim settled before releasing it.
    fn settle_merge_claim_no_child(&self, claim: &MergeClaim) -> Result<MergeClaim>;

    /// Release a settled claim exactly once; the generation is retained.
    fn release_merge_claim(&self, claim: &MergeClaim) -> Result<()>;

    /// Atomically consume one agent attempt owned by a live claim.
    fn consume_merge_agent_attempt(
        &self,
        claim: &MergeClaim,
        max: u32,
    ) -> Result<MergeAttemptResult>;

    /// Give back one attempt consumed by this live claim when the agent never started.
    /// Returns false when the claim is no longer live (nothing changes).
    fn refund_merge_agent_attempt(&self, claim: &MergeClaim) -> Result<bool>;

    /// Replace status fields while this claim owns the row with a live lease, or is settled.
    fn replace_merge_status_claimed(
        &self,
        claim: &MergeClaim,
        status: &str,
        fields: &MergeStatusFields,
    ) -> Result<MergeClaim>;

    // --- PR queue (v50) ---------------------------------------------------
    //
    // Keyed by repo + provider/project identity + PR number rather than worktree:
    // a queued pull request need not have a local checkout.

    /// Queue (or re-queue) a pull request. Re-queueing resets it to `watching`
    /// and clears the prior blocker/detail/attempts, so "I fixed it, watch again"
    /// starts clean — the same gesture as `retry_merge_entry`. This legacy
    /// number-only path has no executable identity and is held for verification.
    fn enqueue_pr(
        &self,
        repo_root: &str,
        number: u64,
        worktree: Option<&str>,
        branch: &str,
        base_branch: &str,
        forge: &str,
    ) -> Result<()>;

    /// Queue a PR with the canonical URL fetched from the forge. Changed checkout
    /// bindings require the explicit `rebind_pr` operation.
    #[allow(clippy::too_many_arguments)] // mirrors enqueue_pr plus the bound URL
    fn enqueue_bound_pr(
        &self,
        repo_root: &str,
        number: u64,
        worktree: Option<&str>,
        branch: &str,
        base_branch: &str,
        forge: &str,
        pr_url: &str,
    ) -> Result<()>;

    /// Update a queued PR's status and, optionally, its blocker word, detail,
    /// and last observed head. Passing `None` leaves that column unchanged, so a
    /// failed refresh can update a note without clobbering a known-good head.
    /// A row whose PR agent holds a live lease is not written: the lease holder
    /// writes only through its own fenced release, and the driver skips leased
    /// rows before reaching this.
    fn update_pr_status(
        &self,
        key: &str,
        status: &str,
        blocker: Option<&str>,
        detail: Option<&str>,
        last_head_oid: Option<&str>,
    ) -> Result<()>;

    /// Record agent-dispatch cycles spent on a PR, so the budget belongs to the
    /// pull request rather than to one drain. A live PR-agent lease fences this
    /// write; the lease holder updates attempts only through its fenced release,
    /// while the driver skips leased rows before reaching this.
    fn set_pr_agent_attempts(&self, key: &str, attempts: u32) -> Result<()>;

    /// Return lease metadata only when the row has an owner and a lease deadline.
    /// The owner token is never exposed; a missing row returns `Ok(None)`.
    fn pr_agent_lease_view(&self, key: &str) -> Result<Option<crate::pr_queue::PrAgentLeaseView>>;

    fn admit_pr_agent(
        &self,
        request: &crate::pr_queue::PrAgentAdmit<'_>,
    ) -> Result<crate::pr_queue::PrAgentAdmission>;
    fn renew_pr_agent(
        &self,
        lease: &crate::pr_queue::PrAgentLease,
        lease_until_ms: i64,
    ) -> Result<crate::pr_queue::PrAgentLease>;
    fn release_pr_agent(
        &self,
        lease: &crate::pr_queue::PrAgentLease,
        finished: Option<(&str, &str)>,
    ) -> Result<bool>;
    /// Record the head an admitted agent produced, fenced on the live lease
    /// generation and on the admitted head still being the row's last head.
    /// Returns false when the lease is no longer this one or the row moved.
    fn record_pr_agent_head(
        &self,
        lease: &crate::pr_queue::PrAgentLease,
        admitted_head: &str,
        own_head: &str,
    ) -> Result<bool>;

    /// Atomically reserve the one automatic CI rerun for `head`. The
    /// `expected_head` fence prevents a stale driver from reverting a newer
    /// observed head. Returns true only for the worker that reserved it.
    fn reserve_pr_ci_rerun(
        &self,
        key: &str,
        expected_head: Option<&str>,
        head: &str,
    ) -> Result<bool>;

    /// Drop one queued pull request.
    fn remove_pr_entry(&self, key: &str) -> Result<()>;

    /// Drop every queued pull request for a repo. Returns how many went.
    fn clear_pr_queue(&self, repo_root: &str) -> Result<usize>;

    /// Every queued pull request, oldest-queued first.
    fn list_pr_queue(&self) -> Result<Vec<PrQueueRow>>;

    /// Explicitly change a queued PR's authority or checkout binding.
    fn rebind_pr(&self, old_key: &str, binding: &crate::db::PrRebind) -> Result<String>;

    /// `(size_bytes, target_bytes, fetched_at)` for one worktree, or `None`.
    fn get_worktree_disk(&self, worktree: &str) -> Result<Option<(i64, i64, i64)>>;

    fn put_worktree_disk(&self, worktree: &str, size_bytes: i64, target_bytes: i64) -> Result<()>;

    /// All cached disk sizes keyed by worktree path → `(size_bytes, target_bytes)`.
    /// One bulk read for the sidebar/statusbar; never scans.
    fn all_worktree_disk(&self) -> Result<std::collections::HashMap<String, (i64, i64)>>;

    /// Every size-cache key with its fetch timestamp. Feeds the background
    /// scanner's TTL/priority planning and its orphan sweep in one read, instead
    /// of a `get_worktree_disk` per registry row.
    fn all_worktree_disk_stamps(&self) -> Result<std::collections::HashMap<String, i64>>;

    /// Drop a worktree's cached size (e.g. right after a `clean`) so the badge
    /// clears without waiting for the next scan.
    fn delete_worktree_disk(&self, worktree: &str) -> Result<()>;

    /// Associate `issue_id` (in `"<provider>:<key>"` form) with a worktree path.
    fn link_issue(&self, worktree_path: &str, issue_id: &str) -> Result<()>;

    /// Remove a worktree↔issue association.
    fn unlink_issue(&self, worktree_path: &str, issue_id: &str) -> Result<()>;

    /// All issue ids linked to a worktree, newest first.
    fn linked_issues(&self, worktree_path: &str) -> Result<Vec<String>>;

    /// Every issue/worktree association, newest links first.
    fn issue_link_worktrees(&self) -> Result<Vec<(String, String)>>;

    /// Record a reset target we are about to create, pruning each worktree's
    /// mark set to the freshest 100 (the undo planner only reads ~100 reflog
    /// entries anyway).
    fn add_undo_mark(&self, worktree: &str, sha: &str) -> Result<()>;

    /// All recorded undo-reset targets for a worktree (newest first).
    fn undo_marks(&self, worktree: &str) -> Result<Vec<String>>;

    /// Record a sandbox event (exec, network, dns, orphan_gc) in the audit log.
    fn insert_container_event(
        &self,
        worktree: &str,
        ts: i64,
        kind: &str,
        detail: Option<&str>,
        exit_code: Option<i64>,
    ) -> Result<()>;

    /// Retrieve the most recent `limit` container events for a worktree,
    /// newest first.
    fn container_events(&self, worktree: &str, limit: usize) -> Result<Vec<ContainerEvent>>;

    /// Delete container events older than `older_than_secs` seconds. Called on
    /// startup to keep the audit table from growing unbounded.
    fn prune_container_events(&self, older_than_secs: i64) -> Result<usize>;
}
