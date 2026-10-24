//! The hibernator: snapshot-then-destroy for claimed provider sandboxes whose
//! compute bills while it exists (commodity VPS; any env with `hibernate = on`).
//!
//! On the hydration cadence, [`tick`] gathers per-worktree candidates and asks
//! the pure `thegn_core::lifecycle::decide_hibernate`; at most one sandbox
//! hibernates per pass, on its own thread, under the same per-sandbox lock the
//! provisioner takes. The cycle (row states in `worktree_hibernations`):
//!
//! 1. `capturing` — intent row written, then the sandbox runs
//!    `syncstate::capture_script` (bundle of unpushed commits + uncommitted
//!    patch + untracked tar, with a size/sha trailer). The host downloads each
//!    artifact, re-hashes it, sanity-checks the bundle, and writes everything
//!    into the `[lifecycle.snapshot]` store — manifest last.
//! 2. `destroying` — snapshot verified; the instance may now die. On destroy
//!    success → `hibernated`. A crash in this window is healed by the sweep
//!    (destroy is idempotent — the provider treats 404 as already-gone).
//! 3. `hibernated` — compute gone, state durable. The next open of the
//!    worktree re-provisions and overlays the snapshot (`SnapshotRestore`).
//!
//! Capture is a PRIMARY path: any failure aborts the cycle, deletes the row,
//! and KEEPS the VM (with a warning + a retry backoff). The reverse — destroy
//! without a verified snapshot — never happens.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use thegn_core::config::Config;
use thegn_core::config_env_tables::SnapshotStoreConfig;
use thegn_core::lifecycle::{HibernateCandidate, decide_hibernate};
use thegn_core::remote::GitLoc;
use thegn_core::snapshot_meta::{
    ArtifactMeta, SnapshotKey, SnapshotManifest, retention_prune_keeping,
};
use thegn_core::store::{HibernationRow, HibernationStore, PoolStore};
use thegn_core::syncstate;
use thegn_svc::snapshot::SnapshotStore;

use crate::agent::block_on_provider;

const TICK_INTERVAL: Duration = Duration::from_secs(60);
/// Whole-capture exec budget (bundle+diff+tar on the sandbox).
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(600);
/// Per-artifact download budget.
const READ_TIMEOUT: Duration = Duration::from_secs(300);
/// After a failed cycle, leave the worktree alone this long before retrying.
const FAILURE_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// Sandbox-side file stem for the capture artifacts (`/tmp/tg-hib.*`).
const STEM: &str = "tg-hib";
/// Age before a crashed-`capturing` row is discarded / a `destroying` row is
/// re-driven by the healing sweep.
const HEAL_AFTER_SECS: u64 = 10 * 60;

/// Per-worktree failure backoff (in-memory: a restart retries, which is fine —
/// the failure will just repeat its warning if it persists).
fn failures() -> &'static Mutex<HashMap<String, Instant>> {
    static R: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One hibernation cycle at a time, process-wide.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Sidebar cache of worktrees currently in a hibernation cycle (any row
/// state). Refreshed on the hydration cadence; read at render time (a mutex
/// lookup — never DB I/O on the render path).
fn hibernated_cache() -> &'static Mutex<std::collections::BTreeSet<String>> {
    static R: std::sync::OnceLock<Mutex<std::collections::BTreeSet<String>>> =
        std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new(std::collections::BTreeSet::new()))
}

/// Refresh the render cache from the DB; returns the set for `SidebarStatus`
/// (whose equality diff drives the repaint when the set changes).
pub(crate) fn refresh_hibernated(db: &thegn_core::db::Db) -> std::collections::BTreeSet<String> {
    let set: std::collections::BTreeSet<String> = db
        .hibernations()
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.worktree_path)
        .collect();
    if let Ok(mut c) = hibernated_cache().lock() {
        *c = set.clone();
    }
    set
}

/// Whether `worktree` is mid-hibernation-cycle (render-time lookup).
pub(crate) fn is_hibernated(worktree: &str) -> bool {
    hibernated_cache()
        .lock()
        .map(|c| c.contains(worktree))
        .unwrap_or(false)
}

/// The snapshot key for one worktree: repo slug / worktree dir name / env.
fn snapshot_key(
    db: &thegn_core::db::Db,
    repo_root: &Path,
    worktree: &str,
    env: &str,
) -> anyhow::Result<SnapshotKey> {
    let wt_slug = Path::new(worktree)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| worktree.to_string());
    Ok(SnapshotKey {
        repo_slug: thegn_core::repo::repo_slug_with_checked(db, repo_root)?,
        worktree_slug: wt_slug,
        env: env.to_string(),
    })
}

/// Open the configured snapshot store with the host secret chain.
pub(crate) fn open_store(cfg: &SnapshotStoreConfig) -> anyhow::Result<Box<dyn SnapshotStore>> {
    thegn_svc::snapshot::open_store(cfg, &crate::secret::resolve_ref_for)
}

/// Throttled entry, called from the hydration thread next to the reapers.
/// Cheap when nothing is eligible; the actual cycle runs on its own thread.
pub fn tick(session: &crate::session::Session, cfg: &Config) {
    // A host with no hibernate-eligible env pays nothing here.
    if !cfg.env.values().any(|e| e.provider.hibernate_enabled()) {
        return;
    }
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|t| t.elapsed() < TICK_INTERVAL) {
            return;
        }
        *last = Some(Instant::now());
    }
    if IN_FLIGHT.load(Ordering::SeqCst) {
        return;
    }
    let Ok(db) = thegn_core::db::Db::open() else {
        return;
    };

    // Healing sweep for rows a crash left mid-cycle (cheap DB reads; any
    // provider work happens in the spawned cycle below).
    let now = thegn_core::util::now();
    let mut heal: Option<HibernationRow> = None;
    for row in db.hibernations().unwrap_or_default() {
        let Some(age) = thegn_core::time_policy::age_seconds(now, row.updated_at) else {
            tracing::warn!(target: "thegn::hibernate", worktree = %row.worktree_path, "quarantined hibernation state with invalid/future update time; reconcile state; no destructive recovery attempted");
            continue;
        };
        match row.state.as_str() {
            // Crashed mid-capture: the VM is alive and the snapshot never
            // verified — drop the row so a later pass recaptures fresh.
            "capturing" if age >= HEAL_AFTER_SECS => {
                tracing::warn!(
                    target: "thegn::hibernate",
                    worktree = %row.worktree_path,
                    "discarding stale mid-capture hibernation intent (crash?)"
                );
                let _ = db.delete_hibernation(&row.worktree_path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            }
            // Snapshot verified but the destroy never confirmed: finish it.
            "destroying" if age >= HEAL_AFTER_SECS => heal = Some(row),
            _ => {}
        }
    }
    if let Some(row) = heal {
        spawn_cycle(cfg.clone(), Cycle::FinishDestroy(row));
        return;
    }

    let states = thegn_core::activity::read_states();
    let active_path: Option<String> = session.active_group().map(|g| g.path.clone());
    let mut cands: Vec<HibernateCandidate> = Vec::new();
    let mut envs: HashMap<String, String> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for g in &session.worktrees {
        if g.path.is_empty() || !seen.insert(g.path.clone()) {
            continue;
        }
        let loc = GitLoc::for_worktree(Path::new(&g.path));
        if !loc.is_remote() {
            continue;
        }
        let env_name = match hibernate_selection(
            &g.path,
            crate::lifecycle::pool_context(&db, cfg, &g.path, &loc),
        ) {
            Some(env_name) => env_name,
            None => continue,
        };
        let Some(env) = cfg.env.get(&env_name) else {
            continue;
        };
        let enabled = env.provider.hibernate_enabled();
        let after = env
            .provider
            .hibernate_idle(cfg.lifecycle.hibernate_after_secs);
        let busy = states.get(&g.name).map(|s| s == "active").unwrap_or(false);
        let in_backoff = failures()
            .lock()
            .ok()
            .and_then(|f| f.get(&g.path).map(|t| t.elapsed() < FAILURE_BACKOFF))
            .unwrap_or(false);
        let Some(already_hibernated) = hibernated_state(&g.path, db.hibernation_for(&g.path))
        else {
            continue;
        };
        envs.insert(g.path.clone(), env_name);
        cands.push(HibernateCandidate {
            worktree: g.path.clone(),
            is_active: active_path.as_deref() == Some(g.path.as_str()),
            // No host-side pane registry reaches this thread; attached
            // interactive shells are caught by the sandbox-side `who`
            // preflight in the cycle instead.
            has_pane: false,
            busy,
            idle_secs: crate::lifecycle::idle_secs(&g.path).unwrap_or(0),
            hibernate_enabled: enabled && !in_backoff,
            after_secs: after,
            already_hibernated,
        });
    }
    if let Some(wt) = decide_hibernate(&cands).into_iter().next() {
        let env_name = envs.remove(&wt).unwrap_or_default();
        spawn_cycle(
            cfg.clone(),
            Cycle::Hibernate {
                worktree: wt,
                env_name,
            },
        );
    }
}

fn hibernated_state(worktree: &str, read: anyhow::Result<Option<HibernationRow>>) -> Option<bool> {
    match read {
        Ok(row) => Some(row.is_some()),
        Err(error) => {
            tracing::warn!(target: "thegn::lifecycle", worktree = %worktree, error = %format!("{error:#}"), "hibernation skipped: hibernation record could not be read");
            None
        }
    }
}

fn hibernate_selection(
    worktree: &str,
    context: anyhow::Result<(std::path::PathBuf, String, String)>,
) -> Option<String> {
    match context {
        Ok((_, _, env_name)) => Some(env_name),
        Err(error) => {
            tracing::warn!(target: "thegn::lifecycle", worktree = %worktree, error = %format!("{error:#}"), "hibernation skipped: environment selection could not be read");
            None
        }
    }
}

enum Cycle {
    Hibernate { worktree: String, env_name: String },
    FinishDestroy(HibernationRow),
}

fn spawn_cycle(cfg: Config, cycle: Cycle) {
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    thegn_core::thread_name::spawn_named("thegn-hibernate", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background); // Background: idle hibernation housekeeping.
        let done = || IN_FLIGHT.store(false, Ordering::SeqCst);
        match cycle {
            Cycle::Hibernate { worktree, env_name } => {
                if let Err(e) = hibernate_one(&cfg, &worktree, &env_name) {
                    thegn_core::msg::warn(&format!(
                        "hibernate {worktree}: {e}; the sandbox is kept (will retry later)"
                    ));
                    if let Ok(mut f) = failures().lock() {
                        f.insert(worktree.clone(), Instant::now());
                    }
                    // The intent row must not outlive a failed capture: the VM
                    // is alive and the snapshot is not trustworthy.
                    if let Ok(db) = thegn_core::db::Db::open()
                        && db
                            .hibernation_for(&worktree)
                            .ok()
                            .flatten()
                            .is_some_and(|r| r.state == "capturing")
                    {
                        let _ = db.delete_hibernation(&worktree); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                    }
                }
                done();
            }
            Cycle::FinishDestroy(row) => {
                finish_destroy(&cfg, &row);
                done();
            }
        }
    });
}

/// Complete a `destroying` row: the snapshot already verified, only the
/// instance teardown is outstanding. Destroy is idempotent (404 = gone).
fn finish_destroy(cfg: &Config, row: &HibernationRow) {
    let Some(env) = cfg.env.get(&row.env_name) else {
        return;
    };
    let Some(provider) = crate::agent::provider_for_named(&env.provider, &row.sandbox_name) else {
        return;
    };
    if let Err(error) = crate::remote_enqueue_auth::revoke_for_sandbox(
        &env.provider,
        &row.sandbox_name,
        Some(&row.worktree_path),
    ) {
        thegn_core::msg::warn(&format!(
            "hibernate: refusing to destroy {} because route-to-host credential revocation failed: {error:#}",
            row.sandbox_name
        ));
        return;
    }
    match block_on_provider(|| async { provider.destroy(&row.sandbox_name).await }) {
        Ok(()) => {
            if let Ok(db) = thegn_core::db::Db::open() {
                let _ = db.set_hibernation_state(&row.worktree_path, "hibernated", None); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                // best-effort: a claimed spare's pool row must not linger.
                let _ = db.delete_pool_spare(&row.sandbox_name);
            }
            match GitLoc::for_worktree_op(
                Path::new(&row.worktree_path),
                "hibernation bridge-key drop",
            ) {
                Ok(loc) => {
                    if let Some(key) = thegn_svc::bridge::bridge_key(&loc) {
                        thegn_svc::bridge::drop_key(&key);
                    }
                }
                Err(error) => thegn_core::msg::warn(&format!(
                    "{error}; the sandbox's bridge key was not dropped"
                )),
            }
            thegn_core::msg::info(&format!(
                "hibernated {}: compute destroyed, state snapshotted ({})",
                row.worktree_path, row.snapshot_id
            ));
        }
        Err(e) => thegn_core::msg::warn(&format!(
            "hibernate: destroy {} failed: {e}; will retry",
            row.sandbox_name
        )),
    }
}

/// One full hibernation cycle for `worktree`. Every error KEEPS the VM.
fn hibernate_one(cfg: &Config, worktree: &str, env_name: &str) -> anyhow::Result<()> {
    let env = cfg
        .env
        .get(env_name)
        .ok_or_else(|| anyhow::anyhow!("env {env_name} not configured"))?;
    let name = crate::agent::provider_sandbox_name(cfg, worktree, env_name)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no sandbox name resolved"))?;

    // Same lock the provisioner takes: never capture under a provision, and
    // never provision under a capture.
    let _guard = crate::provision_gate::sandbox_lock(&name);

    // Re-check the volatile gates now that we hold the lock (the decision ran
    // up to a minute ago).
    if crate::lifecycle::idle_secs(worktree).unwrap_or(0)
        < env
            .provider
            .hibernate_idle(cfg.lifecycle.hibernate_after_secs)
    {
        return Ok(()); // woke up in the meantime — not an error
    }

    let provider = crate::agent::provider_for_named(&env.provider, &name)
        .ok_or_else(|| anyhow::anyhow!("provider unavailable (token unset?)"))?;
    if !provider.caps().files {
        anyhow::bail!("provider has no file API; capture impossible");
    }

    // Sandbox-side pane guard: an attached interactive shell (an ssh pts
    // session on a VPS) means someone may be mid-something — skip quietly.
    // (`who` misses exotic attach paths; VPS panes are ssh and do show up.)
    let who = exec_capture(
        &provider,
        &name,
        "who 2>/dev/null | wc -l",
        Duration::from_secs(30),
    )?;
    if who.trim().lines().last().unwrap_or("0").trim() != "0" {
        return Ok(());
    }

    let db = thegn_core::db::Db::open()?;
    let loc =
        GitLoc::for_worktree_op(Path::new(worktree), "hibernate").map_err(anyhow::Error::msg)?;
    let (repo_root, _repo, _env) = crate::lifecycle::pool_context(&db, cfg, worktree, &loc)?;
    let key = snapshot_key(&db, &repo_root, worktree, env_name)?;
    let store = open_store(&cfg.lifecycle.snapshot)?;

    // Intent row BEFORE any capture work (the crash-healing anchor).
    db.put_hibernation(&HibernationRow {
        worktree_path: worktree.to_string(),
        repo_path: repo_root.to_string_lossy().into_owned(),
        env_name: env_name.to_string(),
        sandbox_name: name.clone(),
        snapshot_id: String::new(),
        head: String::new(),
        state: "capturing".into(),
        created_at: 0,
        updated_at: 0,
    })?;

    let workdir = crate::provider_workdir::resolve(&env.provider, &name);
    let manifest = capture_to_store(
        &provider,
        &name,
        &workdir,
        &key,
        store.as_ref(),
        &cfg.lifecycle.snapshot,
    )?;

    // Snapshot verified — the instance may now die.
    let mut row = db
        .hibernation_for(worktree)?
        .ok_or_else(|| anyhow::anyhow!("hibernation row vanished mid-cycle"))?;
    row.snapshot_id = manifest.id.clone();
    row.head = manifest.head.clone();
    row.state = "destroying".into();
    db.put_hibernation(&row)?;
    finish_destroy(cfg, &row);
    Ok(())
}

/// The snapshot a re-provision must overlay, from an already-performed record
/// read. An unreadable record is an error when hibernation is on for the env
/// (provisioning a fresh clone would hide the user's snapshot), and "nothing
/// to restore" (with a warning) when it is off.
fn restore_snapshot_id(
    worktree: &str,
    hibernation_enabled: bool,
    read: anyhow::Result<Option<HibernationRow>>,
) -> anyhow::Result<Option<String>> {
    match read {
        Ok(Some(row)) if row.state != "capturing" && !row.snapshot_id.is_empty() => {
            Ok(Some(row.snapshot_id))
        }
        Ok(_) => Ok(None),
        Err(error) if hibernation_enabled => Err(error.context(format!(
            "cannot read the hibernation record for {worktree}; refusing to provision a fresh sandbox that would hide its snapshot (retry once the state DB is readable)"
        ))),
        Err(error) => {
            tracing::warn!(target: "thegn::hibernate", worktree = %worktree, error = %format!("{error:#}"), "hibernation record unreadable; hibernation is off for this env, provisioning without a restore");
            Ok(None)
        }
    }
}

/// Read the snapshot id the provision plan should overlay. `capturing` rows
/// are not restorable (their snapshot never verified). An unreadable record
/// is an error while hibernation is enabled for this environment.
pub(crate) fn begin_restore(
    worktree: &str,
    hibernation_enabled: bool,
) -> anyhow::Result<Option<String>> {
    let db = thegn_core::db::Db::open();
    let read = match &db {
        Ok(db) => db.hibernation_for(worktree),
        Err(error) => Err(anyhow::anyhow!("open the state DB: {error:#}")),
    };
    restore_snapshot_id(worktree, hibernation_enabled, read)
}

/// Mark a worktree as restoring once its restore plan is actually being built.
/// This is a best-effort cache write; git/forge stays the source of truth.
pub(crate) fn mark_restoring(worktree: &str) {
    match thegn_core::db::Db::open() {
        Ok(db) => {
            if let Err(error) = db.set_hibernation_state(worktree, "restoring", None) {
                tracing::debug!(target: "thegn::hibernate", worktree = %worktree, error = %format!("{error:#}"), "could not mark hibernation record as restoring");
            }
        }
        Err(error) => {
            tracing::debug!(target: "thegn::hibernate", worktree = %worktree, error = %format!("{error:#}"), "could not open state DB to mark hibernation record as restoring");
        }
    }
}

/// Apply the `snapshot_restore` plan step: fetch the snapshot's artifacts from
/// the store, verify them against the manifest, upload them into the fresh
/// sandbox, and replay (fetch bundle + `reset --hard` + apply patch + untar).
/// NOT best-effort: on failure the row returns to `hibernated` so the next
/// open retries; the snapshot is retained either way (retention prunes it
/// naturally later). On success the row is deleted and — durability bonus —
/// the commit bundle is also fetched into the HOST worktree under
/// `refs/thegn/hibernate/<id>`, so the commits survive even a lost store.
pub(crate) fn apply_snapshot_restore(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    cfg: &Config,
    worktree: &str,
    workdir: &str,
    snapshot_id: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<()> {
    let db = thegn_core::db::Db::open()?;
    let res = restore_into_sandbox(
        provider,
        id,
        cfg,
        &db,
        worktree,
        workdir,
        snapshot_id,
        exec_env,
    );
    match &res {
        Ok(()) => {
            let _ = db.delete_hibernation(worktree); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            thegn_core::msg::info(&format!(
                "restored hibernated work into {worktree}'s fresh sandbox ({snapshot_id})"
            ));
        }
        Err(e) => {
            let _ = db.set_hibernation_state(worktree, "hibernated", None); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            thegn_core::msg::warn(&format!(
                "snapshot restore for {worktree} failed: {e}; will retry on next open"
            ));
        }
    }
    res
}

#[allow(clippy::too_many_arguments)]
fn restore_into_sandbox(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    cfg: &Config,
    db: &thegn_core::db::Db,
    worktree: &str,
    workdir: &str,
    snapshot_id: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<()> {
    let _ = exec_env; // replay runs under the login shell defaults, like parity
    let row = db
        .hibernation_for(worktree)?
        .ok_or_else(|| anyhow::anyhow!("no hibernation row for {worktree}"))?;
    let key = snapshot_key(db, Path::new(&row.repo_path), worktree, &row.env_name)?;
    let store = open_store(&cfg.lifecycle.snapshot)?;
    let legacy_key = key.legacy();
    let (restore_key, manifest) = match store.get_manifest(&key, snapshot_id) {
        Ok(manifest) => (&key, manifest),
        Err(primary) if legacy_key != key => match store.get_manifest(&legacy_key, snapshot_id) {
            Ok(manifest) => (&legacy_key, manifest),
            Err(legacy) => {
                return Err(anyhow::anyhow!(
                    "snapshot manifest lookup failed: {primary:#}; legacy key: {legacy:#}"
                ));
            }
        },
        Err(error) => return Err(error),
    };

    let mut bundle_data: Option<Vec<u8>> = None;
    for a in &manifest.artifacts {
        let data = store.get(restore_key, snapshot_id, &a.name)?;
        // Same integrity bar as capture: what the store returns must be what
        // the manifest published.
        if data.len() as u64 != a.bytes || !hex_sha256(&data).eq_ignore_ascii_case(&a.sha256) {
            anyhow::bail!("artifact {} corrupt in the snapshot store", a.name);
        }
        let path = syncstate::artifact_path(STEM, &a.name);
        block_on_provider(|| async {
            tokio::time::timeout(READ_TIMEOUT, provider.write(id, &path, &data))
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("upload of {path} timed out")))
        })?;
        if a.name == "bundle" {
            bundle_data = Some(data);
        }
    }

    let has_bundle = manifest.artifact("bundle").is_some();
    let script = syncstate::replay_script(
        workdir,
        STEM,
        (has_bundle && !manifest.head.is_empty()).then_some(manifest.head.as_str()),
        manifest.artifact("patch").is_some(),
        manifest.artifact("tar").is_some(),
        "snapshot restored",
    );
    exec_capture(provider, id, &script, CAPTURE_TIMEOUT)?;

    // The replay stages are individually non-fatal (`|| true`), so prove the
    // part that matters: when commits were carried, the sandbox HEAD must now
    // BE the captured head.
    if has_bundle && !manifest.head.is_empty() {
        let head = exec_capture(
            provider,
            id,
            &format!(
                "cd {} && git rev-parse HEAD",
                thegn_core::util::sh_quote(workdir)
            ),
            Duration::from_secs(30),
        )?;
        let got = head.trim().lines().last().unwrap_or("").trim().to_string();
        if got != manifest.head {
            anyhow::bail!(
                "replay did not land on the captured head (got {got}, want {})",
                manifest.head
            );
        }
    }

    // Durability bonus, best-effort: park the carried commits in the HOST
    // worktree under a hibernate ref (prerequisite commits may be absent
    // locally, in which case the fetch just fails quietly — the store copy
    // remains the source).
    if let Some(data) = bundle_data {
        backup_bundle_to_host(worktree, snapshot_id, &data);
    }
    Ok(())
}

// off-loop: provision worker thread only.
#[expect(clippy::disallowed_methods)]
fn backup_bundle_to_host(worktree: &str, snapshot_id: &str, data: &[u8]) {
    let Ok(mut tmp) = tempfile::Builder::new()
        .prefix("tg-hib-restore-")
        .suffix(".bundle")
        .tempfile()
    else {
        return;
    };
    if tmp.write_all(data).is_ok() {
        let refname = format!("HEAD:refs/thegn/hibernate/{snapshot_id}");
        // best-effort: the snapshot store keeps the authoritative copy.
        let _ = thegn_core::util::git_cmd(Path::new(worktree))
            .args(["fetch", "--quiet"])
            .arg(tmp.path())
            .arg(&refname)
            .output();
    }
}

/// Run `script` in the sandbox and return its combined output; non-zero exit
/// is an error.
fn exec_capture(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    script: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    // remote/sandbox target is Linux; POSIX sh is correct here
    let argv = vec!["/bin/sh".to_string(), "-lc".to_string(), script.to_string()];
    let (code, out) = block_on_provider(|| async {
        tokio::time::timeout(timeout, provider.run_exec(id, &argv, None, &[]))
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "sandbox exec timed out after {}s",
                    timeout.as_secs()
                ))
            })
    })?;
    if code != 0 {
        anyhow::bail!("sandbox exec exited {code}: {}", out.trim());
    }
    Ok(out)
}

/// Capture the worktree's state from the sandbox into the snapshot store.
/// Verifies sizes + sha256 across the transport hop and sanity-checks the git
/// bundle before anything is trusted; the manifest is written last. Returns
/// the published manifest.
fn capture_to_store(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    workdir: &str,
    key: &SnapshotKey,
    store: &dyn SnapshotStore,
    snap_cfg: &SnapshotStoreConfig,
) -> anyhow::Result<SnapshotManifest> {
    let out = exec_capture(
        provider,
        id,
        &syncstate::capture_script(workdir, STEM),
        CAPTURE_TIMEOUT,
    )?;
    let report = syncstate::parse_capture_output(&out)
        .map_err(|e| anyhow::anyhow!("capture report: {e}"))?;

    // Enforce the artifact ceiling BEFORE downloading anything.
    if let Some(max) = snap_cfg.max_artifact_bytes()
        && let Some(a) = report.artifacts.iter().find(|a| a.bytes > max)
    {
        anyhow::bail!(
            "artifact {} is {} MiB (max_artifact_mb = {}); not hibernating",
            a.name,
            a.bytes / (1024 * 1024),
            snap_cfg.max_artifact_mb
        );
    }

    let created_at = thegn_core::util::now();
    let head = report.head.as_deref();
    // Artifacts land under one random generation before its manifest publishes.
    let mut generation = [0_u8; 8];
    getrandom::fill(&mut generation)
        .map_err(|error| anyhow::anyhow!("generate snapshot generation: {error}"))?;
    let generation = generation_hex(generation);
    let id_probe = SnapshotManifest::new(head, &report.branch, created_at, Vec::new())
        .with_generation(&generation);

    let mut metas: Vec<ArtifactMeta> = Vec::new();
    for a in &report.artifacts {
        let path = syncstate::artifact_path(STEM, &a.name);
        let data = block_on_provider(|| async {
            tokio::time::timeout(READ_TIMEOUT, provider.read(id, &path))
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("download of {path} timed out")))
        })?;
        if data.len() as u64 != a.bytes {
            anyhow::bail!(
                "artifact {}: downloaded {} bytes, sandbox reported {}",
                a.name,
                data.len(),
                a.bytes
            );
        }
        let sha = hex_sha256(&data);
        if !sha.eq_ignore_ascii_case(&a.sha256) {
            anyhow::bail!("artifact {}: checksum mismatch across transport", a.name);
        }
        if a.name == "bundle" {
            verify_bundle_shape(&data)?;
        }
        store.put(key, &id_probe.id, &a.name, &data)?;
        metas.push(ArtifactMeta {
            name: a.name.clone(),
            bytes: a.bytes,
            sha256: sha,
        });
    }

    let manifest =
        SnapshotManifest::new(head, &report.branch, created_at, metas).with_generation(&generation);
    store.put_manifest(key, &manifest)?;

    if let Err(error) = verify_published(store, key, &manifest) {
        if let Err(cleanup) = store.delete(key, &manifest.id) {
            tracing::debug!(
                target: "thegn::hibernate",
                %cleanup,
                id = %manifest.id,
                "could not delete an unverified snapshot generation"
            );
        }
        return Err(error);
    }

    // Retention: prune older snapshots only after this one reads back intact.
    if let Ok(all) = store.list(key) {
        for stale in retention_prune_keeping(&all, snap_cfg.keep_clamped(), &manifest.id) {
            if stale == manifest.id {
                continue;
            }
            let _ = store.delete(key, &stale); // best-effort: GC only
        }
    }
    // best-effort: the sandbox is about to be destroyed anyway.
    let _ = exec_capture(
        provider,
        id,
        &format!("rm -f /tmp/{STEM}.bundle /tmp/{STEM}.patch /tmp/{STEM}.tar 2>/dev/null; true"),
        Duration::from_secs(30),
    );
    Ok(manifest)
}

fn hex_sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}"); // best-effort: String formatting cannot fail
    }
    s
}

fn generation_hex(bytes: [u8; 8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(16);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// Re-read a just-published generation from the store and prove it is what was captured: the
/// manifest reads back identical and every artifact has the recorded size and SHA-256. The
/// hibernator destroys compute only after this passes.
fn verify_published(
    store: &dyn SnapshotStore,
    key: &SnapshotKey,
    manifest: &SnapshotManifest,
) -> anyhow::Result<()> {
    let read = store.get_manifest(key, &manifest.id).map_err(|error| {
        anyhow::anyhow!(
            "snapshot {} read-back: manifest unreadable: {error:#}",
            manifest.id
        )
    })?;
    anyhow::ensure!(
        &read == manifest,
        "snapshot {} read-back: manifest differs from the one published",
        manifest.id
    );

    for artifact in &manifest.artifacts {
        let data = store
            .get(key, &manifest.id, &artifact.name)
            .map_err(|error| {
                anyhow::anyhow!(
                    "snapshot {} read-back: artifact {} unreadable: {error:#}",
                    manifest.id,
                    artifact.name
                )
            })?;
        anyhow::ensure!(
            data.len() as u64 == artifact.bytes
                && hex_sha256(&data).eq_ignore_ascii_case(&artifact.sha256),
            "snapshot {} read-back: artifact {} does not match its recorded size and sha256",
            manifest.id,
            artifact.name
        );
        drop(data);
    }

    Ok(())
}

/// Structural sanity for a downloaded bundle: `git bundle list-heads` reads
/// the header without needing a repository (a prerequisite check would need
/// the SANDBOX's remote tips, which the host may not have).
// off-loop: hibernator worker thread only.
fn verify_bundle_shape(data: &[u8]) -> anyhow::Result<()> {
    verify_bundle_shape_in(&std::env::temp_dir(), data)
}

fn verify_bundle_shape_in(scratch_root: &Path, data: &[u8]) -> anyhow::Result<()> {
    let scratch_root = scratch_root.canonicalize()?;
    let scratch = tempfile::Builder::new()
        .prefix("tg-hib-verify-")
        .tempdir_in(&scratch_root)?;
    let mut tmp = tempfile::Builder::new()
        .suffix(".bundle")
        .tempfile_in(scratch.path())?;
    tmp.write_all(data)?;

    // off-loop: hibernator worker thread only.
    #[expect(clippy::disallowed_methods)]
    let output = bundle_list_heads_command(scratch.path(), tmp.path(), &scratch_root).output();
    match output {
        Ok(output) if output.status.success() && !output.stdout.is_empty() => Ok(()),
        Ok(output) => anyhow::bail!(
            "captured bundle failed list-heads verification: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => anyhow::bail!("captured bundle failed list-heads verification: {error}"),
    }
}

fn bundle_list_heads_command(
    scratch: &Path,
    bundle: &Path,
    ceiling: &Path,
) -> std::process::Command {
    let mut cmd = thegn_core::util::git_cmd(scratch);
    cmd.env("GIT_CEILING_DIRECTORIES", ceiling)
        .args(["bundle", "list-heads"])
        .arg(bundle);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::store::WorkspaceStore;

    #[test]
    fn snapshot_key_uses_repo_slug_worktree_dirname_env() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        let k = snapshot_key(
            &db,
            Path::new("/home/u/code/myrepo"),
            "/home/u/wt/tg-fox",
            "hetzner",
        )
        .unwrap();
        assert_eq!(k.repo_slug, "myrepo");
        assert_eq!(k.worktree_slug, "tg-fox");
        assert_eq!(k.env, "hetzner");
        assert!(k.prefix().ends_with("/tg-fox/hetzner"));
    }

    #[test]
    fn snapshot_key_refuses_unresolved_slug() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("db.sqlite");
        let db = thegn_core::db::Db::open_at(&path).unwrap();
        assert_eq!(
            db.slug_for_repo("/elsewhere/same-name", "same-name")
                .unwrap(),
            "same-name"
        );
        let ro = thegn_core::db::Db::open_read_only_wal_at(&path)
            .unwrap()
            .expect("db file exists");

        assert!(
            snapshot_key(
                &ro,
                Path::new("/home/me/code/same-name"),
                "/wt/x",
                "hetzner",
            )
            .is_err()
        );
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn generation_hex_is_sixteen_lowercase_hex_digits() {
        let generation = generation_hex([0x00, 0x01, 0x0a, 0x10, 0x7f, 0x80, 0xfe, 0xff]);
        assert_eq!(generation, "00010a107f80feff");
        assert_eq!(generation.len(), 16);
        assert!(
            generation
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    #[test]
    fn verify_bundle_shape_does_not_follow_legacy_temp_path_symlink() {
        let temp_dir = std::env::temp_dir();
        let link = temp_dir.join(format!("tg-hib-verify-{}.bundle", std::process::id()));
        let mut victim = tempfile::NamedTempFile::new().unwrap();
        victim.write_all(b"keep this content").unwrap();
        victim.flush().unwrap();

        match std::fs::remove_file(&link) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("could not clear stale test link: {error}"),
        }

        match thegn_core::fsperm::create_symlink(victim.path(), &link, false) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                return;
            }
            Err(error) => panic!("could not plant test symlink: {error}"),
        }

        let result = verify_bundle_shape(b"not a bundle");
        let content = std::fs::read(victim.path()).unwrap();
        std::fs::remove_file(link).unwrap();
        assert!(result.is_err());
        assert_eq!(content, b"keep this content");
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture waits for its git child"
    )]
    fn git_output(command: &mut std::process::Command) -> std::process::Output {
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap()
    }

    #[test]
    fn verify_bundle_shape_in_ignores_a_broken_repository_above_its_scratch_dir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".git"),
            format!("gitdir: {}\n", root.path().join("missing-gitdir").display()),
        )
        .unwrap();

        let repo = tempfile::tempdir().unwrap();
        assert!(
            git_output(thegn_core::util::git_cmd(repo.path()).args(["init", "-q"]))
                .status
                .success()
        );
        assert!(
            git_output(thegn_core::util::git_cmd(repo.path()).args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ]))
            .status
            .success()
        );
        let bundle = repo.path().join("b.bundle");
        assert!(
            git_output(
                thegn_core::util::git_cmd(repo.path())
                    .args(["bundle", "create"])
                    .arg(&bundle)
                    .arg("--all")
            )
            .status
            .success()
        );
        let bytes = std::fs::read(bundle).unwrap();

        verify_bundle_shape_in(root.path(), &bytes).unwrap();
    }

    #[test]
    fn verify_bundle_shape_command_sets_a_ceiling_above_its_scratch_dir() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let scratch = root.join("s");
        let bundle = scratch.join("b.bundle");
        let command = bundle_list_heads_command(&scratch, &bundle, &root);

        assert!(command.get_envs().any(|(key, value)| {
            key == "GIT_CEILING_DIRECTORIES" && value == Some(root.as_os_str())
        }));
        let args: Vec<_> = command.get_args().collect();
        let git_cwd = args.iter().position(|arg| *arg == "-C").unwrap();
        assert_eq!(args.get(git_cwd + 1), Some(&scratch.as_os_str()));
    }

    #[test]
    fn verify_bundle_shape_in_rejects_garbage() {
        let root = tempfile::tempdir().unwrap();

        assert!(verify_bundle_shape_in(root.path(), b"not a bundle").is_err());
        assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    }

    #[derive(Clone, Copy)]
    enum Tamper {
        None,
        FlipByte,
        Truncate,
        DropArtifact,
        AlterManifest,
    }

    struct ReadbackStore {
        objects: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
        manifests: std::sync::Mutex<std::collections::HashMap<String, SnapshotManifest>>,
        tamper: Tamper,
    }

    impl ReadbackStore {
        fn new(tamper: Tamper) -> Self {
            Self {
                objects: std::sync::Mutex::new(std::collections::HashMap::new()),
                manifests: std::sync::Mutex::new(std::collections::HashMap::new()),
                tamper,
            }
        }

        fn object_key(id: &str, name: &str) -> String {
            format!("{id}/{name}")
        }
    }

    impl SnapshotStore for ReadbackStore {
        fn put(&self, _key: &SnapshotKey, id: &str, name: &str, data: &[u8]) -> anyhow::Result<()> {
            self.objects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(Self::object_key(id, name), data.to_vec());
            Ok(())
        }

        fn get(&self, _key: &SnapshotKey, id: &str, name: &str) -> anyhow::Result<Vec<u8>> {
            let mut data = self
                .objects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&Self::object_key(id, name))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("artifact {name} is missing from the fake store"))?;
            match self.tamper {
                Tamper::None | Tamper::AlterManifest => {}
                Tamper::FlipByte => {
                    if let Some(byte) = data.first_mut() {
                        *byte ^= 0xff;
                    }
                }
                Tamper::Truncate => {
                    data.pop();
                }
                Tamper::DropArtifact => {
                    anyhow::bail!("artifact {name} is missing from the fake store");
                }
            }
            Ok(data)
        }

        fn put_manifest(
            &self,
            _key: &SnapshotKey,
            manifest: &SnapshotManifest,
        ) -> anyhow::Result<()> {
            self.manifests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(manifest.id.clone(), manifest.clone());
            Ok(())
        }

        fn get_manifest(&self, _key: &SnapshotKey, id: &str) -> anyhow::Result<SnapshotManifest> {
            let mut manifest = self
                .manifests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("manifest {id} is missing from the fake store"))?;
            if matches!(self.tamper, Tamper::AlterManifest) {
                manifest.branch.push_str("-altered");
            }
            Ok(manifest)
        }

        fn list(&self, _key: &SnapshotKey) -> anyhow::Result<Vec<SnapshotManifest>> {
            Ok(self
                .manifests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .cloned()
                .collect())
        }

        fn delete(&self, _key: &SnapshotKey, id: &str) -> anyhow::Result<()> {
            self.manifests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
            self.objects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|object, _| !object.starts_with(&format!("{id}/")));
            Ok(())
        }
    }

    fn readback_fixture(tamper: Tamper) -> (ReadbackStore, SnapshotKey, SnapshotManifest) {
        let key = SnapshotKey {
            repo_slug: "repo".into(),
            worktree_slug: "worktree".into(),
            env: "env".into(),
        };
        let store = ReadbackStore::new(tamper);
        let artifacts = [
            ("bundle", b"bundle-bytes".as_slice()),
            ("patch", b"patch-bytes"),
        ];
        let mut metas = Vec::new();
        for (name, data) in artifacts {
            store
                .put(
                    &key,
                    "00000000000000000001-abc-0011223344556677",
                    name,
                    data,
                )
                .unwrap();
            metas.push(ArtifactMeta {
                name: name.into(),
                bytes: data.len() as u64,
                sha256: hex_sha256(data),
            });
        }
        let manifest = SnapshotManifest::new(Some("abc"), "main", 1, metas)
            .with_generation("0011223344556677");
        store.put_manifest(&key, &manifest).unwrap();
        (store, key, manifest)
    }

    #[test]
    fn published_snapshot_readback_accepts_an_intact_generation() {
        let (store, key, manifest) = readback_fixture(Tamper::None);
        assert!(verify_published(&store, &key, &manifest).is_ok());
    }

    #[test]
    fn published_snapshot_readback_refuses_a_corrupted_artifact() {
        let (store, key, manifest) = readback_fixture(Tamper::FlipByte);
        let error = verify_published(&store, &key, &manifest).unwrap_err();
        assert!(error.to_string().contains("bundle"));
        assert!(error.to_string().contains("read-back"));
    }

    #[test]
    fn published_snapshot_readback_refuses_a_truncated_artifact() {
        let (store, key, manifest) = readback_fixture(Tamper::Truncate);
        let error = verify_published(&store, &key, &manifest).unwrap_err();
        assert!(error.to_string().contains("bundle"));
        assert!(error.to_string().contains("read-back"));
    }

    #[test]
    fn published_snapshot_readback_refuses_a_missing_artifact() {
        let (store, key, manifest) = readback_fixture(Tamper::DropArtifact);
        let error = verify_published(&store, &key, &manifest).unwrap_err();
        assert!(error.to_string().contains("bundle"));
        assert!(error.to_string().contains("missing from the fake store"));
    }

    #[test]
    fn published_snapshot_readback_refuses_a_manifest_that_reads_back_differently() {
        let (store, key, manifest) = readback_fixture(Tamper::AlterManifest);
        let error = verify_published(&store, &key, &manifest).unwrap_err();
        assert!(error.to_string().contains("manifest differs"));
        assert!(error.to_string().contains("read-back"));
    }

    #[test]
    fn hibernate_selection_skips_an_unreadable_selection() {
        assert!(hibernate_selection("/wt", Err(anyhow::anyhow!("locked"))).is_none());
    }

    #[test]
    fn hibernate_selection_passes_a_read_through() {
        assert_eq!(
            hibernate_selection(
                "/wt",
                Ok((std::path::PathBuf::new(), "repo".into(), "picked".into()))
            ),
            Some("picked".into())
        );
    }

    fn row(state: &str, snapshot_id: &str) -> HibernationRow {
        HibernationRow {
            worktree_path: "/wt".into(),
            repo_path: "/repo".into(),
            env_name: "e".into(),
            sandbox_name: "s".into(),
            snapshot_id: snapshot_id.into(),
            head: String::new(),
            state: state.into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn restore_snapshot_id_refuses_an_unreadable_record_when_hibernation_is_on() {
        let result = restore_snapshot_id("/wt", true, Err(anyhow::anyhow!("database is locked")));
        let Err(error) = result else {
            panic!("unreadable hibernation record must refuse restore");
        };
        let message = format!("{error:#}");
        assert!(message.contains("cannot read the hibernation record for /wt"));
        assert!(message.contains("database is locked"));
    }

    #[test]
    fn restore_snapshot_id_skips_an_unreadable_record_when_hibernation_is_off() {
        assert_eq!(
            restore_snapshot_id("/wt", false, Err(anyhow::anyhow!("database is locked"))).unwrap(),
            None
        );
    }

    #[test]
    fn restore_snapshot_id_returns_the_hibernated_snapshot() {
        for state in ["hibernated", "restoring"] {
            for enabled in [true, false] {
                assert_eq!(
                    restore_snapshot_id("/wt", enabled, Ok(Some(row(state, "0009-abcd")))).unwrap(),
                    Some("0009-abcd".into())
                );
            }
        }
    }

    #[test]
    fn restore_snapshot_id_ignores_capturing_and_empty_rows() {
        assert_eq!(
            restore_snapshot_id("/wt", true, Ok(Some(row("capturing", "0009-abcd")))).unwrap(),
            None
        );
        assert_eq!(
            restore_snapshot_id("/wt", true, Ok(Some(row("hibernated", "")))).unwrap(),
            None
        );
    }

    #[test]
    fn restore_snapshot_id_is_none_without_a_record() {
        for enabled in [true, false] {
            assert_eq!(restore_snapshot_id("/wt", enabled, Ok(None)).unwrap(), None);
        }
    }

    #[test]
    fn hibernated_state_skips_an_unreadable_record() {
        assert_eq!(
            hibernated_state("/wt", Err(anyhow::anyhow!("locked"))),
            None
        );
    }

    #[test]
    fn hibernated_state_reports_whether_a_row_exists() {
        assert_eq!(hibernated_state("/wt", Ok(None)), Some(false));
        assert_eq!(
            hibernated_state("/wt", Ok(Some(row("hibernated", "x")))),
            Some(true)
        );
    }
}
