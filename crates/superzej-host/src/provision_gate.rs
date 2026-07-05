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

use superzej_core::store::{PoolStore, WorkspaceStore};
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

/// A stable-ish generic name for a new warm-pool spare: `<repo>-pool-<hash>`. The
/// hash varies per process+counter so concurrent mints never collide.
pub(crate) fn mint_spare_name(repo: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let slug = std::path::Path::new(repo)
        .file_name()
        .and_then(|s| s.to_str())
        .map(superzej_core::util::slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sz".to_string());
    let h = superzej_core::util::short_hash(&format!("{repo}-{}-{n}", std::process::id()), 6);
    format!("{slug}-pool-{h}")
}

/// Whether `env_name` may host (or hand over) warm-pool spares at all: only a
/// CONFIGURED provider env qualifies. The implicit `"default"` env — or any name
/// without an `[env.<name>]` table — has no provider to create/hand-over
/// sandboxes with, so "provisioning" a spare for it succeeds instantly as a
/// no-op, minting a phantom `ready` spare; claiming that phantom then skips the
/// worktree's real provisioning and the pane lands in a bare sandbox (the
/// bare-sprite-shell incident of 2026-07-02).
pub(crate) fn poolable_env(cfg: &superzej_core::config::Config, env_name: &str) -> bool {
    cfg.env
        .get(env_name)
        .is_some_and(|e| matches!(e.placement, superzej_core::config::PlacementMode::Provider))
}

/// Hash of the repo's `flake.lock` (staleness key for a spare's seeded devShell);
/// empty when the repo has no lockfile.
pub(crate) fn flake_lock_hash(repo_root: &std::path::Path) -> String {
    std::fs::read(repo_root.join("flake.lock"))
        .ok()
        .map(|b| superzej_core::util::short_hash(&String::from_utf8_lossy(&b), 16))
        .unwrap_or_default()
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
pub(crate) fn try_claim_spare(cfg: &superzej_core::config::Config, worktree: &str) -> bool {
    let loc = superzej_core::remote::GitLoc::for_worktree(std::path::Path::new(worktree));
    if !loc.is_remote() || worktree_live(worktree) {
        return false;
    }
    let db = superzej_core::db::Db::open().ok();
    let repo_root = db
        .as_ref()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| superzej_core::repo::main_worktree(std::path::Path::new(worktree)))
        .unwrap_or_else(|| std::path::PathBuf::from(worktree));
    // The worktree's EFFECTIVE env (its DB selection), exactly as the exec/
    // provision paths resolve it — claiming under the ambient default while the
    // worktree runs a picked env hands over a spare from the wrong pool.
    let selected = db
        .as_ref()
        .and_then(|db| db.effective_env(worktree, &repo_root.to_string_lossy()));
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
    // tab reopen) must keep using it, not stack a second bind.
    if db
        .as_ref()
        .and_then(|db| db.worktree_provider_sandbox(worktree).ok().flatten())
        .is_some()
    {
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
    let branch = superzej_core::util::git_cmd(std::path::Path::new(worktree))
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|b| !b.is_empty() && b != "HEAD");
    claim_spare(cfg, worktree, &repo_root, &env_name, branch.as_deref()).is_some()
}

// ---------------------------------------------------------------------------
// Warm-pool spare lifecycle (mint / claim / destroy) — the flows the guards
// above arbitrate. Extracted from the pinned `agent.rs`.

/// Mint + fully provision a NEW warm-pool spare for `(repo, env)`: a generically-
/// named sandbox, cloned + devShell-seeded + tooled + checkpointed (so it suspends
/// for free), recorded `ready` so a worktree can claim it. Returns the spare name.
/// On failure the half-built sandbox + DB row are torn down.
pub fn provision_spare(
    cfg: &superzej_core::config::Config,
    repo_root: &std::path::Path,
    env_name: &str,
    mut progress: impl FnMut(&[crate::agent::ProvisionStepView]),
) -> anyhow::Result<String> {
    let repo = repo_root.to_string_lossy().to_string();
    let name = mint_spare_name(&repo);
    if let Ok(db) = superzej_core::db::Db::open() {
        let _ = db.insert_pool_spare(&name, &repo, env_name);
    }
    // The repo's main worktree gives env-resolution + origin context; the name is
    // overridden to the generic spare name (the clone is branch-less either way).
    let ctx = superzej_core::repo::main_worktree(repo_root)
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
            if let Ok(db) = superzej_core::db::Db::open() {
                let _ = db.set_pool_spare_ready(&name, checkpoint.as_deref(), &lock);
            }
            Ok(name)
        }
        Ok((false, _)) => {
            let _ = destroy_spare(cfg, env_name, &name);
            Err(anyhow::anyhow!(
                "env '{env_name}' cannot host a pool spare (no configured provider)"
            ))
        }
        Err(e) => {
            let _ = destroy_spare(cfg, env_name, &name);
            Err(e)
        }
    }
}

/// Claim a `ready` spare for `(repo, env)` and hand it to `worktree`: bind it (DB,
/// atomic), check out the worktree's branch in the spare's workdir, mirror local
/// state, and REBIND the persisted remote location so chrome git/fs reads and CLI
/// panes route into the spare. Returns the claimed sandbox name, or `None` when no
/// spare is ready or the env can't hand one over (caller provisions fresh).
pub fn claim_spare(
    cfg: &superzej_core::config::Config,
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
    let db = superzej_core::db::Db::open().ok()?;
    let (name, checkpoint) = db.claim_pool_spare(&repo, env_name, worktree).ok()??;
    // The claimed spare's provisioned-base checkpoint rides its pool row; the
    // recycle paths (stale reconcile / worktree delete) restore from it.
    tracing::debug!(
        target: "szhost::lifecycle",
        %name,
        checkpoint = checkpoint.as_deref().unwrap_or("-"),
        "claimed spare checkpoint"
    );
    let workdir = env.provider.sync_workdir();
    // Per-worktree work: settle the branch in the spare's existing clone (the
    // sandbox auto-resumes when the exec opens). Best-effort — the bind already
    // succeeded, so the pane opens against the spare regardless.
    if let Some(provider) = crate::agent::provider_for_named(&env.provider, &name) {
        if let Some(b) = branch.map(str::trim).filter(|b| !b.is_empty()) {
            let wd = superzej_core::util::sh_quote(&workdir);
            let bq = superzej_core::util::sh_quote(b);
            let script = format!(
                "cd {wd} 2>/dev/null && (git checkout {bq} 2>/dev/null || git checkout -b {bq}) 2>&1"
            );
            let argv = vec!["/bin/sh".to_string(), "-lc".to_string(), script];
            let _ = crate::agent::block_on_provider(|| async {
                provider.run_exec(&name, &argv, None, &[]).await
            });
        }
        // Bring the claimed spare to full parity with the local worktree, same as
        // a fresh provision (only for an `in_env` provider — a projected data mode
        // mirrors the tree by other means). Best-effort.
        if env.data == superzej_core::config::DataMode::InEnv
            && let Err(e) =
                crate::agent::apply_local_parity(&provider, &name, worktree, &workdir, &[])
        {
            superzej_core::msg::warn(&format!(
                "local parity on claimed spare {name}: {e}; using the origin checkout."
            ));
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
            let marker = superzej_core::envplan::EnvPlan::marker_path(&workdir);
            let exists = crate::agent::block_on_provider(|| async { dp.list().await })
                .map(|names| names.iter().any(|n| n == &d))
                .unwrap_or(false);
            let bare = exists
                && crate::agent::block_on_provider(|| async { dp.read(&d, &marker).await })
                    .is_err();
            if bare {
                let _ = crate::agent::block_on_provider(|| async { dp.destroy(&d).await });
            }
        }
    }
    // Rebind the persisted remote location: the stored control prefix embeds the
    // DERIVED sandbox id, so without this every chrome git/fs read and CLI pane
    // keeps routing into the abandoned husk instead of the claimed spare.
    // `control_command_template` (not raw `exec_command`) so VPS envs — whose
    // prefix is the implicit `szhost vps-ssh` self-bridge — rebind too.
    let prefix: Vec<String> = env
        .provider
        .control_command_template()
        .iter()
        .map(|s| s.replace("{id}", &name))
        .collect();
    if !prefix.is_empty()
        && let Ok(rows) = db.worktrees()
        && let Some(row) = rows.into_iter().find(|r| r.worktree == worktree)
    {
        let loc = superzej_core::remote::GitLoc::provider_db_string(&prefix, &workdir);
        let _ = db.put_worktree(
            &row.tab_name,
            &row.repo_root,
            worktree,
            &row.branch,
            Some(&loc),
            None,
        );
    }
    superzej_core::msg::info(&format!("claimed warm spare {name} for {worktree}"));
    // The claim consumed this spare — refill the pool now rather than waiting for
    // the next ~8s maintainer tick to notice the gap (off-loop, its own thread).
    crate::lifecycle::refill_pool_after_claim(cfg, worktree);
    Some(name)
}

/// Destroy a spare sandbox + drop its DB row. Best-effort (idempotent).
pub fn destroy_spare(
    cfg: &superzej_core::config::Config,
    env_name: &str,
    name: &str,
) -> anyhow::Result<()> {
    if let Some(env) = cfg.env.get(env_name)
        && let Some(provider) = crate::agent::provider_for_named(&env.provider, name)
    {
        let _ = crate::agent::block_on_provider(|| async { provider.destroy(name).await });
    }
    if let Ok(db) = superzej_core::db::Db::open() {
        let _ = db.delete_pool_spare(name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

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
        use superzej_core::config::{Config, EnvConfig, PlacementMode};
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
        use superzej_core::config::{Config, EnvConfig, EnvProviderConfig, PlacementMode};
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
                    api_key_env: "SZ_TEST_NO_SUCH_TOKEN_VAR".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert!(claim_spare(&cfg, "/no/such/wt", repo, "sprites", None).is_none());
    }

    #[test]
    fn mint_spare_name_is_repo_slugged_and_unique() {
        let a = mint_spare_name("/home/u/code/superzej");
        let b = mint_spare_name("/home/u/code/superzej");
        assert!(a.starts_with("superzej-pool-"), "{a}");
        assert_ne!(a, b, "concurrent mints must never collide");
    }

    #[test]
    fn flake_lock_hash_empty_without_lockfile() {
        let dir = std::env::temp_dir().join("sz-gate-no-flake");
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(flake_lock_hash(&dir), "");
    }
}
