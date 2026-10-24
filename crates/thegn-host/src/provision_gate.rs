//! Provision arbitration: the process-global coordination between the flows
//! that can provision a sandbox concurrently (the eager provisioner, the
//! focused materialize, the warm-pool spare builder).
//!
//! Two primitives, both RAII guards over a name registry:
//! - [`sandbox_lock`] — EXCLUSIVE, keyed by the resolved sandbox name.
//!   `provision_provider_env_named` takes it around the whole pipeline so two
//!   concurrent provisions of the SAME sandbox serialize; the loser wakes into
//!   the provision-marker short-circuit and no-ops. The marker alone only
//!   guards *sequential* re-runs (it is written at the END of the pipeline) —
//!   dropping this lock (a7211cc) let an eager and a materialize provision
//!   interleave on one sprite, so the materialize leg raced through the
//!   eager leg's completed steps, returned early, and attached a pane to a
//!   half-provisioned sprite (the premature raw `sh` at `~`, with the splash
//!   returning at the eager leg's next stage).
//! - [`worktree_live_guard`] / [`worktree_live`] — a SHARED "a provision for
//!   this worktree is in flight" flag, held by `provision_worktree`. The
//!   materialize warm-claim fast path consults it: claiming a spare (and
//!   clearing the splash) while the eager provisioner is mid-provision of the
//!   worktree's derived sprite would flip the binding under a live stream and
//!   orphan the derived sandbox.
//!
//! Blocking by design: everything here runs inside `spawn_blocking` tasks
//! (provisioning is minutes of subprocess/network work) — never on the loop.
//!
//! Also home to the warm-pool naming helpers ([`mint_spare_name`],
//! [`flake_lock_hash`]) and the materialize warm-claim step
//! ([`try_claim_spare`]), extracted from the pinned `agent.rs`/`run.rs`.

use std::collections::HashMap;
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};

use sha2::{Digest, Sha256};
use thegn_core::store::{PoolStore, WorkspaceStore};
/// A name registry with a hold-count per name. Exclusive acquires wait for the
/// count to reach zero; shared acquires just increment. One registry per use
/// (locks vs live-flags) so an exclusive sandbox lock never contends with the
/// shared worktree flag.
struct Registry {
    held: Mutex<HashMap<String, usize>>,
    freed: Condvar,
}

impl Registry {
    fn new() -> Self {
        Self {
            held: Mutex::new(HashMap::new()),
            freed: Condvar::new(),
        }
    }

    /// Lock the map, shrugging off poisoning: a panicked provision thread must
    /// not wedge every future provision in the process.
    fn map(&self) -> MutexGuard<'_, HashMap<String, usize>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn acquire(&'static self, name: &str, exclusive: bool) -> Guard {
        let mut held = self.map();
        if exclusive {
            while held.contains_key(name) {
                held = self
                    .freed
                    .wait(held)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        *held.entry(name.to_string()).or_insert(0) += 1;
        Guard {
            reg: self,
            name: name.to_string(),
        }
    }

    fn count(&self, name: &str) -> usize {
        self.map().get(name).copied().unwrap_or(0)
    }
}

/// RAII hold on a registry name; releasing wakes exclusive waiters.
pub(crate) struct Guard {
    reg: &'static Registry,
    name: String,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut held = self.reg.map();
        if let Some(n) = held.get_mut(&self.name) {
            *n -= 1;
            if *n == 0 {
                held.remove(&self.name);
            }
        }
        drop(held);
        self.reg.freed.notify_all();
    }
}

/// Exclusive per-sandbox-name provision locks.
static LOCKS: LazyLock<Registry> = LazyLock::new(Registry::new);
/// Shared per-worktree "provision in flight" flags.
static LIVE: LazyLock<Registry> = LazyLock::new(Registry::new);
/// Exclusive per-HOST provision locks (leader election for the host state
/// machine, `host_flow::ensure_ready`). A sibling registry so a host lock
/// never contends with sandbox locks — and it is always released BEFORE any
/// sandbox lock is taken (coarser gate first; no nesting).
static HOSTS: LazyLock<Registry> = LazyLock::new(Registry::new);

/// Serialize host provisioning for one host id (belt-and-braces under the
/// host_flow flight registry — a racing same-process caller that bypassed the
/// flight map still serializes here).
pub(crate) fn host_lock(host_id: &str) -> Guard {
    HOSTS.acquire(host_id, true)
}

/// Serialize provisioning of `name` (the RESOLVED sandbox name): blocks while
/// another thread holds the lock for the same name. Take it around the whole
/// provision pipeline; the marker short-circuit makes the woken loser a no-op.
pub(crate) fn sandbox_lock(name: &str) -> Guard {
    LOCKS.acquire(name, true)
}

/// Mark a provision as in flight for `worktree` (shared — the eager and the
/// materialize legs may both hold it; they serialize on [`sandbox_lock`]).
pub(crate) fn worktree_live_guard(worktree: &str) -> Guard {
    LIVE.acquire(worktree, false)
}

/// Whether any provision for `worktree` is currently in flight.
pub(crate) fn worktree_live(worktree: &str) -> bool {
    LIVE.count(worktree) > 0
}

/// A generic name for a new warm-pool spare with a collision-resistant random suffix.
pub(crate) fn mint_spare_name(repo: &str) -> anyhow::Result<String> {
    let slug = std::path::Path::new(repo)
        .file_name()
        .and_then(|s| s.to_str())
        .map(thegn_core::util::slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tg".to_string());
    let mut random = [0_u8; 6];
    getrandom::fill(&mut random)
        .map_err(|_| anyhow::anyhow!("warm pool: cannot mint a spare name: no OS randomness"))?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("{slug}-pool-{suffix}"))
}

/// Whether `env_name` may host (or hand over) warm-pool spares at all: only a
/// CONFIGURED provider env qualifies. The implicit `"default"` env — or any name
/// without an `[env.<name>]` table — has no provider to create/hand-over
/// sandboxes with, so "provisioning" a spare for it succeeds instantly as a
/// no-op, minting a phantom `ready` spare; claiming that phantom then skips the
/// worktree's real provisioning and the pane lands in a bare sandbox (the
/// bare-sprite-shell incident of 2026-07-02).
pub(crate) fn poolable_env(cfg: &thegn_core::config::Config, env_name: &str) -> bool {
    cfg.env
        .get(env_name)
        .is_some_and(|e| matches!(e.placement, thegn_core::config::PlacementMode::Provider))
}

/// Staleness key for a spare's seeded devShell: `""` when `flake.lock` is absent,
/// a SHA-256 hash when it is readable, and a per-call `unreadable:` value when
/// another read error occurs. Unreadable values are never persisted as match keys,
/// so repeated calls compare unequal and lifecycle checks treat the spare as stale.
/// The old FNV key makes existing spares and base checkpoints stale once, so they rotate.
pub(crate) fn flake_lock_hash(repo_root: &std::path::Path) -> String {
    let lock_path = repo_root.join("flake.lock");
    match std::fs::read(&lock_path) {
        Ok(bytes) => {
            let digest = Sha256::digest(bytes);
            digest.iter().map(|byte| format!("{byte:02x}")).collect()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            tracing::warn!(
                target: "thegn::lifecycle",
                path = %lock_path.display(),
                error = %error,
                "flake.lock unreadable; treating every warm-pool spare and base checkpoint as stale"
            );
            let mut random = [0_u8; 6];
            match getrandom::fill(&mut random) {
                Ok(()) => {
                    let suffix = random
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>();
                    format!("unreadable:{suffix}")
                }
                Err(_) => {
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_nanos())
                        .unwrap_or(0);
                    format!("unreadable:{nanos}")
                }
            }
        }
    }
}

/// The materialize warm-claim fast path: bind a ready pool spare to `worktree`
/// (repo/env resolved from the worktree, branch settled in the spare's clone).
/// `false` when the worktree isn't remote, no spare is ready, or — the
/// arbitration — a provision for this worktree is already IN FLIGHT
/// ([`worktree_live`]): claiming then would clear a live splash, flip the
/// sandbox binding under the running provision, and orphan the derived sprite;
/// the caller falls through to `provision_worktree`, which serializes on the
/// sandbox lock and short-circuits once the live run finishes. Off-loop only
/// (DB + git subprocess + provider network).
pub(crate) fn try_claim_spare(cfg: &thegn_core::config::Config, worktree: &str) -> bool {
    let loc = thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(worktree));
    if !loc.is_remote() || worktree_live(worktree) {
        return false;
    }
    let db = thegn_core::db::Db::open().ok(); // best-effort: cache: the git fallback below decides liveness
    let repo_root = db
        .as_ref()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| thegn_core::repo::main_worktree(std::path::Path::new(worktree)))
        .unwrap_or_else(|| std::path::PathBuf::from(worktree));
    // The worktree's EFFECTIVE env (its DB selection), exactly as the exec/
    // provision paths resolve it — claiming under the ambient default while the
    // worktree runs a picked env hands over a spare from the wrong pool.
    let read = match db.as_ref() {
        Some(db) => db.effective_env_checked(worktree, &repo_root.to_string_lossy()),
        None => Err(anyhow::anyhow!("the state database could not be opened")),
    };
    let Some(selected) = claim_selection(worktree, read) else {
        return false;
    };
    let env_name = cfg
        .resolve_env(
            &repo_root,
            &loc,
            std::path::Path::new(worktree),
            selected.as_deref(),
        )
        .name;
    // Never claim for an env that can't hand a spare over (no configured
    // provider) — the bind would "succeed" while skipping branch settle, parity,
    // and the worktree's real provisioning.
    if !poolable_env(cfg, &env_name) {
        return false;
    }
    // Already bound to a spare ⇒ nothing to claim; a re-materialize (restart,
    // tab reopen) must keep using it, not stack a second bind. An unreadable
    // binding never claims: a bound worktree would stack a second spare.
    if !matches!(
        db.as_ref().map(|db| db.worktree_provider_sandbox(worktree)),
        Some(Ok(None))
    ) {
        return false;
    }
    // Claim ONLY when the worktree actually needs provisioning (its sandbox is
    // missing or bare): a provisioned worktree that re-materializes must not be
    // rebound to a fresh spare — that would orphan its live, stateful sandbox.
    if !crate::agent::provision_pending(cfg, worktree) {
        return false;
    }
    // off-loop: inside spawn_blocking
    #[expect(clippy::disallowed_methods)]
    let branch = thegn_core::util::git_cmd(std::path::Path::new(worktree))
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|b| !b.is_empty() && b != "HEAD");
    claim_spare(cfg, worktree, &repo_root, &env_name, branch.as_deref()).is_some()
}

/// The provisioning-gate halt for an attach: `Some` when `worktree` resolves to a
/// provider env whose sandbox has NOT finished provisioning (no LOCAL marker — set
/// at provision completion in `provision_provider_env_named`). Refusing the raw
/// shell here keeps a bare shell off an unprovisioned VM on the direct-spawn paths
/// (resurrect, pane splits, respawn-on-exit) that call `launch_spec` without a
/// provision check — the materialize path passes, since it provisions first. The
/// caller gates this on `outcome.location.is_some()` (a live provider placement).
/// `None` for the clean-shell escape hatch or an already-provisioned sandbox. A
/// cheap local file stat + config resolve — no network, loop-safe.
/// `Err(SandboxHalt)` when `has_provider_location` (a live provider placement) and
/// the sandbox has NOT finished provisioning (no LOCAL marker — set at provision
/// completion). `Ok(())` otherwise. Cheap (local stat + config resolve, no
/// network — loop-safe).
pub(crate) fn guard_unprovisioned_attach(
    cfg: &thegn_core::config::Config,
    worktree: &str,
    choice: &str,
    has_provider_location: bool,
) -> anyhow::Result<()> {
    if !has_provider_location || choice == "clean-shell" {
        return Ok(());
    }
    let loc = thegn_core::remote::GitLoc::for_worktree_op(
        std::path::Path::new(worktree),
        "provider attach",
    )
    .map_err(anyhow::Error::msg)?;
    let repo_root = thegn_core::db::Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| thegn_core::repo::main_worktree(std::path::Path::new(worktree)))
        .unwrap_or_else(|| std::path::PathBuf::from(worktree));
    let selected_env = attach_selection(
        thegn_core::db::Db::open()
            .and_then(|db| db.effective_env_checked(worktree, &repo_root.to_string_lossy())),
    )
    .map_err(anyhow::Error::from)?;
    let env_name = cfg
        .resolve_env(
            &repo_root,
            &loc,
            std::path::Path::new(worktree),
            selected_env.as_deref(),
        )
        .name;
    let Some(id) = crate::agent::provider_sandbox_name(cfg, worktree, &env_name) else {
        return Ok(());
    };
    if crate::provider_workdir::is_provisioned_locally(&id) {
        return Ok(());
    }
    Err(crate::agent::SandboxHalt {
        env_name,
        placement: "provider".into(),
        reason: "sandbox is still provisioning — retry once setup finishes".into(),
        ask: false,
        dormant: None,
    }
    .into())
}

fn claim_selection(worktree: &str, read: anyhow::Result<Option<String>>) -> Option<Option<String>> {
    match read {
        Ok(selected) => Some(selected),
        Err(error) => {
            tracing::warn!(target: "thegn::lifecycle", worktree = %worktree, error = %format!("{error:#}"), "warm-pool claim skipped: environment selection could not be read");
            None
        }
    }
}

#[expect(
    clippy::result_large_err,
    reason = "SandboxHalt is returned unboxed by every attach/launch gate; boxing here alone would split the convention"
)]
fn attach_selection(
    read: anyhow::Result<Option<String>>,
) -> Result<Option<String>, crate::agent::SandboxHalt> {
    match read {
        Ok(selected) => Ok(selected),
        Err(error) => Err(crate::agent::SandboxHalt {
            env_name: "?".into(),
            placement: "provider".into(),
            reason: format!(
                "the environment selection could not be read ({error:#}); retry once the state database is readable"
            ),
            ask: false,
            dormant: None,
        }),
    }
}

// ---------------------------------------------------------------------------
// Warm-pool spare lifecycle (mint / claim / destroy) — the flows the guards
// above arbitrate. Extracted from the pinned `agent.rs`.

/// A reserved provisioning row and its spare lease. Dropping it releases the
/// lease and leaves the row for the stale sweep.
pub(crate) struct ReservedSpare {
    pub(crate) name: String,
    lease: crate::pool_lease::Lease,
}

/// Reserve a name, hold its lease, and record the provisioning row.
pub(crate) fn reserve_spare(
    repo_root: &std::path::Path,
    env_name: &str,
) -> anyhow::Result<ReservedSpare> {
    let repo = repo_root.to_string_lossy().to_string();
    let name = mint_spare_name(&repo)?;
    let lease = crate::pool_lease::spare_lease(&name)
        .map_err(|error| anyhow::anyhow!("warm pool: cannot lease new spare {name}: {error}"))?;
    {
        let db = thegn_core::db::Db::open().map_err(|error| {
            anyhow::anyhow!(
                "warm pool: cannot open ledger before provisioning spare {name}: {error:#}"
            )
        })?;
        db.insert_pool_spare(&name, &repo, env_name)
            .map_err(|error| {
                anyhow::anyhow!(
                    "warm pool: cannot record provisioning intent for spare {name}: {error:#}"
                )
            })?;
    }
    Ok(ReservedSpare { name, lease })
}

/// Mint + fully provision a NEW warm-pool spare for `(repo, env)`: a generically-
/// named sandbox, cloned + devShell-seeded + tooled + checkpointed (so it suspends
/// for free), recorded `ready` so a worktree can claim it. Returns the spare name.
/// On failure the half-built sandbox + DB row are torn down.
#[cfg(test)]
pub fn provision_spare(
    cfg: &thegn_core::config::Config,
    repo_root: &std::path::Path,
    env_name: &str,
    progress: impl FnMut(&[crate::agent::ProvisionStepView]),
) -> anyhow::Result<String> {
    let spare = reserve_spare(repo_root, env_name)?;
    provision_reserved(cfg, repo_root, env_name, spare, progress)
}

/// Provision a reserved spare and hold its lease through publish or rollback.
pub(crate) fn provision_reserved(
    cfg: &thegn_core::config::Config,
    repo_root: &std::path::Path,
    env_name: &str,
    spare: ReservedSpare,
    mut progress: impl FnMut(&[crate::agent::ProvisionStepView]),
) -> anyhow::Result<String> {
    let name = spare.name;
    let _lease = spare.lease;
    // The repo's main worktree gives env-resolution + origin context; the name is
    // overridden to the generic spare name (the clone is branch-less either way).
    let ctx = thegn_core::repo::main_worktree(repo_root)
        .unwrap_or_else(|| repo_root.to_path_buf())
        .to_string_lossy()
        .into_owned();
    match crate::agent::provision_provider_env_named(
        cfg,
        &ctx,
        env_name,
        Some(&name),
        &mut progress,
    ) {
        // Ok((true, _)) ONLY: Ok((false, _)) means the pipeline did nothing (not
        // a provider env, no token, no files API) — marking that `ready` would
        // mint a phantom spare whose claim skips the worktree's real
        // provisioning (the bare-sprite-shell incident). The captured
        // provisioned-base checkpoint id rides the spare's row so the recycle
        // paths (stale reconcile / worktree delete) can restore from it.
        Ok((true, checkpoint)) => {
            let lock = flake_lock_hash(repo_root);
            let ready_result = thegn_core::db::Db::open()
                .map_err(|error| {
                    anyhow::anyhow!(
                        "warm pool: cannot open ledger to publish spare {name}: {error:#}"
                    )
                })
                .and_then(|db| {
                    db.set_pool_spare_ready(&name, checkpoint.as_deref(), &lock)
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "warm pool: cannot publish spare {name} as ready: {error:#}"
                            )
                        })
                });
            if let Err(error) = ready_result {
                return Err(rollback_spare_failure(cfg, env_name, &name, error));
            }
            Ok(name)
        }
        Ok((false, _)) => {
            let error = anyhow::anyhow!(
                "env '{env_name}' cannot host a pool spare (no configured provider)"
            );
            match thegn_core::db::Db::open().and_then(|db| db.delete_pool_spare(&name)) {
                Ok(()) => Err(error),
                Err(delete_error) => Err(error.context(format!(
                    "also failed to delete unprovisioned spare intent row {name}: {delete_error:#}"
                ))),
            }
        }
        Err(error) => Err(rollback_spare_failure(cfg, env_name, &name, error)),
    }
}

/// Claim a `ready` spare for `(repo, env)` and hand it to `worktree`: bind it (DB,
/// atomic), check out the worktree's branch in the spare's workdir, mirror local
/// state, and REBIND the persisted remote location so chrome git/fs reads and CLI
/// panes route into the spare. Returns the claimed sandbox name, or `None` when no
/// spare is ready or the env can't hand one over (caller provisions fresh).
pub fn claim_spare(
    cfg: &thegn_core::config::Config,
    worktree: &str,
    repo_root: &std::path::Path,
    env_name: &str,
    branch: Option<&str>,
) -> Option<String> {
    // Hand-over requires a configured provider WITH its token: binding first and
    // failing later would leave the worktree pointing at a spare nothing can
    // reach (branch settle, parity, exec all need the provider).
    let env = cfg.env.get(env_name)?;
    crate::agent::provider_for_named(&env.provider, "claim-probe")?;
    // The DERIVED sandbox id (pre-bind), so the bare husk warm-on-open may have
    // auto-created can be destroyed once the spare takes over below.
    let derived = crate::agent::provider_sandbox_name(cfg, worktree, env_name);
    let repo = repo_root.to_string_lossy().into_owned();
    let db = thegn_core::db::Db::open().ok()?;
    let (name, checkpoint) = db.begin_pool_spare_claim(&repo, env_name).ok()??;
    let _spare_lease = {
        let mut retries = 0;
        loop {
            match crate::pool_lease::spare_lease(&name) {
                Ok(lease) => break lease,
                Err(crate::pool_lease::LeaseError::Busy) if retries < 3 => {
                    retries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(error) => {
                    quarantine_claim_failure(
                        &db,
                        &name,
                        worktree,
                        &format!("could not acquire spare lease: {error}"),
                    );
                    return None;
                }
            }
        }
    };
    // The claimed spare's provisioned-base checkpoint rides its pool row; the
    // recycle paths (stale reconcile / worktree delete) restore from it.
    tracing::debug!(
        target: "thegn::lifecycle",
        %name,
        checkpoint = checkpoint.as_deref().unwrap_or("-"),
        "claimed spare checkpoint"
    );
    let workdir = crate::provider_workdir::resolve(&env.provider, &name);
    // Per-worktree work: settle the branch in the spare's existing clone (the
    // sandbox auto-resumes when the exec opens). Best-effort — the bind already
    // succeeded, so the pane opens against the spare regardless.
    if let Some(provider) = crate::agent::provider_for_named(&env.provider, &name) {
        if let Some(b) = branch.map(str::trim).filter(|b| !b.is_empty()) {
            let wd = thegn_core::util::sh_quote(&workdir);
            let bq = thegn_core::util::sh_quote(b);
            let script = format!(
                "cd {wd} 2>/dev/null && (git checkout {bq} 2>/dev/null || git checkout -b {bq}) 2>&1"
            );
            let argv = vec!["/bin/sh".to_string(), "-lc".to_string(), script];
            let _ = crate::agent::block_on_provider(|| async {
                // best-effort: warm-up checkout: the parity step below reconciles the branch
                provider.run_exec(&name, &argv, None, &[]).await
            });
        }
        // Bring the claimed spare to full parity with the local worktree, same as
        // a fresh provision (only for an `in_env` provider — a projected data mode
        // mirrors the tree by other means). Best-effort.
        if env.data == thegn_core::config::DataMode::InEnv {
            let result =
                crate::parity::apply_local_parity(&provider, &name, worktree, &workdir, &[]);
            crate::parity::record_outcome(worktree, &result);
            if let Err(e) = &result {
                thegn_core::msg::warn(&format!(
                    "local parity on claimed spare {name}: {e}; {}; autonomous agents are held on this worktree until parity succeeds.",
                    crate::parity::failure_note(e)
                ));
            }
        }
        // The worktree's DERIVED sandbox may exist as a bare husk (auto-created
        // by warm-on-open moments before the claim); after the spare takes over
        // it is an orphan the provider keeps billing. `try_claim_spare` only
        // runs while provisioning is PENDING (derived missing or marker-less),
        // so a marker-less derived sandbox here is that husk — destroy it.
        // Best-effort: an error just leaves the husk for manual cleanup.
        if let Some(d) = derived.filter(|d| *d != name)
            && let Some(dp) = crate::agent::provider_for_named(&env.provider, &d)
        {
            let marker = thegn_core::envplan::EnvPlan::marker_path(&workdir);
            let exists = crate::agent::block_on_provider(|| async { dp.list().await })
                .map(|names| names.iter().any(|n| n == &d))
                .unwrap_or(false);
            let bare = exists
                && crate::agent::block_on_provider(|| async { dp.read(&d, &marker).await })
                    .is_err();
            if bare
                && crate::remote_enqueue_auth::revoke_for_sandbox(&env.provider, &d, None).is_ok()
            {
                let _ = crate::agent::block_on_provider(|| async { dp.destroy(&d).await }); // best-effort: cleanup: a leftover sandbox dir with no marker; the next claim re-creates it
            }
        }
    }
    // Rebind the persisted remote location: the stored control prefix embeds the
    // DERIVED sandbox id, so without this every chrome git/fs read and CLI pane
    // keeps routing into the abandoned husk instead of the claimed spare.
    // `control_command_template` (not raw `exec_command`) so VPS envs — whose
    // prefix is the implicit `thegn vps-ssh` self-bridge — rebind too.
    let prefix: Vec<String> = env
        .provider
        .control_command_template()
        .iter()
        .map(|s| s.replace("{id}", &name))
        .collect();
    let original_location = match db.location_for(worktree) {
        Ok(location) => location,
        Err(error) => {
            quarantine_claim_failure(
                &db,
                &name,
                worktree,
                &format!("could not read prior control location: {error:#}"),
            );
            return None;
        }
    };
    use thegn_core::store::PlacementStore;
    let original_tenancy = match db.tenancy_for(&name) {
        Ok(row) => row.map(|row| row.worktree),
        Err(error) => {
            quarantine_claim_failure(
                &db,
                &name,
                worktree,
                &format!("could not read prior tenancy binding: {error:#}"),
            );
            return None;
        }
    };
    let location_result = if !prefix.is_empty() {
        let loc = thegn_core::remote::GitLoc::provider_db_string(&prefix, &workdir);
        db.set_worktree_location(worktree, &loc)
    } else {
        Ok(())
    };
    let tenancy_result = db.tenancy_rebind(&name, worktree);
    match finish_claim_rebinds(
        &db,
        &name,
        worktree,
        &location_result,
        &tenancy_result,
        original_location.as_deref(),
        original_tenancy.as_deref(),
    ) {
        ClaimPublish::Publish => {
            if let Err(error) = db.publish_pool_spare_claim(&name, worktree) {
                restore_claim_rebinds(
                    &db,
                    &name,
                    worktree,
                    original_location.as_deref(),
                    original_tenancy.as_deref(),
                );
                quarantine_claim_failure(
                    &db,
                    &name,
                    worktree,
                    &format!("claim publication failed: {error:#}"),
                );
                return None;
            }
        }
        ClaimPublish::Quarantine(reason) => {
            quarantine_claim_failure(&db, &name, worktree, &reason);
            return None;
        }
    }
    thegn_core::msg::info(&format!("claimed warm spare {name} for {worktree}"));
    // The claim consumed this spare — refill the pool now rather than waiting for
    // the next ~8s maintainer tick to notice the gap (off-loop, its own thread).
    crate::lifecycle::refill_pool_after_claim(cfg, worktree);
    Some(name)
}

fn restore_claim_rebinds(
    db: &thegn_core::db::Db,
    name: &str,
    worktree: &str,
    location: Option<&str>,
    tenancy_worktree: Option<&str>,
) {
    if let Err(error) = db.restore_worktree_location(worktree, location) {
        tracing::warn!(target: "thegn::lifecycle", %name, %worktree, %error, "failed to restore worktree location after warm-pool claim failure");
    }
    if let Some(previous_worktree) = tenancy_worktree {
        use thegn_core::store::PlacementStore;
        if let Err(error) = db.tenancy_rebind(name, previous_worktree) {
            tracing::warn!(target: "thegn::lifecycle", %name, %previous_worktree, %error, "failed to restore tenancy binding after warm-pool claim failure");
        }
    }
}

fn finish_claim_rebinds(
    db: &thegn_core::db::Db,
    name: &str,
    worktree: &str,
    location: &anyhow::Result<()>,
    tenancy: &anyhow::Result<()>,
    previous_location: Option<&str>,
    previous_tenancy_worktree: Option<&str>,
) -> ClaimPublish {
    let decision = claim_publish_decision(location, tenancy);
    if matches!(&decision, ClaimPublish::Quarantine(_)) {
        restore_claim_rebinds(
            db,
            name,
            worktree,
            previous_location,
            previous_tenancy_worktree,
        );
    }
    decision
}

enum ClaimPublish {
    Publish,
    Quarantine(String),
}

fn claim_publish_decision(
    location: &anyhow::Result<()>,
    tenancy: &anyhow::Result<()>,
) -> ClaimPublish {
    if let Err(error) = location {
        ClaimPublish::Quarantine(format!("control-location rebind failed: {error:#}"))
    } else if let Err(error) = tenancy {
        ClaimPublish::Quarantine(format!("tenancy rebind failed: {error:#}"))
    } else {
        ClaimPublish::Publish
    }
}

fn quarantine_claim_failure(db: &thegn_core::db::Db, name: &str, worktree: &str, reason: &str) {
    let quarantine_status = match db.quarantine_pool_spare(name, "claiming") {
        Ok(()) => "the spare is quarantined".to_string(),
        Err(error) => {
            tracing::warn!(target: "thegn::lifecycle", %name, error = %error, "failed to quarantine warm-pool claim");
            format!("quarantine failed ({error:#})")
        }
    };
    thegn_core::msg::warn(&format!(
        "warm pool: claim of spare {name} for {worktree} failed ({reason}); {quarantine_status} and a fresh sandbox will be provisioned"
    ));
}

/// On-open location rebind: build the `GitLoc` for the freshly RESOLVED
/// provider location blob and persist it when it differs from the stored row —
/// a stale create-time location (e.g. a machine0 blob on a worktree whose env
/// now resolves to sprites) must not keep routing chrome reads and repo
/// provisioning at a dead provider. A blob that parses non-remote is returned
/// but never persisted (malformed input must not clobber a possibly-good row).
pub(crate) fn rebind_resolved_location(
    db: Option<&thegn_core::db::Db>,
    worktree: &str,
    blob: &str,
) -> thegn_core::remote::GitLoc {
    let fresh = thegn_core::remote::GitLoc::from_db(worktree, Some(blob));
    if fresh.is_remote()
        && let Some(db) = db
        && db.location_for(worktree).ok().flatten().as_deref() != Some(blob)
    {
        // best-effort: a missed rebind heals on the next open.
        let _ = db.set_worktree_location(worktree, blob);
    }
    fresh
}

/// Destroy a spare sandbox and drop its ledger row only after provider success.
pub fn destroy_spare(
    cfg: &thegn_core::config::Config,
    env_name: &str,
    name: &str,
) -> anyhow::Result<()> {
    let destroy_result = if let Some(env) = cfg.env.get(env_name)
        && let Some(provider) = crate::agent::provider_for_named(&env.provider, name)
    {
        crate::remote_enqueue_auth::revoke_for_sandbox(&env.provider, name, None).map_err(|error| {
            anyhow::anyhow!(
                "refusing to destroy spare {name}: route-to-host credential revocation failed: {error:#}"
            )
        })?;
        crate::agent::block_on_provider(|| async { provider.destroy(name).await })
    } else {
        Err(anyhow::anyhow!(
            "warm pool: cannot destroy spare {name}: env '{env_name}' has no usable provider; keeping its ledger row"
        ))
    };
    if spare_row_after_destroy(&destroy_result) == SpareRow::Keep {
        return destroy_result;
    }

    match thegn_core::db::Db::open() {
        Ok(db) => {
            if let Err(error) = db.delete_pool_spare(name) {
                tracing::warn!(target: "thegn::db", %error, spare = name, "failed to delete destroyed spare ledger row");
            }
            // Free any placement-engine slot the spare held (no-op otherwise).
            use thegn_core::store::PlacementStore;
            if let Err(error) = db.tenancy_release(name, unix_now_secs()) {
                tracing::warn!(target: "thegn::db", %error, spare = name, "failed to release destroyed spare tenancy");
            }
        }
        Err(error) => {
            tracing::warn!(target: "thegn::db", %error, spare = name, "could not open ledger after spare destroy");
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum SpareRow {
    Drop,
    Keep,
}

fn spare_row_after_destroy(outcome: &anyhow::Result<()>) -> SpareRow {
    if outcome.is_ok() {
        SpareRow::Drop
    } else {
        SpareRow::Keep
    }
}

fn rollback_spare_failure(
    cfg: &thegn_core::config::Config,
    env_name: &str,
    name: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    match destroy_spare(cfg, env_name, name) {
        Ok(()) => error,
        Err(rollback) => error.context(format!("rollback also failed: {rollback:#}")),
    }
}

fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| thegn_core::time_policy::saturating_i64(u128::from(d.as_secs())))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn claim_publish_decision_quarantines_any_failed_rebind() {
        assert!(matches!(
            claim_publish_decision(&Ok(()), &Ok(())),
            ClaimPublish::Publish
        ));

        let location_error = anyhow::anyhow!("location denied");
        match claim_publish_decision(&Err(location_error), &Ok(())) {
            ClaimPublish::Quarantine(reason) => assert!(reason.contains("location denied")),
            ClaimPublish::Publish => panic!("failed location rebind must quarantine"),
        }

        let tenancy_error = anyhow::anyhow!("tenancy denied");
        assert!(matches!(
            claim_publish_decision(&Ok(()), &Err(tenancy_error)),
            ClaimPublish::Quarantine(_)
        ));
    }

    #[test]
    fn tenancy_rebind_failure_restores_location_and_tenancy() {
        use thegn_core::capacity::ResourceReq;
        use thegn_core::host::HostId;
        use thegn_core::store::{PlacementStore, TenancyMode, TenancyRow, TenancyState};

        let db = thegn_core::db::Db::open_memory().unwrap();
        let worktree = "/wt/claim-rollback";
        let previous_location = "old-location";
        db.put_worktree(
            "tab",
            "/repo",
            worktree,
            "feat",
            Some(previous_location),
            None,
        )
        .unwrap();
        db.set_worktree_location(worktree, "spare-location")
            .unwrap();
        db.tenancy_force(
            &TenancyRow {
                sandbox: "spare-rollback".into(),
                host: HostId::named("test"),
                worktree: "/wt/original".into(),
                zone: String::new(),
                mode: TenancyMode::Packed,
                req: ResourceReq {
                    cpu_floor_milli: 1,
                    mem_floor_mb: 1,
                    cpu_ceiling_milli: None,
                    mem_ceiling_mb: None,
                },
                state: TenancyState::Reserved,
                reserved_at: 0,
                activated_at: None,
                released_at: None,
            },
            0,
        )
        .unwrap();
        db.tenancy_rebind("spare-rollback", worktree).unwrap();

        let failure = anyhow::anyhow!("injected tenancy failure");
        assert!(matches!(
            finish_claim_rebinds(
                &db,
                "spare-rollback",
                worktree,
                &Ok(()),
                &Err(failure),
                Some(previous_location),
                Some("/wt/original"),
            ),
            ClaimPublish::Quarantine(_)
        ));

        assert_eq!(
            db.location_for(worktree).unwrap().as_deref(),
            Some(previous_location)
        );
        assert_eq!(
            db.tenancy_for("spare-rollback").unwrap().unwrap().worktree,
            "/wt/original"
        );
    }

    #[test]
    fn rebind_resolved_location_persists_only_remote_diffs() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        let wt = "/wt/rebind";
        let old = thegn_core::remote::GitLoc::provider_db_string(
            &["machine0-ssh".into(), "vm".into()],
            "/workspace",
        );
        db.put_worktree("tab", "/repo", wt, "feat", Some(&old), None)
            .unwrap();
        // A different resolved provider blob replaces the stale row.
        let fresh = thegn_core::remote::GitLoc::provider_db_string(
            &["sprite".into(), "exec".into()],
            "/workspace",
        );
        let loc = rebind_resolved_location(Some(&db), wt, &fresh);
        assert!(loc.is_remote());
        assert_eq!(
            db.location_for(wt).unwrap().as_deref(),
            Some(fresh.as_str())
        );
        // Same blob again: no-op (equality short-circuit).
        let loc = rebind_resolved_location(Some(&db), wt, &fresh);
        assert!(loc.is_remote());
        // A malformed blob parses Local and must NOT clobber the stored row.
        let loc = rebind_resolved_location(Some(&db), wt, "not-a-blob");
        assert!(!loc.is_remote());
        assert_eq!(
            db.location_for(wt).unwrap().as_deref(),
            Some(fresh.as_str())
        );
    }

    #[test]
    fn sandbox_lock_serializes_same_name() {
        // Thread B must not acquire "same" until A drops it — the eager-vs-
        // materialize double-provision race, distilled.
        let a = sandbox_lock("same");
        let (tx, rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            let _b = sandbox_lock("same");
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "second lock on the same sandbox must block while the first is held"
        );
        drop(a);
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "second lock must acquire once the first is released"
        );
        t.join().unwrap();
    }

    #[test]
    fn sandbox_lock_distinct_names_dont_contend() {
        let _a = sandbox_lock("name-a");
        let (tx, rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            let _b = sandbox_lock("name-b");
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "a lock on a different sandbox must not block"
        );
        t.join().unwrap();
    }

    #[test]
    fn worktree_live_only_while_a_guard_is_held() {
        assert!(!worktree_live("/wt/x"));
        let g1 = worktree_live_guard("/wt/x");
        assert!(worktree_live("/wt/x"));
        // Shared: a second holder (eager + materialize overlap) stacks.
        let g2 = worktree_live_guard("/wt/x");
        drop(g1);
        assert!(
            worktree_live("/wt/x"),
            "still live while the second holder remains"
        );
        drop(g2);
        assert!(!worktree_live("/wt/x"));
        // Independent per worktree.
        let _g = worktree_live_guard("/wt/x");
        assert!(!worktree_live("/wt/y"));
    }

    #[test]
    fn poolable_env_requires_configured_provider_placement() {
        // The implicit "default" env (no `[env.default]` table) must never
        // pool: provisioning a spare for it no-ops instantly, minting a phantom
        // `ready` spare whose claim skips the worktree's real provisioning.
        use thegn_core::config::{Config, EnvConfig, PlacementMode};
        let mut cfg = Config::default();
        assert!(!poolable_env(&cfg, "default"));
        assert!(!poolable_env(&cfg, "no-such-env"));
        cfg.env.insert(
            "sprites".into(),
            EnvConfig {
                placement: PlacementMode::Provider,
                ..Default::default()
            },
        );
        assert!(poolable_env(&cfg, "sprites"));
        // A configured but non-provider env has no sandbox to mint/hand over.
        cfg.env.insert(
            "laptop".into(),
            EnvConfig {
                placement: PlacementMode::Local,
                ..Default::default()
            },
        );
        assert!(!poolable_env(&cfg, "laptop"));
    }

    #[test]
    fn claim_refused_before_binding_when_env_cannot_hand_over() {
        use thegn_core::config::{Config, EnvConfig, EnvProviderConfig, PlacementMode};
        // Both refusals fire BEFORE any DB access (no bind to roll back):
        // an unconfigured env name…
        let cfg = Config::default();
        let repo = std::path::Path::new("/no/such/repo");
        assert!(claim_spare(&cfg, "/no/such/wt", repo, "default", None).is_none());
        // …and a provider env whose API token is absent.
        let mut cfg = Config::default();
        cfg.env.insert(
            "sprites".into(),
            EnvConfig {
                placement: PlacementMode::Provider,
                provider: EnvProviderConfig {
                    provider: "sprites".into(),
                    api_key_env: "TG_TEST_NO_SUCH_TOKEN_VAR".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert!(claim_spare(&cfg, "/no/such/wt", repo, "sprites", None).is_none());
    }

    #[test]
    fn mint_spare_name_is_repo_slugged_and_unique() {
        let a = mint_spare_name("/home/u/code/thegn").unwrap();
        let b = mint_spare_name("/home/u/code/thegn").unwrap();
        assert!(a.starts_with("thegn-pool-"), "{a}");
        assert_ne!(a, b, "independent random mints should differ");
        for name in [&a, &b] {
            let suffix = name.strip_prefix("thegn-pool-").unwrap();
            assert_eq!(suffix.len(), 12, "{name}");
            assert!(
                suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
                "{name}"
            );
        }
    }

    #[test]
    fn flake_lock_hash_empty_without_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(flake_lock_hash(dir.path()), "");
    }

    #[test]
    fn flake_lock_hash_uses_sha256_of_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("flake.lock");
        std::fs::write(&lock, b"abc").unwrap();
        let hash = flake_lock_hash(dir.path());
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hash.len(), 64);
        assert!(
            hash.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );

        std::fs::write(&lock, b"abd").unwrap();
        assert_ne!(flake_lock_hash(dir.path()), hash);
    }

    #[test]
    fn flake_lock_hash_preserves_invalid_utf8_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("flake.lock");
        std::fs::write(&lock, [0xff, 0xfe]).unwrap();
        let first = flake_lock_hash(dir.path());
        std::fs::write(&lock, [0xff, 0xfd]).unwrap();
        assert_ne!(first, flake_lock_hash(dir.path()));
    }

    #[test]
    fn flake_lock_hash_marks_unreadable_lockfile_and_changes_per_call() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("flake.lock")).unwrap();
        let first = flake_lock_hash(dir.path());
        let second = flake_lock_hash(dir.path());
        assert!(first.starts_with("unreadable:"), "{first}");
        assert_ne!(
            first, second,
            "unreadable lockfiles must never reuse a stored key"
        );
    }

    #[test]
    fn spare_ledger_destroy_without_provider_keeps_row() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open().unwrap();
        db.insert_pool_spare("ledger-no-provider", "/repo", "missing-env")
            .unwrap();

        let result = destroy_spare(
            &thegn_core::config::Config::default(),
            "missing-env",
            "ledger-no-provider",
        );

        assert!(result.is_err());
        assert!(
            db.pool_spare_by_name("ledger-no-provider")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn spare_ledger_unprovisioned_spare_drops_intent_row() {
        use thegn_core::config::{EnvConfig, EnvProviderConfig, PlacementMode};

        let _state = thegn_core::testenv::IsolatedState::new();
        let repo = tempfile::tempdir().unwrap();
        let mut cfg = thegn_core::config::Config::default();
        cfg.env.insert(
            "sprites".into(),
            EnvConfig {
                placement: PlacementMode::Provider,
                provider: EnvProviderConfig {
                    provider: "sprites".into(),
                    api_key_env: "TG_TEST_SPARE_LEDGER_NO_TOKEN".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        let result = provision_spare(&cfg, repo.path(), "sprites", |_| {});

        assert!(
            result.is_err(),
            "missing provider token must refuse provisioning"
        );
        assert!(
            thegn_core::db::Db::open()
                .unwrap()
                .pool_spares_for(&repo.path().to_string_lossy(), "sprites")
                .unwrap()
                .is_empty(),
            "an unprovisioned spare must not leave its intent row behind"
        );
    }

    #[test]
    fn spare_ledger_failed_destroy_keeps_row() {
        let failed = Err(anyhow::anyhow!("provider destroy failed"));
        let succeeded = Ok(());
        assert_eq!(spare_row_after_destroy(&failed), SpareRow::Keep);
        assert_eq!(spare_row_after_destroy(&succeeded), SpareRow::Drop);
    }

    #[test]
    fn claim_selection_never_claims_on_an_unreadable_selection() {
        assert!(claim_selection("/wt", Err(anyhow::anyhow!("locked"))).is_none());
    }

    #[test]
    fn claim_selection_passes_a_read_through() {
        assert_eq!(
            claim_selection("/wt", Ok(Some("picked".into()))),
            Some(Some("picked".into()))
        );
    }

    #[test]
    fn attach_selection_halts_on_an_unreadable_selection() {
        match attach_selection(Err(anyhow::anyhow!("locked"))) {
            Err(halt) => {
                assert_eq!(halt.env_name, "?");
                assert!(halt.reason.contains("could not be read"));
            }
            Ok(_) => panic!("unreadable selection must halt attach"),
        }
    }

    #[test]
    fn attach_selection_passes_a_read_through() {
        assert_eq!(
            attach_selection(Ok(Some("picked".into()))).unwrap(),
            Some("picked".into())
        );
    }

    #[test]
    fn guard_unprovisioned_attach_refuses_an_unreadable_location() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().to_string_lossy().into_owned();
        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree("app/wt", &wt, &wt, "main", Some("garbage"), None)
            .unwrap();
        drop(db);

        let result =
            guard_unprovisioned_attach(&thegn_core::config::Config::default(), &wt, "shell", true);
        assert!(
            matches!(result, Err(error) if error.to_string().contains("provider attach refused"))
        );
    }
}
