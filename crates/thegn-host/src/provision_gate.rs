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

/// Whether a worktree may try a warm claim given its stored location and resolved placement.
pub(crate) fn claim_location_gate(
    stored_is_remote: bool,
    provider_placed: bool,
    live: bool,
) -> bool {
    !live && (stored_is_remote || provider_placed)
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
    if worktree_live(worktree) {
        return false;
    }
    // Serialize warm claims per worktree across processes (TUI materialize vs a
    // CLI `agent run`): the binding check below and the publish are not atomic.
    let Ok(_claim_lease) = crate::pool_lease::pool_lease(worktree, "\u{0}worktree-claim") else {
        tracing::debug!(target: "thegn::lifecycle", worktree = %worktree, "warm claim skipped: another claim for this worktree is in flight");
        return false;
    };
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
        Some(db) => db.effective_env_selection(worktree, &repo_root.to_string_lossy()),
        None => Err(anyhow::anyhow!("the state database could not be opened")),
    };
    let mut selection = match read {
        Ok(selection) => selection,
        Err(error) => {
            let _selection = claim_selection(worktree, Err(error));
            return false;
        }
    };
    let Some(selected) = claim_selection(worktree, Ok(selection.name.clone())) else {
        return false;
    };
    selection.name = selected;
    let environment = cfg.resolve_env(&repo_root, &loc, std::path::Path::new(worktree), &selection);
    let provider_placed = matches!(
        environment.placement,
        thegn_core::placement::Placement::Provider(_)
    );
    if !claim_location_gate(loc.is_remote(), provider_placed, false) {
        return false;
    }
    if matches!(
        environment.placement,
        thegn_core::placement::Placement::Provider(_)
    ) && thegn_core::sandbox_floor::provider_refusal(
        true,
        &environment.sandbox,
        &environment.name,
    )
    .is_some()
    {
        return false;
    }
    let env_name = environment.name;
    // A warm spare is provisioned before the user's model-credential answer
    // exists, so claiming one for an unset Sprites env would bypass the
    // one-time question (and could hand over the wrong credential state).
    // Only a remembered "forward" may claim; every other answer provisions
    // through the focused path, which prompts or withholds.
    if let Some(env_cfg) = cfg.env.get(&env_name) {
        let kind = thegn_core::config::EnvProviderKind::of(&env_cfg.provider.provider);
        if kind == thegn_core::config::EnvProviderKind::Sprites
            && !env_cfg.provider.model_credentials_is_explicit()
        {
            let consent = db.as_ref().and_then(|db| {
                thegn_core::model_credential_consent::load(
                    db,
                    &repo_root.to_string_lossy(),
                    &env_cfg.provider.provider,
                )
                .ok()
                .flatten()
            });
            if consent != Some(thegn_core::model_credential_consent::Consent::Forward) {
                tracing::debug!(
                    target: "thegn::lifecycle",
                    worktree = %worktree,
                    env = %env_name,
                    "warm claim skipped: model credentials need a per-project answer first"
                );
                return false;
            }
            crate::agent::memo_model_credential_consent(
                &env_cfg.provider.provider,
                worktree,
                thegn_core::model_credential_consent::Consent::Forward,
            );
        }
    }
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
    // A parked (stopped) Vercel sandbox is stateful and provisioned: never
    // rebind it, and never resume it to look.
    if crate::agent::provision_check(cfg, worktree) != crate::agent::ProvisionCheck::Pending {
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

/// Headless warm start: before `provision_headless_blocking` provisions a
/// worktree, claim a `ready` pool spare for the same (repo, env) and lockfile
/// through [`try_claim_spare`]. `true` means a spare now serves `worktree` and
/// provisioning is skipped. Never creates: the pool stays opt-in
/// (`[lifecycle.pool] size`). Off-loop only.
///
/// `false` unless the worktree's effective env is a provider env whose pool
/// target is above zero ([`headless_pool_gate`], reads only). A claim failure
/// is a debug log plus `false`: the caller provisions cold.
pub(crate) fn claim_spare_for_headless(cfg: &thegn_core::config::Config, worktree: &str) -> bool {
    let gate = match thegn_core::db::Db::open() {
        Ok(db) => headless_pool_gate(cfg, &db, worktree),
        Err(error) => {
            tracing::debug!(target: "thegn::lifecycle", worktree = %worktree, error = %format!("{error:#}"), "headless warm claim skipped: the state database could not be opened");
            false
        }
    };
    if !gate {
        return false;
    }
    let claimed = try_claim_spare(cfg, worktree);
    if !claimed {
        tracing::debug!(target: "thegn::lifecycle", worktree = %worktree, "headless warm claim found no usable spare; provisioning cold");
    }
    claimed
}

/// Whether a headless open may try the warm pool: the worktree's effective env
/// is a configured provider env and its pool target (runtime override, else
/// `[lifecycle.pool] size`) is above zero. Reads only: never writes the DB, so
/// the default (pool off) costs a selection read and nothing else.
fn headless_pool_gate(
    cfg: &thegn_core::config::Config,
    db: &thegn_core::db::Db,
    worktree: &str,
) -> bool {
    let repo_root = db
        .repo_root_for(worktree)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| thegn_core::repo::main_worktree(std::path::Path::new(worktree)))
        .unwrap_or_else(|| std::path::PathBuf::from(worktree));
    let repo = repo_root.to_string_lossy().into_owned();
    let selection = match db.effective_env_selection(worktree, &repo) {
        Ok(selection) => selection,
        Err(error) => {
            tracing::debug!(target: "thegn::lifecycle", worktree = %worktree, error = %format!("{error:#}"), "headless warm claim skipped: environment selection unreadable");
            return false;
        }
    };
    let loc = thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(worktree));
    let environment = cfg.resolve_env(&repo_root, &loc, std::path::Path::new(worktree), &selection);
    if !matches!(
        environment.placement,
        thegn_core::placement::Placement::Provider(_)
    ) || !poolable_env(cfg, &environment.name)
    {
        return false;
    }
    match crate::lifecycle::effective_pool_target(db, cfg, &repo, &environment.name) {
        Ok(target) if target > 0 => true,
        // Pool off: a spare left from an earlier bake (checkpointed, keyed on
        // the flake lock) still skips the cold create for a repeat `env up`.
        Ok(_) => match db.pool_spares_for(&repo, &environment.name) {
            Ok(spares) => has_fresh_ready_spare(&spares, &flake_lock_hash(&repo_root)),
            Err(error) => {
                tracing::debug!(target: "thegn::lifecycle", worktree = %worktree, error = %format!("{error:#}"), "headless warm claim skipped: spare ledger unreadable");
                false
            }
        },
        Err(error) => {
            tracing::debug!(target: "thegn::lifecycle", worktree = %worktree, error = %format!("{error:#}"), "headless warm claim skipped: pool target unreadable");
            false
        }
    }
}

/// Whether `spares` holds a `ready` spare baked against exactly `lock` (the
/// repo's current flake-lock hash). A spare with no recorded hash or a
/// different one is stale: claiming it would hand over an outdated devShell.
fn has_fresh_ready_spare(spares: &[thegn_core::db::PoolSpare], lock: &str) -> bool {
    spares
        .iter()
        .any(|s| s.state == "ready" && s.lock_hash.as_deref() == Some(lock))
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
    let Some((env_name, id)) = provider_target(cfg, worktree)? else {
        return Ok(());
    };
    if !crate::provider_workdir::is_provisioned_locally(&id) {
        return Err(unprovisioned_halt(env_name, worktree).into());
    }
    if let Some(text) = deferred_attach_refusal(
        cfg,
        choice,
        worktree,
        thegn_core::provision_journal::read(&id).as_ref(),
    ) {
        anyhow::bail!(text);
    }
    Ok(())
}

pub(crate) fn deferred_attach_refusal(
    cfg: &thegn_core::config::Config,
    choice: &str,
    worktree: &str,
    journal: Option<&thegn_core::provision_journal::Journal>,
) -> Option<String> {
    use thegn_core::provision_journal::{AgentGate, agent_gate, is_agent_choice};

    if !is_agent_choice(cfg, choice) || agent_gate(journal) != AgentGate::Wait {
        return None;
    }
    let step = journal
        .map(|journal| journal.step.as_str())
        .filter(|step| !step.is_empty())
        .unwrap_or("background setup")
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    Some(format!(
        "{choice} is not started yet: its CLI and login are still being set up in the background ({step}); it opens once setup finishes. Run `thegn env up --worktree {worktree}` to wait for it"
    ))
}

pub(crate) fn deferred_pending_env(
    cfg: &thegn_core::config::Config,
    worktree: &str,
) -> anyhow::Result<Option<String>> {
    let Some((env_name, id)) = provider_target(cfg, worktree)? else {
        return Ok(None);
    };
    Ok(thegn_core::provision_journal::read(&id).map(|_| env_name))
}

/// The env name when `worktree` resolves to a provider env whose sandbox has NOT
/// finished provisioning on this host (no LOCAL marker); `None` for a non-provider
/// env or an already-provisioned sandbox. Cheap (local stat + config resolve, no
/// network). Shared by the attach gate and the headless `session open` path,
/// which provisions instead of refusing.
pub(crate) fn unprovisioned_provider_env(
    cfg: &thegn_core::config::Config,
    worktree: &str,
) -> anyhow::Result<Option<String>> {
    let Some((env_name, id)) = provider_target(cfg, worktree)? else {
        return Ok(None);
    };
    if crate::provider_workdir::is_provisioned_locally(&id) {
        return Ok(None);
    }
    Ok(Some(env_name))
}

/// `(env name, sandbox id)` when `worktree` resolves to a provider env with a
/// sandbox name; `None` otherwise. No network.
pub(crate) fn provider_target(
    cfg: &thegn_core::config::Config,
    worktree: &str,
) -> anyhow::Result<Option<(String, String)>> {
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
    let mut selection = thegn_core::db::Db::open()
        .and_then(|db| db.effective_env_selection(worktree, &repo_root.to_string_lossy()))
        .map_err(|error| attach_selection(Err(error)).expect_err("failed selection read"))?;
    let selected_env = attach_selection(Ok(selection.name.clone())).map_err(anyhow::Error::from)?;
    selection.name = selected_env.clone();
    let env_name = cfg
        .resolve_env(&repo_root, &loc, std::path::Path::new(worktree), &selection)
        .name;
    let Some(id) = crate::agent::provider_sandbox_name(cfg, worktree, &env_name) else {
        return Ok(None);
    };
    Ok(Some((env_name, id)))
}

/// The refusal for an attach onto an unprovisioned provider sandbox. Names the
/// headless way out (`thegn env up --worktree <path>`): only the TUI loading
/// screen provisions on open, so a CLI/daemon caller retrying alone never
/// succeeds.
pub(crate) fn unprovisioned_halt(env_name: String, worktree: &str) -> crate::agent::SandboxHalt {
    crate::agent::SandboxHalt {
        env_name,
        placement: "provider".into(),
        reason: format!(
            "the sandbox is not provisioned yet — run `thegn env up --worktree {worktree}` \
             (or wait for the TUI's setup screen to finish), then retry"
        ),
        ask: false,
        dormant: None,
        sealed_home: false,
    }
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
            sealed_home: false,
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
/// spare is ready or the env can't hand one over (caller provisions fresh). A
/// spare ledger that cannot be read is reported visibly (status line and toast
/// in the TUI) and returns `None`, so the caller provisions fresh; it is never
/// silently treated as an empty pool. The claiming worktree's declared egress
/// policy is verified (and re-applied on drift) on the spare before hand-over;
/// a failure quarantines the claim and the caller provisions fresh.
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
    let db = match thegn_core::db::Db::open() {
        Ok(db) => db,
        Err(error) => {
            crate::close_teardown::warn_visible(&spare_ledger_unreadable_warning(
                env_name, worktree, &error,
            ));
            return None;
        }
    };
    let selection = match db.effective_env_selection(worktree, &repo) {
        Ok(selection) => selection,
        Err(error) => {
            crate::close_teardown::warn_visible(&format!(
                "warm pool: cannot read worktree isolation selection for {worktree}: {error:#}"
            ));
            return None;
        }
    };
    let (name, checkpoint) = claim_ledger_pick(
        db.begin_pool_spare_claim(&repo, env_name),
        env_name,
        worktree,
        crate::close_teardown::warn_visible,
    )?;
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
    let egress_loc = thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(worktree));
    let mut egress_selection = selection.clone();
    egress_selection.name = Some(env_name.to_string());
    let egress_env = cfg.resolve_env(
        repo_root,
        &egress_loc,
        std::path::Path::new(worktree),
        &egress_selection,
    );
    let egress_provider = crate::agent::provider_for_named(&env.provider, &name);
    let egress = match egress_provider.as_ref() {
        Some(provider) => {
            if crate::egress_admit::lists_unenforced(
                provider.caps().egress,
                &egress_env.sandbox.network_allow,
                &egress_env.sandbox.network_block,
            ) {
                crate::egress_admit::warn_unsupported_once(
                    crate::egress_admit::provider_label(provider),
                    &name,
                );
            }
            let steps = crate::egress_admit::BlockingProviderEgress { provider };
            claim_egress_gate(
                Some((&steps, provider.caps().egress)),
                &name,
                &egress_env.sandbox.network_allow,
                &egress_env.sandbox.network_block,
            )
        }
        None => claim_egress_gate(
            None,
            &name,
            &egress_env.sandbox.network_allow,
            &egress_env.sandbox.network_block,
        ),
    };
    if let Err(reason) = egress {
        quarantine_claim_failure(&db, &name, worktree, &reason);
        return None;
    }
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

fn claim_egress_gate(
    provider: Option<(&dyn crate::egress_admit::EgressSteps, bool)>,
    name: &str,
    allow: &[String],
    block: &[String],
) -> Result<(), String> {
    if allow.is_empty() && block.is_empty() {
        return Ok(());
    }
    let Some((steps, caps_egress)) = provider else {
        return Err(format!(
            "no provider is available to verify the claiming worktree's network egress policy on spare {name}"
        ));
    };
    // Without an egress policy API the lists are unenforced: the caller warns
    // and the claim proceeds.
    if let crate::egress_admit::EgressAdmission::Skip
    | crate::egress_admit::EgressAdmission::SkipUnsupported =
        crate::egress_admit::egress_admission(false, caps_egress, allow, block)
    {
        return Ok(());
    }
    crate::egress_admit::verify_existing_egress(steps, name, allow, block).map_err(|error| {
        format!(
            "the claiming worktree's network egress policy could not be verified on spare {name}: {error:#}"
        )
    })
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

/// The healed location blob for a stored provider `row` whose transport prefix
/// no longer matches the env's `fresh_prefix` (THE-2054: a sprite row written
/// under the vendor `sprite exec` CLI after the env moved to the native
/// `sprite-exec` bridge). Keeps the row's in-sandbox path. `None` when the row
/// is not a provider blob (local/ssh rows are never touched), the fresh prefix
/// is empty (an unresolvable transport never clobbers a working row), or the
/// prefixes already agree.
pub(crate) fn healed_blob(row: &str, fresh_prefix: &[String]) -> Option<String> {
    match thegn_core::remote::GitLoc::from_db("", Some(row)) {
        thegn_core::remote::GitLoc::Provider {
            control_prefix,
            path,
        } if !fresh_prefix.is_empty() && control_prefix != fresh_prefix => Some(
            thegn_core::remote::GitLoc::provider_db_string(fresh_prefix, &path),
        ),
        _ => None,
    }
}

/// Launch-time heal of a stale stored provider transport prefix: re-resolve
/// the worktree's env and, when its provider control prefix differs from the
/// stored row's, persist the fresh prefix (keeping the row's path) and return
/// the rebuilt location. Only provider rows are considered and the new prefix
/// comes from config, so this never widens transport. `None` = nothing to heal.
pub(crate) fn heal_provider_prefix(
    db: &thegn_core::db::Db,
    cfg: &thegn_core::config::Config,
    repo_root: &std::path::Path,
    worktree: &str,
    selected_env: Option<String>,
) -> Option<thegn_core::remote::GitLoc> {
    let row = db.location_for(worktree).ok().flatten()?;
    let row_loc = thegn_core::remote::GitLoc::from_db(worktree, Some(&row));
    if !matches!(row_loc, thegn_core::remote::GitLoc::Provider { .. }) {
        return None;
    }
    let env = cfg.resolve_env(
        repo_root,
        &row_loc,
        std::path::Path::new(worktree),
        &thegn_core::sandbox_pin::EnvSelection {
            name: selected_env,
            profile: None,
        },
    );
    let thegn_core::placement::Placement::Provider(p) = &env.placement else {
        return None;
    };
    let new = healed_blob(&row, &p.control_prefix)?;
    // best-effort: a failed write still launches over the fresh prefix; the
    // next launch retries the heal.
    let _ = db.set_worktree_location(worktree, &new);
    tracing::info!(target: "thegn::lifecycle", %worktree, "healed stale provider transport prefix for {worktree}");
    Some(thegn_core::remote::GitLoc::from_db(worktree, Some(&new)))
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

fn spare_ledger_unreadable_warning(
    env_name: &str,
    worktree: &str,
    error: &anyhow::Error,
) -> String {
    format!(
        "warm pool: the spare ledger for env '{env_name}' could not be read while opening {worktree} ({error:#}); provisioning a fresh sandbox instead of claiming a ready spare"
    )
}

fn claim_ledger_pick(
    read: anyhow::Result<Option<(String, Option<String>)>>,
    env_name: &str,
    worktree: &str,
    warn: impl FnOnce(&str),
) -> Option<(String, Option<String>)> {
    match read {
        Ok(picked) => picked,
        Err(error) => {
            warn(&spare_ledger_unreadable_warning(env_name, worktree, &error));
            None
        }
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
    fn claim_lease_is_exclusive_per_worktree() {
        let _held =
            crate::pool_lease::pool_lease("/w", "\u{0}worktree-claim").expect("first claim lease");
        assert!(matches!(
            crate::pool_lease::pool_lease("/w", "\u{0}worktree-claim"),
            Err(crate::pool_lease::LeaseError::Busy)
        ));
        assert!(crate::pool_lease::pool_lease("/other", "\u{0}worktree-claim").is_ok());
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn healed_blob_rewrites_only_stale_provider_prefixes() {
        let stale = argv(&["sprite", "exec", "-s", "sb", "--"]);
        let fresh = argv(&["/bin/thegn", "sprite-exec", "sb", "--"]);
        let row = thegn_core::remote::GitLoc::provider_db_string(&stale, "/workspace/repo");

        // Stale prefix → rewritten, path kept.
        let healed = healed_blob(&row, &fresh).expect("stale prefix heals");
        match thegn_core::remote::GitLoc::from_db("/wt", Some(&healed)) {
            thegn_core::remote::GitLoc::Provider {
                control_prefix,
                path,
            } => {
                assert_eq!(control_prefix, fresh);
                assert_eq!(path, "/workspace/repo");
            }
            other => panic!("provider loc expected, got {other:?}"),
        }

        // Equal prefix → nothing to do.
        assert_eq!(healed_blob(&healed, &fresh), None);
        // Empty fresh prefix never clobbers a working row.
        assert_eq!(healed_blob(&row, &[]), None);
        // ssh blobs are never touched (lane 3's ssh rows stay as written).
        let ssh = thegn_core::remote::GitLoc::remote_db_string("host", 22, false, "/srv/wt");
        assert_eq!(healed_blob(&ssh, &fresh), None);
        // Non-blobs (local / garbage) → None.
        assert_eq!(healed_blob("local", &fresh), None);
        assert_eq!(healed_blob("", &fresh), None);
        assert_eq!(healed_blob("not json", &fresh), None);
    }

    fn sprites_auto_cfg_with_stale_exec_command() -> thegn_core::config::Config {
        use thegn_core::config::{EnvConfig, EnvProviderConfig, PlacementMode, ProviderExecMode};
        let mut cfg = thegn_core::config::Config::default();
        cfg.env.insert(
            "sprites".into(),
            EnvConfig {
                placement: PlacementMode::Provider,
                provider: EnvProviderConfig {
                    provider: "sprites".into(),
                    exec: ProviderExecMode::Auto,
                    exec_command: argv(&["sprite", "exec", "-s", "{id}", "--"]),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        cfg
    }

    #[test]
    fn heal_provider_prefix_rewrites_stale_sprite_cli_row() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().to_string_lossy().into_owned();
        let worktree = repo.path().join("wt-heal").to_string_lossy().into_owned();
        let db = thegn_core::db::Db::open().unwrap();
        let stale = thegn_core::remote::GitLoc::provider_db_string(
            &argv(&["sprite", "exec", "-s", "sb-heal", "--"]),
            "/workspace/wt-heal",
        );
        db.put_worktree(
            "tab",
            &repo_root,
            &worktree,
            "feat",
            Some(stale.as_str()),
            None,
        )
        .unwrap();
        let cfg = sprites_auto_cfg_with_stale_exec_command();

        let healed =
            heal_provider_prefix(&db, &cfg, repo.path(), &worktree, Some("sprites".into()))
                .expect("a stale sprite CLI row heals");

        let stored = db.location_for(&worktree).unwrap().expect("row kept");
        assert_ne!(stored, stale, "the row is rewritten");
        let prefix_of = |loc: &thegn_core::remote::GitLoc| match loc {
            thegn_core::remote::GitLoc::Provider { control_prefix, .. } => control_prefix.clone(),
            other => panic!("provider loc expected, got {other:?}"),
        };
        let stored_loc = thegn_core::remote::GitLoc::from_db(&worktree, Some(&stored));
        assert_eq!(
            prefix_of(&stored_loc),
            prefix_of(&healed),
            "the returned loc carries the new row"
        );
        match healed {
            thegn_core::remote::GitLoc::Provider {
                control_prefix,
                path,
            } => {
                assert_eq!(
                    control_prefix.get(1).map(String::as_str),
                    Some("sprite-exec"),
                    "{control_prefix:?}"
                );
                assert_eq!(path, "/workspace/wt-heal", "the row's path is kept");
            }
            other => panic!("provider loc expected, got {other:?}"),
        }
        // Idempotent: a second launch finds nothing to heal.
        assert!(
            heal_provider_prefix(&db, &cfg, repo.path(), &worktree, Some("sprites".into()))
                .is_none()
        );
    }

    #[test]
    fn heal_provider_prefix_leaves_local_worktrees_alone() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().to_string_lossy().into_owned();
        let worktree = repo.path().join("wt-local").to_string_lossy().into_owned();
        let db = thegn_core::db::Db::open().unwrap();
        let cfg = sprites_auto_cfg_with_stale_exec_command();

        // No row at all.
        assert!(
            heal_provider_prefix(&db, &cfg, repo.path(), &worktree, Some("sprites".into()))
                .is_none()
        );
        assert_eq!(db.location_for(&worktree).unwrap(), None);

        // A local row (no location) is never written.
        db.put_worktree("tab", &repo_root, &worktree, "feat", None, None)
            .unwrap();
        assert!(
            heal_provider_prefix(&db, &cfg, repo.path(), &worktree, Some("sprites".into()))
                .is_none()
        );
        assert_eq!(
            db.location_for(&worktree).unwrap(),
            None,
            "no write happens"
        );
    }

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

    /// b162 D9: the headless warm claim is off unless a pool target is set,
    /// and deciding that writes nothing to the DB.
    #[test]
    fn headless_claim_gate_is_false_without_a_pool_and_writes_nothing() {
        let _state = thegn_core::testenv::IsolatedState::new();
        use thegn_core::config::{Config, EnvConfig, EnvProviderConfig, PlacementMode};
        let db = thegn_core::db::Db::open_memory().unwrap();
        let wt = "/nonexistent/b162/headless-wt";
        // No env tables at all.
        let mut cfg = Config::default();
        assert!(!headless_pool_gate(&cfg, &db, wt));
        // A provider env pinned to the worktree, but `[lifecycle.pool] size`
        // left at its default (0) and no runtime override.
        cfg.env.insert(
            "sprites".into(),
            EnvConfig {
                placement: PlacementMode::Provider,
                provider: EnvProviderConfig {
                    provider: "sprites".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        db.set_worktree_env(wt, "sprites").unwrap();
        assert_eq!(cfg.lifecycle.pool.size, 0);
        assert!(!headless_pool_gate(&cfg, &db, wt));
        // Zero writes from the gate: no pool target recorded, no spare rows.
        for repo in [wt, "/nonexistent/b162"] {
            assert_eq!(db.pool_target(repo, "sprites").unwrap(), None);
            assert!(db.pool_spares_for(repo, "sprites").unwrap().is_empty());
        }
    }

    #[test]
    fn fresh_ready_spare_requires_ready_state_and_matching_lock() {
        let spare = |state: &str, lock: Option<&str>| thegn_core::db::PoolSpare {
            sandbox_name: "s".into(),
            repo_path: "/r".into(),
            env_name: "sprites".into(),
            state: state.into(),
            checkpoint_id: None,
            lock_hash: lock.map(str::to_string),
            created_at: 0,
            updated_at: 0,
        };
        assert!(!has_fresh_ready_spare(&[], "abc"));
        assert!(has_fresh_ready_spare(&[spare("ready", Some("abc"))], "abc"));
        assert!(!has_fresh_ready_spare(
            &[spare("ready", Some("old"))],
            "abc"
        ));
        assert!(!has_fresh_ready_spare(&[spare("ready", None)], "abc"));
        assert!(!has_fresh_ready_spare(
            &[spare("claimed", Some("abc"))],
            "abc"
        ));
        assert!(has_fresh_ready_spare(
            &[
                spare("quarantined", Some("abc")),
                spare("ready", Some("abc"))
            ],
            "abc"
        ));
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
    fn claim_ledger_pick_warns_and_falls_back_on_a_read_error() {
        let mut warned: Vec<String> = Vec::new();
        let picked = claim_ledger_pick(
            Err(anyhow::anyhow!("database is locked")),
            "sprites",
            "/wt/a",
            |text| warned.push(text.to_string()),
        );

        assert!(picked.is_none());
        assert_eq!(warned.len(), 1);
        assert!(warned[0].contains("sprites"));
        assert!(warned[0].contains("/wt/a"));
        assert!(warned[0].contains("database is locked"));
        assert!(warned[0].contains("provisioning a fresh sandbox"));
    }

    #[test]
    fn claim_ledger_pick_passes_a_ready_spare_and_an_empty_pool_through_silently() {
        let mut warned: Vec<String> = Vec::new();
        let ready = claim_ledger_pick(
            Ok(Some(("spare-1".into(), Some("cp".into())))),
            "sprites",
            "/wt/a",
            |text| warned.push(text.to_string()),
        );
        assert_eq!(ready, Some(("spare-1".into(), Some("cp".into()))));
        assert!(warned.is_empty());

        let empty = claim_ledger_pick(Ok(None), "sprites", "/wt/a", |text| {
            warned.push(text.to_string())
        });
        assert_eq!(empty, None);
        assert!(warned.is_empty());
    }

    #[test]
    fn spare_ledger_unreadable_warning_names_the_env_worktree_and_cause() {
        let error = anyhow::anyhow!("no such table: pool_spares")
            .context("warm pool: cannot read ready spares for /r / sprites");
        let warning = spare_ledger_unreadable_warning("sprites", "/wt/a", &error);

        assert!(warning.contains("sprites"));
        assert!(warning.contains("/wt/a"));
        assert!(warning.contains("warm pool: cannot read ready spares"));
        assert!(warning.contains("no such table: pool_spares"));
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

    #[test]
    fn deferred_attach_refusal_only_blocks_pending_agents() {
        use thegn_core::config::NamedCommand;
        use thegn_core::provision_journal::{DeferredState, Journal};

        let mut cfg = thegn_core::config::Config::default();
        cfg.agents.push(NamedCommand {
            backends: Vec::new(),
            name: "codex".into(),
            command: "codex".into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
            resume: false,
            route_via_proxy: false,
            drawer_scope: None,
            drawer_cwd: None,
        });
        let mut pending = Journal::pending("/wt", "cloud");
        pending.step = "agents\ninstall".into();
        let mut failed = pending.clone();
        failed.state = DeferredState::Failed;
        assert!(deferred_attach_refusal(&cfg, "shell", "/wt", Some(&pending)).is_none());
        assert!(deferred_attach_refusal(&cfg, "tool", "/wt", Some(&pending)).is_none());
        assert!(deferred_attach_refusal(&cfg, "codex", "/wt", None).is_none());
        let refusal = deferred_attach_refusal(&cfg, "codex", "/wt", Some(&pending))
            .expect("pending agent is held");
        assert!(refusal.contains("thegn env up --worktree /wt"));
        assert!(refusal.contains("agentsinstall"));
        assert!(!refusal.contains('\n'));
        assert!(deferred_attach_refusal(&cfg, "codex", "/wt", Some(&failed)).is_none());
    }

    #[test]
    fn deferred_pending_env_is_none_for_a_local_worktree() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().to_string_lossy().into_owned();
        assert_eq!(
            deferred_pending_env(&thegn_core::config::Config::default(), &wt).unwrap(),
            None
        );
    }

    #[test]
    fn unprovisioned_halt_names_thegn_env_up_for_the_worktree() {
        let halt = unprovisioned_halt("sprites".into(), "/wt/app");
        let text = anyhow::Error::from(halt).to_string();
        assert!(text.contains("thegn env up --worktree /wt/app"), "{text}");
        assert!(text.contains("'sprites'"), "{text}");
        assert!(!text.contains("still provisioning"), "{text}");
    }

    #[test]
    fn unprovisioned_provider_env_is_none_for_a_local_worktree() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().to_string_lossy().into_owned();
        let result = unprovisioned_provider_env(&thegn_core::config::Config::default(), &wt);
        assert!(matches!(result, Ok(None)), "{result:?}");
    }

    #[test]
    fn provider_target_is_none_for_a_local_worktree() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().to_string_lossy().into_owned();
        let result = provider_target(&thegn_core::config::Config::default(), &wt);
        assert!(matches!(result, Ok(None)), "{result:?}");
    }

    #[test]
    fn claim_location_gate_table() {
        let cases = [
            (true, false, true, false),
            (false, true, false, true),
            (false, false, false, false),
            (true, false, false, true),
        ];
        for (stored_is_remote, provider_placed, live, expected) in cases {
            assert_eq!(
                claim_location_gate(stored_is_remote, provider_placed, live),
                expected,
                "{stored_is_remote}, {provider_placed}, {live}"
            );
        }
    }

    struct ScriptedClaimEgress {
        calls: std::cell::RefCell<Vec<String>>,
        applies: std::cell::RefCell<Vec<anyhow::Result<()>>>,
        reads: std::cell::RefCell<Vec<anyhow::Result<Vec<thegn_svc::provider::PolicyRule>>>>,
    }

    impl crate::egress_admit::EgressSteps for ScriptedClaimEgress {
        fn apply(&self, id: &str, _allow: &[String], _block: &[String]) -> anyhow::Result<()> {
            self.calls.borrow_mut().push(format!("apply:{id}"));
            self.applies.borrow_mut().remove(0)
        }

        fn read_back(&self, id: &str) -> anyhow::Result<Vec<thegn_svc::provider::PolicyRule>> {
            self.calls.borrow_mut().push(format!("read:{id}"));
            self.reads.borrow_mut().remove(0)
        }

        fn destroy(&self, id: &str) -> anyhow::Result<()> {
            self.calls.borrow_mut().push(format!("destroy:{id}"));
            Ok(())
        }
    }

    fn claim_egress_lists() -> (Vec<String>, Vec<String>) {
        (
            vec!["allowed.test".to_string()],
            vec!["blocked.test".to_string()],
        )
    }

    #[test]
    fn claim_egress_gate_matching_policy_is_verified_with_one_read() {
        let (allow, block) = claim_egress_lists();
        let fake = ScriptedClaimEgress {
            calls: std::cell::RefCell::new(Vec::new()),
            applies: std::cell::RefCell::new(Vec::new()),
            reads: std::cell::RefCell::new(vec![Ok(thegn_svc::provider::rules_from(
                &allow, &block,
            )
            .unwrap())]),
        };

        claim_egress_gate(Some((&fake, true)), "spare-1", &allow, &block).unwrap();

        assert_eq!(*fake.calls.borrow(), vec!["read:spare-1"]);
    }

    #[test]
    fn claim_egress_gate_reapplies_a_drifted_policy() {
        let (allow, block) = claim_egress_lists();
        let fake = ScriptedClaimEgress {
            calls: std::cell::RefCell::new(Vec::new()),
            applies: std::cell::RefCell::new(vec![Ok(())]),
            reads: std::cell::RefCell::new(vec![
                Ok(Vec::new()),
                Ok(thegn_svc::provider::rules_from(&allow, &block).unwrap()),
            ]),
        };

        claim_egress_gate(Some((&fake, true)), "spare-1", &allow, &block).unwrap();

        assert_eq!(
            *fake.calls.borrow(),
            vec!["read:spare-1", "apply:spare-1", "read:spare-1"]
        );
    }

    #[test]
    fn claim_egress_gate_refuses_an_unverifiable_policy_naming_the_spare() {
        let (allow, block) = claim_egress_lists();
        let fake = ScriptedClaimEgress {
            calls: std::cell::RefCell::new(Vec::new()),
            applies: std::cell::RefCell::new(vec![Err(anyhow::anyhow!("apply failed"))]),
            reads: std::cell::RefCell::new(vec![Ok(Vec::new())]),
        };

        let result = claim_egress_gate(Some((&fake, true)), "spare-1", &allow, &block);

        assert!(matches!(result, Err(reason)
            if reason.contains("spare-1")
                && reason.contains("could not be verified")
                && reason.contains("apply failed")));
        assert_eq!(*fake.calls.borrow(), vec!["read:spare-1", "apply:spare-1"]);
    }

    #[test]
    fn claim_egress_gate_without_declared_lists_makes_no_provider_call() {
        let fake = ScriptedClaimEgress {
            calls: std::cell::RefCell::new(Vec::new()),
            applies: std::cell::RefCell::new(Vec::new()),
            reads: std::cell::RefCell::new(Vec::new()),
        };

        claim_egress_gate(Some((&fake, true)), "spare-1", &[], &[]).unwrap();
        claim_egress_gate(None, "spare-1", &[], &[]).unwrap();

        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn claim_egress_gate_refuses_declared_lists_without_a_provider() {
        let (allow, block) = claim_egress_lists();

        let result = claim_egress_gate(None, "spare-1", &allow, &block);

        assert!(matches!(result, Err(reason)
            if reason.contains("spare-1") && reason.contains("no provider")));
    }

    #[test]
    fn claim_egress_gate_skips_a_provider_without_egress_translation() {
        let (allow, block) = claim_egress_lists();
        let fake = ScriptedClaimEgress {
            calls: std::cell::RefCell::new(Vec::new()),
            applies: std::cell::RefCell::new(Vec::new()),
            reads: std::cell::RefCell::new(Vec::new()),
        };

        claim_egress_gate(Some((&fake, false)), "spare-1", &allow, &block).unwrap();

        assert!(fake.calls.borrow().is_empty());
    }
}
