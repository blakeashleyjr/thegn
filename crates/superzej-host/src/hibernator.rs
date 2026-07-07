//! The hibernator: snapshot-then-destroy for claimed provider sandboxes whose
//! compute bills while it exists (commodity VPS; any env with `hibernate = on`).
//!
//! On the hydration cadence, [`tick`] gathers per-worktree candidates and asks
//! the pure `superzej_core::lifecycle::decide_hibernate`; at most one sandbox
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
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use superzej_core::config::Config;
use superzej_core::config_env_tables::SnapshotStoreConfig;
use superzej_core::lifecycle::{HibernateCandidate, decide_hibernate};
use superzej_core::remote::GitLoc;
use superzej_core::snapshot_meta::{ArtifactMeta, SnapshotKey, SnapshotManifest, retention_prune};
use superzej_core::store::{HibernationRow, HibernationStore, PoolStore};
use superzej_core::syncstate;
use superzej_svc::snapshot::SnapshotStore;

use crate::agent::block_on_provider;

const TICK_INTERVAL: Duration = Duration::from_secs(60);
/// Whole-capture exec budget (bundle+diff+tar on the sandbox).
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(600);
/// Per-artifact download budget.
const READ_TIMEOUT: Duration = Duration::from_secs(300);
/// After a failed cycle, leave the worktree alone this long before retrying.
const FAILURE_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// Sandbox-side file stem for the capture artifacts (`/tmp/sz-hib.*`).
const STEM: &str = "sz-hib";
/// Age before a crashed-`capturing` row is discarded / a `destroying` row is
/// re-driven by the healing sweep.
const HEAL_AFTER_SECS: i64 = 10 * 60;

/// Per-worktree failure backoff (in-memory: a restart retries, which is fine —
/// the failure will just repeat its warning if it persists).
fn failures() -> &'static Mutex<HashMap<String, Instant>> {
    static R: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One hibernation cycle at a time, process-wide.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// The snapshot key for one worktree: repo slug / worktree dir name / env.
fn snapshot_key(repo_root: &Path, worktree: &str, env: &str) -> SnapshotKey {
    let wt_slug = Path::new(worktree)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| worktree.to_string());
    SnapshotKey {
        repo_slug: superzej_core::repo::repo_slug(repo_root),
        worktree_slug: wt_slug,
        env: env.to_string(),
    }
}

/// Open the configured snapshot store with the host secret chain.
pub(crate) fn open_store(cfg: &SnapshotStoreConfig) -> anyhow::Result<Box<dyn SnapshotStore>> {
    superzej_svc::snapshot::open_store(cfg, &|r| crate::secret::resolve(r))
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
    let Ok(db) = superzej_core::db::Db::open() else {
        return;
    };

    // Healing sweep for rows a crash left mid-cycle (cheap DB reads; any
    // provider work happens in the spawned cycle below).
    let now = superzej_core::util::now();
    let mut heal: Option<HibernationRow> = None;
    for row in db.hibernations().unwrap_or_default() {
        let age = now - row.updated_at;
        match row.state.as_str() {
            // Crashed mid-capture: the VM is alive and the snapshot never
            // verified — drop the row so a later pass recaptures fresh.
            "capturing" if age >= HEAL_AFTER_SECS => {
                tracing::warn!(
                    target: "szhost::hibernate",
                    worktree = %row.worktree_path,
                    "discarding stale mid-capture hibernation intent (crash?)"
                );
                let _ = db.delete_hibernation(&row.worktree_path);
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

    let states = superzej_core::activity::read_states();
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
        let (_root, _repo, env_name) = crate::lifecycle::pool_context(&db, cfg, &g.path, &loc);
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
            already_hibernated: db.hibernation_for(&g.path).ok().flatten().is_some(),
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

enum Cycle {
    Hibernate { worktree: String, env_name: String },
    FinishDestroy(HibernationRow),
}

fn spawn_cycle(cfg: Config, cycle: Cycle) {
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let done = || IN_FLIGHT.store(false, Ordering::SeqCst);
        match cycle {
            Cycle::Hibernate { worktree, env_name } => {
                if let Err(e) = hibernate_one(&cfg, &worktree, &env_name) {
                    superzej_core::msg::warn(&format!(
                        "hibernate {worktree}: {e}; the sandbox is kept (will retry later)"
                    ));
                    if let Ok(mut f) = failures().lock() {
                        f.insert(worktree.clone(), Instant::now());
                    }
                    // The intent row must not outlive a failed capture: the VM
                    // is alive and the snapshot is not trustworthy.
                    if let Ok(db) = superzej_core::db::Db::open()
                        && db
                            .hibernation_for(&worktree)
                            .ok()
                            .flatten()
                            .is_some_and(|r| r.state == "capturing")
                    {
                        let _ = db.delete_hibernation(&worktree);
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
    match block_on_provider(|| async { provider.destroy(&row.sandbox_name).await }) {
        Ok(()) => {
            if let Ok(db) = superzej_core::db::Db::open() {
                let _ = db.set_hibernation_state(&row.worktree_path, "hibernated", None);
                // best-effort: a claimed spare's pool row must not linger.
                let _ = db.delete_pool_spare(&row.sandbox_name);
            }
            let loc = GitLoc::for_worktree(Path::new(&row.worktree_path));
            if let Some(key) = superzej_svc::bridge::bridge_key(&loc) {
                superzej_svc::bridge::drop_key(&key);
            }
            superzej_core::msg::info(&format!(
                "hibernated {}: compute destroyed, state snapshotted ({})",
                row.worktree_path, row.snapshot_id
            ));
        }
        Err(e) => superzej_core::msg::warn(&format!(
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

    let db = superzej_core::db::Db::open()?;
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let (repo_root, _repo, _env) = crate::lifecycle::pool_context(&db, cfg, worktree, &loc);
    let key = snapshot_key(&repo_root, worktree, env_name);
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

    let workdir = env.provider.sync_workdir();
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

/// If `worktree` has a restorable hibernation row, flip it to `restoring` and
/// return the snapshot id the provision plan should overlay. `capturing`
/// rows are not restorable (their snapshot never verified).
pub(crate) fn begin_restore(worktree: &str) -> Option<String> {
    let db = superzej_core::db::Db::open().ok()?;
    let row = db.hibernation_for(worktree).ok().flatten()?;
    if row.state == "capturing" || row.snapshot_id.is_empty() {
        return None;
    }
    let _ = db.set_hibernation_state(worktree, "restoring", None);
    Some(row.snapshot_id)
}

/// Apply the `snapshot_restore` plan step: fetch the snapshot's artifacts from
/// the store, verify them against the manifest, upload them into the fresh
/// sandbox, and replay (fetch bundle + `reset --hard` + apply patch + untar).
/// NOT best-effort: on failure the row returns to `hibernated` so the next
/// open retries; the snapshot is retained either way (retention prunes it
/// naturally later). On success the row is deleted and — durability bonus —
/// the commit bundle is also fetched into the HOST worktree under
/// `refs/superzej/hibernate/<id>`, so the commits survive even a lost store.
pub(crate) fn apply_snapshot_restore(
    provider: &superzej_svc::provider::Provider,
    id: &str,
    cfg: &Config,
    worktree: &str,
    workdir: &str,
    snapshot_id: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<()> {
    let db = superzej_core::db::Db::open()?;
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
            let _ = db.delete_hibernation(worktree);
            superzej_core::msg::info(&format!(
                "restored hibernated work into {worktree}'s fresh sandbox ({snapshot_id})"
            ));
        }
        Err(e) => {
            let _ = db.set_hibernation_state(worktree, "hibernated", None);
            superzej_core::msg::warn(&format!(
                "snapshot restore for {worktree} failed: {e}; will retry on next open"
            ));
        }
    }
    res
}

#[allow(clippy::too_many_arguments)]
fn restore_into_sandbox(
    provider: &superzej_svc::provider::Provider,
    id: &str,
    cfg: &Config,
    db: &superzej_core::db::Db,
    worktree: &str,
    workdir: &str,
    snapshot_id: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<()> {
    let _ = exec_env; // replay runs under the login shell defaults, like parity
    let row = db
        .hibernation_for(worktree)?
        .ok_or_else(|| anyhow::anyhow!("no hibernation row for {worktree}"))?;
    let key = snapshot_key(Path::new(&row.repo_path), worktree, &row.env_name);
    let store = open_store(&cfg.lifecycle.snapshot)?;
    let manifest = store.get_manifest(&key, snapshot_id)?;

    let mut bundle_data: Option<Vec<u8>> = None;
    for a in &manifest.artifacts {
        let data = store.get(&key, snapshot_id, &a.name)?;
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
                superzej_core::util::sh_quote(workdir)
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
    let tmp = std::env::temp_dir().join(format!("sz-hib-restore-{}.bundle", std::process::id()));
    if std::fs::write(&tmp, data).is_ok() {
        let refname = format!("HEAD:refs/superzej/hibernate/{snapshot_id}");
        // best-effort: the snapshot store keeps the authoritative copy.
        let _ = superzej_core::util::git_cmd(Path::new(worktree))
            .args(["fetch", "--quiet"])
            .arg(&tmp)
            .arg(&refname)
            .output();
    }
    let _ = std::fs::remove_file(&tmp);
}

/// Run `script` in the sandbox and return its combined output; non-zero exit
/// is an error.
fn exec_capture(
    provider: &superzej_svc::provider::Provider,
    id: &str,
    script: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
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
    provider: &superzej_svc::provider::Provider,
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

    let created_at = superzej_core::util::now();
    let head = report.head.as_deref();
    // The id is a pure function of (created_at, head): compute it up front so
    // artifacts land under it before the manifest publishes them.
    let id_probe = SnapshotManifest::new(head, &report.branch, created_at, Vec::new());

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

    let manifest = SnapshotManifest::new(head, &report.branch, created_at, metas);
    store.put_manifest(key, &manifest)?;

    // Retention: prune older snapshots now that this one is published.
    if let Ok(all) = store.list(key) {
        for stale in retention_prune(&all, snap_cfg.keep_clamped()) {
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
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Structural sanity for a downloaded bundle: `git bundle list-heads` reads
/// the header without needing a repository (a prerequisite check would need
/// the SANDBOX's remote tips, which the host may not have).
// off-loop: hibernator worker thread only.
#[expect(clippy::disallowed_methods)]
fn verify_bundle_shape(data: &[u8]) -> anyhow::Result<()> {
    let tmp = std::env::temp_dir().join(format!("sz-hib-verify-{}.bundle", std::process::id()));
    std::fs::write(&tmp, data)?;
    let ok = superzej_core::util::git_cmd(&std::env::temp_dir())
        .args(["bundle", "list-heads"])
        .arg(&tmp)
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&tmp);
    if !ok {
        anyhow::bail!("captured bundle failed list-heads verification");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_key_uses_repo_slug_worktree_dirname_env() {
        let k = snapshot_key(
            Path::new("/home/u/code/myrepo"),
            "/home/u/wt/sz-fox",
            "hetzner",
        );
        assert_eq!(k.worktree_slug, "sz-fox");
        assert_eq!(k.env, "hetzner");
        assert!(k.prefix().ends_with("/sz-fox/hetzner"));
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
