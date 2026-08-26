//! `thegn sandbox` — on-demand container-estate maintenance.
//!
//! `gc` is the startup orphan sweep runnable any time; `prune` reclaims
//! thegn-owned stopped containers and `thegn.managed` images/volumes (locally or
//! on a provisioned `--host`). Both are owned-only by construction — the removal
//! argv is built from `sandbox_manage` ownership witnesses / label-filtered
//! listings, so a foreign resource is never a candidate — and cleanup only ever
//! runs when explicitly invoked (never on a schedule).

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::Result;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::outln;
use thegn_core::sandbox::{self, Backend, PruneKinds, PruneReport};
use thegn_core::sandbox_manage::{self as manage, human_bytes};
use thegn_core::store::WorkspaceStore;

/// Bounded per-command deadline for host-side prune (over the control channel).
const HOST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Remove thegn containers whose worktree no longer exists in the registry
    /// (the startup orphan sweep, on demand). Reports what it removed per
    /// backend; exit 0 even when there is nothing to do.
    Gc,
    /// Remove thegn-owned stopped containers and `thegn.managed` images/volumes.
    /// Owned-only; persistent-role volumes are always kept and named. Lists +
    /// confirms on a TTY; `--yes` executes non-interactively; `--dry-run` never
    /// removes. With `--host <name>`, prunes on that provisioned host over its
    /// control channel. With no kind flag, all three kinds are pruned.
    Prune {
        /// Prune on a provisioned `[host.<name>]` instead of locally.
        #[arg(long)]
        host: Option<String>,
        /// Execute without the interactive confirm (for scripts).
        #[arg(long)]
        yes: bool,
        /// List what would be removed and stop.
        #[arg(long)]
        dry_run: bool,
        /// Restrict to stopped containers.
        #[arg(long)]
        containers: bool,
        /// Restrict to `thegn.managed` images.
        #[arg(long)]
        images: bool,
        /// Restrict to `thegn.managed` volumes (persistent roles always kept).
        #[arg(long)]
        volumes: bool,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Gc => gc(),
        Action::Prune {
            host,
            yes,
            dry_run,
            containers,
            images,
            volumes,
        } => {
            let kinds = resolve_kinds(containers, images, volumes);
            match host {
                Some(name) => prune_host(cfg, &name, kinds, yes, dry_run),
                None => prune_local(kinds, yes, dry_run),
            }
        }
    }
}

fn resolve_kinds(containers: bool, images: bool, volumes: bool) -> PruneKinds {
    if !containers && !images && !volumes {
        PruneKinds::all()
    } else {
        PruneKinds {
            containers,
            images,
            volumes,
        }
    }
}

fn gc() -> Result<()> {
    let db = Db::open()?;
    // A FAILED worktrees query must NOT read as "no worktrees" — that would make
    // every thegn container look orphaned. Bail instead (same rule as the
    // startup sweep in run.rs).
    let rows = db
        .worktrees()
        .map_err(|e| anyhow::anyhow!("worktrees query failed, refusing to sweep: {e}"))?;
    let worktrees: Vec<String> = rows.into_iter().map(|r| r.worktree).collect();
    let report = sandbox::run_gc_detailed(&worktrees);
    if report.is_empty() {
        outln!("sandbox gc: no orphan containers to remove");
        return Ok(());
    }
    let mut total = 0;
    for (backend, removed) in &report {
        total += removed.len();
        outln!("  {backend}: {} — {}", removed.len(), removed.join(", "));
    }
    outln!("sandbox gc: removed {total} orphan container(s)");
    Ok(())
}

fn prune_local(kinds: PruneKinds, yes: bool, dry_run: bool) -> Result<()> {
    // Plan first (list only), then confirm, then execute — two listings, so the
    // human sees exactly what a `--yes` run would remove.
    let plan = sandbox::prune_local(kinds, false);
    print_plan("local", &plan);
    if dry_run {
        outln!("dry run — nothing removed");
        return Ok(());
    }
    if plan.is_empty() {
        return Ok(());
    }
    if !confirm_or_bail(yes)? {
        outln!("aborted");
        return Ok(());
    }
    let done = sandbox::prune_local(kinds, true);
    print_removed("local", &done);
    Ok(())
}

fn confirm_or_bail(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("refusing to prune without a TTY — pass --yes to confirm");
    }
    Ok(super::confirm("Remove the above thegn-owned resources?"))
}

fn print_plan(location: &str, plan: &PruneReport) {
    if plan.is_empty() && plan.kept_volumes.is_empty() {
        outln!("sandbox prune ({location}): nothing to remove");
        return;
    }
    outln!("sandbox prune ({location}) — would remove:");
    for c in &plan.containers {
        outln!("  container  {c}");
    }
    for i in &plan.images {
        outln!("  image      {i}");
    }
    for v in &plan.volumes {
        outln!("  volume     {v}");
    }
    if plan.bytes > 0 {
        outln!("  (~{} reclaimable)", human_bytes(plan.bytes));
    }
    for k in &plan.kept_volumes {
        outln!("  kept       {k}  (persistent role — user state, skipped)");
    }
}

fn print_removed(location: &str, done: &PruneReport) {
    outln!(
        "sandbox prune ({location}): removed {} container(s), {} image(s), {} volume(s){}",
        done.containers.len(),
        done.images.len(),
        done.volumes.len(),
        if done.bytes > 0 {
            format!(" (~{} reclaimed)", human_bytes(done.bytes))
        } else {
            String::new()
        },
    );
}

// --- host-side prune --------------------------------------------------------

fn prune_host(cfg: &Config, name: &str, kinds: PruneKinds, yes: bool, dry_run: bool) -> Result<()> {
    let binding = cfg.host_binding(name).ok_or_else(|| {
        anyhow::anyhow!("no usable [host.{name}] in the global config (see `thegn config path`)")
    })?;
    let runner = thegn_svc::host::oci_runner_for(&binding.reach)
        .map_err(|e| anyhow::anyhow!("cannot reach host {name}: {e}"))?;
    let Some(backend) = detect_host_backend(&runner)? else {
        anyhow::bail!("no podman/docker runtime found on host {name}");
    };

    // Plan from ownership-filtered listings run on the host.
    let plan = host_plan(&runner, backend, kinds)?;
    print_plan(name, &plan);
    if dry_run {
        outln!("dry run — nothing removed on {name}");
        return Ok(());
    }
    if plan.is_empty() {
        return Ok(());
    }
    if !confirm_or_bail(yes)? {
        outln!("aborted");
        return Ok(());
    }
    let done = host_execute(&runner, backend, &plan)?;
    print_removed(name, &done);
    Ok(())
}

/// Which container runtime the host has, as a [`Backend`] whose `sandbox_manage`
/// argv dialect matches. Podman preferred (the provisioning default).
fn detect_host_backend(runner: &thegn_svc::host::OciRunner) -> Result<Option<Backend>> {
    let (_ok, out, _err) = runner
        .host_exec(
            "command -v podman >/dev/null 2>&1 && echo podman || \
             (command -v docker >/dev/null 2>&1 && echo docker || echo none)",
            HOST_TIMEOUT,
        )
        .map_err(|e| anyhow::anyhow!("host runtime probe failed: {e}"))?;
    Ok(match out.trim() {
        "podman" => Some(Backend::Podman),
        "docker" => Some(Backend::Docker),
        _ => None,
    })
}

/// The host `bin` for `backend` (no `sudo` wrap over the control channel).
fn host_bin(backend: Backend) -> &'static str {
    if backend == Backend::Docker {
        "docker"
    } else {
        "podman"
    }
}

/// Run a management argv on the host, returning stdout (or `None` on failure).
fn host_run(
    runner: &thegn_svc::host::OciRunner,
    backend: Backend,
    argv: &[String],
) -> Option<String> {
    let cmd = std::iter::once(host_bin(backend).to_string())
        .chain(argv.iter().map(|a| sh_quote(a)))
        .collect::<Vec<_>>()
        .join(" ");
    runner
        .host_exec(&cmd, HOST_TIMEOUT)
        .ok()
        .and_then(|(ok, out, _err)| ok.then_some(out))
}

/// Build the host prune plan (list only) via ownership-filtered listings. Only
/// owned resources are ever parsed (label filter for volumes, managed-repo tag
/// for images, `thegn-` name prefix for containers), so the plan can name
/// nothing foreign.
fn host_plan(
    runner: &thegn_svc::host::OciRunner,
    backend: Backend,
    kinds: PruneKinds,
) -> Result<PruneReport> {
    let mut rep = PruneReport::default();
    if kinds.containers
        && let Some(argv) = manage::mgmt_list_argv(backend)
        && let Some(out) = host_run(runner, backend, &argv)
    {
        let rows = if backend == Backend::Docker {
            thegn_core::sandbox::parse_docker_ps(&out)
        } else {
            thegn_core::sandbox::parse_podman_ps(&out)
        };
        for c in rows
            .iter()
            .filter(|c| c.ours && !manage::container_running(&c.status))
        {
            rep.containers.push(c.name.clone());
        }
    }
    if kinds.images
        && let Some(argv) = manage::mgmt_image_list_argv(backend)
        && let Some(out) = host_run(runner, backend, &argv)
    {
        for img in manage::parse_owned_images(&out) {
            rep.bytes += img.size_bytes.unwrap_or(0);
            rep.images.push(img.reference);
        }
    }
    if kinds.volumes
        && let Some(argv) = manage::mgmt_volume_list_argv(backend)
        && let Some(out) = host_run(runner, backend, &argv)
    {
        for vol in manage::parse_owned_volumes(&out) {
            if vol.is_persistent() {
                rep.kept_volumes.push(vol.name().to_string());
            } else {
                rep.bytes += vol.size_bytes.unwrap_or(0);
                rep.volumes.push(vol.name().to_string());
            }
        }
    }
    Ok(rep)
}

/// Execute the host prune plan (rm each planned resource over the control
/// channel). Every name came from an ownership-filtered listing, so nothing
/// foreign can be targeted.
fn host_execute(
    runner: &thegn_svc::host::OciRunner,
    backend: Backend,
    plan: &PruneReport,
) -> Result<PruneReport> {
    let bin = host_bin(backend);
    let mut done = PruneReport {
        kept_volumes: plan.kept_volumes.clone(),
        ..Default::default()
    };
    for c in &plan.containers {
        let cmd = format!("{bin} rm -f {}", sh_quote(c));
        if runner.host_exec(&cmd, HOST_TIMEOUT).is_ok() {
            done.containers.push(c.clone());
        }
    }
    for i in &plan.images {
        let cmd = format!("{bin} image rm {}", sh_quote(i));
        if runner.host_exec(&cmd, HOST_TIMEOUT).is_ok() {
            done.images.push(i.clone());
        }
    }
    for v in &plan.volumes {
        let cmd = format!("{bin} volume rm {}", sh_quote(v));
        if runner.host_exec(&cmd, HOST_TIMEOUT).is_ok() {
            done.volumes.push(v.clone());
        }
    }
    done.bytes = plan.bytes;
    Ok(done)
}

/// Single-quote a shell argument (for the `sh -lc` host exec). Container names,
/// image references and the `{{json .}}` format string all pass through intact.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
