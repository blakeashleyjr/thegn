//! The explicitly selected build/test runner, built from an env's provider
//! config (`[jobs] runner = "<env>"` or `thegn job run --runner <env>`).
//!
//! The Sprites and Vercel executors (`thegn_svc::{sprites,vercel}::job`) take a
//! ready provider, a project binding and a snapshot source. This module builds
//! those from the same `[env.<name>.provider]` table an interactive worktree
//! uses (`provider_factory`): the token is resolved here through the one audited
//! secret chokepoint and moves straight into the provider's client, so it never
//! reaches the recipe's environment. Every missing piece is a typed
//! [`RunnerError`] naming what to configure.
//!
//! It also owns the daemon's startup reconcile of attempts a crash left behind.

use std::fmt;
use std::path::{Path, PathBuf};

use thegn_core::config::{Config, EnvProviderConfig, EnvProviderKind};
use thegn_svc::provider::Provider;
use thegn_svc::selected_job::{SelectedJobResult, SelectedRunner};
use thegn_svc::sprites::job as sprites_job;
use thegn_svc::sprites::job::Reconciled;
use thegn_svc::vercel::job as vercel_job;

/// Why a selected runner could not be built. Each variant names the env and the
/// piece of configuration to fix; none carries a secret value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunnerError {
    /// Neither `--runner` nor `[jobs] runner` names an env.
    NotSelected,
    /// The named env is not configured.
    UnknownEnv(String),
    /// The env's provider has no selected-job executor.
    UnsupportedProvider { env: String, provider: String },
    /// The token reference did not resolve (`detail` names the reference).
    NoToken { env: String, detail: String },
    /// The env lacks the project binding the executor needs.
    MissingBinding {
        env: String,
        field: &'static str,
        what: &'static str,
    },
    /// The env's binding is present but unusable.
    InvalidBinding { env: String, detail: String },
    /// The provider could not be built for another reason.
    Provider { env: String, detail: String },
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSelected => f.write_str(
                "no job runner is selected: pass --runner <env>, or set [jobs] runner to an \
                 [env.<name>] whose provider is sprites or vercel",
            ),
            Self::UnknownEnv(env) => write!(f, "job runner: no [env.{env}] is configured"),
            Self::UnsupportedProvider { env, provider } => write!(
                f,
                "job runner: [env.{env}] uses provider {provider:?}; only sprites and vercel \
                 can run selected jobs"
            ),
            Self::NoToken { env, detail } => {
                write!(f, "job runner [env.{env}]: no usable token: {detail}")
            }
            Self::MissingBinding { env, field, what } => write!(
                f,
                "job runner [env.{env}]: no project binding: set {field} in \
                 [env.{env}.provider] ({what})"
            ),
            Self::InvalidBinding { env, detail } => {
                write!(
                    f,
                    "job runner [env.{env}]: invalid project binding: {detail}"
                )
            }
            Self::Provider { env, detail } => {
                write!(
                    f,
                    "job runner [env.{env}]: cannot build the provider: {detail}"
                )
            }
        }
    }
}

impl std::error::Error for RunnerError {}

/// The env name to use: an explicit request wins, else `[jobs] runner`.
pub(crate) fn select_env<'a>(
    cfg: &'a Config,
    requested: Option<&'a str>,
) -> Result<&'a str, RunnerError> {
    let named = |name: &'a str| {
        let name = name.trim();
        (!name.is_empty()).then_some(name)
    };
    requested
        .and_then(named)
        .or_else(|| named(&cfg.jobs.runner))
        .ok_or(RunnerError::NotSelected)
}

/// A ledger- and alias-safe project key for a repo root: lowercase ASCII
/// alphanumerics with single interior dashes (both executors derive their alias
/// from it, and the alias rule is stricter than the ledger's). `None` when the
/// name has no ASCII letter or digit to build one from.
pub(crate) fn project_key(repo_root: &Path) -> Option<String> {
    let raw = thegn_core::repo::repo_name_from_path(repo_root);
    let mut key = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            key.push(ch.to_ascii_lowercase());
        } else if !key.is_empty() && !key.ends_with('-') {
            key.push('-');
        }
    }
    key.truncate(48);
    while key.ends_with('-') {
        key.pop();
    }
    (!key.is_empty()).then_some(key)
}

/// True for a row a startup pass may reattach: `Running` with a session handle
/// and no live owner lease. A live lease (fresh heartbeat, or its process
/// still alive) belongs to a running owner and is never touched.
pub(crate) fn is_orphaned_running(
    state: sprites_job::AttemptState,
    session: Option<&str>,
    lease: Option<&sprites_job::OwnerLease>,
    now_ms: i64,
) -> bool {
    state == sprites_job::AttemptState::Running
        && session.is_some()
        && !lease.is_some_and(|lease| lease.is_live(now_ms))
}

/// The attempts of `rows` a startup pass may reattach.
pub(crate) fn orphaned_running(rows: &[sprites_job::AttemptRecord], now_ms: i64) -> Vec<u64> {
    rows.iter()
        .filter(|row| {
            is_orphaned_running(
                row.state,
                row.session.as_deref(),
                row.lease.as_ref(),
                now_ms,
            )
        })
        .map(|row| row.attempt)
        .collect()
}

/// A built executor.
pub(crate) enum Runner {
    Sprites(Box<sprites_job::SpritesSelectedRunner>),
    Vercel(Box<vercel_job::VercelSelectedRunner>),
}

/// What the ledger recorded about the attempt behind a receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttemptSummary {
    pub attempt: u64,
    pub duration_ms: Option<u64>,
    /// The reported spend estimate, in micro-USD (`None` until it reported).
    pub spent_micros: Option<u64>,
    /// What the attempt reserved against the envelope, in micro-USD.
    pub reserved_micros: u64,
}

fn summary(
    attempt: u64,
    created_ms: i64,
    finished_ms: Option<i64>,
    spent_micros: Option<u64>,
    reserved_micros: u64,
) -> AttemptSummary {
    AttemptSummary {
        attempt,
        duration_ms: finished_ms.and_then(|end| u64::try_from(end.saturating_sub(created_ms)).ok()),
        spent_micros,
        reserved_micros,
    }
}

impl Runner {
    pub(crate) fn alias(&self) -> &str {
        match self {
            Self::Sprites(runner) => runner.alias(),
            Self::Vercel(runner) => runner.alias(),
        }
    }

    pub(crate) fn selected(&self) -> &dyn SelectedRunner {
        match self {
            Self::Sprites(runner) => runner.as_ref(),
            Self::Vercel(runner) => runner.as_ref(),
        }
    }

    /// Settle what a crash left behind. Async: call it on a runtime, never from
    /// the thread that blocks inside `submit`.
    pub(crate) async fn reconcile_pending(&self) -> anyhow::Result<Vec<Reconciled>> {
        match self {
            Self::Sprites(runner) => runner.reconcile_pending().await,
            Self::Vercel(runner) => runner.reconcile_pending().await,
        }
    }

    /// Reattach Sprites attempts a dead owner left `Running` with a session
    /// handle: follow the provider's stream while it still holds the session,
    /// otherwise settle them as indeterminate. A row with a live owner lease is
    /// never touched, and nothing is redispatched. Vercel has no session to
    /// follow, so its `reconcile_pending` is the whole recovery. Async: call it
    /// on a runtime, never from the thread that blocks inside `submit`.
    pub(crate) async fn reattach_orphans(&self) -> anyhow::Result<Vec<u64>> {
        let Self::Sprites(runner) = self else {
            return Ok(Vec::new());
        };
        let now = thegn_core::util::now_ms();
        let attempts = orphaned_running(&runner.store().records()?, now);
        for attempt in &attempts {
            match runner.reattach(*attempt).await {
                Ok(report) => tracing::info!(
                    target: "thegn::jobs",
                    attempt = report.attempt,
                    reattached = report.reattached,
                    "reattached an orphaned selected-job attempt"
                ),
                Err(error) => tracing::warn!(
                    target: "thegn::jobs",
                    attempt,
                    %error,
                    "cannot reattach an orphaned selected-job attempt"
                ),
            }
        }
        Ok(attempts)
    }

    /// The newest ledger row for the receipt's recipe and source that was
    /// created at or after `since_ms` (the run's start): duration and cost the
    /// route's receipt does not carry. `None` when the ledger cannot be read or
    /// this run created no such row, so an earlier run's numbers are never
    /// shown as this run's.
    pub(crate) fn latest_attempt(
        &self,
        result: &SelectedJobResult,
        since_ms: i64,
    ) -> Option<AttemptSummary> {
        match self {
            Self::Sprites(runner) => runner
                .store()
                .records()
                .ok()?
                .into_iter()
                .rfind(|row| {
                    row.recipe == result.recipe
                        && row.source == result.source
                        && row.created_ms >= since_ms
                })
                .map(|row| {
                    summary(
                        row.attempt,
                        row.created_ms,
                        row.finished_ms,
                        row.spent_micros,
                        row.reserved_micros,
                    )
                }),
            Self::Vercel(runner) => runner
                .store()
                .records()
                .ok()?
                .into_iter()
                .rfind(|row| {
                    row.recipe == result.recipe
                        && row.source == result.source
                        && row.created_ms >= since_ms
                })
                .map(|row| {
                    summary(
                        row.attempt,
                        row.created_ms,
                        row.finished_ms,
                        row.spent_micros,
                        row.reserved_micros,
                    )
                }),
        }
    }
}

/// Build the executor for env `env_name`, bound to `project` and snapshotting
/// `root`. `handle` must belong to a multi-threaded runtime that something else
/// drives: the executor's `submit` blocks on it from a plain thread.
pub(crate) fn build(
    cfg: &Config,
    env_name: &str,
    project: &str,
    root: &Path,
    handle: tokio::runtime::Handle,
) -> Result<Runner, RunnerError> {
    let env = cfg
        .env
        .get(env_name)
        .ok_or_else(|| RunnerError::UnknownEnv(env_name.to_owned()))?;
    build_from(env_name, &env.provider, project, root, handle)
}

/// [`build`] from the env's provider table.
pub(crate) fn build_from(
    env_name: &str,
    pc: &EnvProviderConfig,
    project: &str,
    root: &Path,
    handle: tokio::runtime::Handle,
) -> Result<Runner, RunnerError> {
    let env = env_name.to_owned();
    let invalid = |detail: String| RunnerError::InvalidBinding {
        env: env.clone(),
        detail,
    };
    match EnvProviderKind::of(&pc.provider) {
        EnvProviderKind::Sprites => {
            // The binding needs no secret, so it is checked first: a missing
            // one is reported without resolving a token.
            let sprite = pc.id.trim();
            if sprite.is_empty() {
                return Err(RunnerError::MissingBinding {
                    env,
                    field: "id",
                    what: "the one Sprite this project is bound to",
                });
            }
            if sprite.contains('{') {
                return Err(invalid(format!(
                    "id {sprite:?} is a per-worktree template; a selected runner needs one \
                     fixed Sprite name"
                )));
            }
            let provider = match crate::provider_factory::provider_for_inventory(pc) {
                Ok(Provider::Sprites(provider)) => provider,
                Ok(_) => return Err(provider_error(&env, "unexpected provider kind".into())),
                Err(why) => return Err(classify_provider_error(&env, why)),
            };
            let credential = crate::provider_factory::token_ref(pc)
                .unwrap_or_else(|| "SPRITES_TOKEN".to_owned());
            let account = provider
                .account(&credential)
                .map_err(|error| invalid(format!("cannot derive the account: {error:#}")))?;
            let binding = sprites_job::ProjectBinding::new(project, &account, sprite)
                .map_err(|error| invalid(format!("{error:#}")))?;
            sprites_job::SpritesSelectedRunner::new(
                provider,
                binding,
                Box::new(sprites_job::DirSnapshot::new(root)),
                handle,
            )
            .map(|runner| Runner::Sprites(Box::new(runner)))
            .map_err(|error| provider_error(&env, format!("{error:#}")))
        }
        EnvProviderKind::Vercel => {
            let team = pc.team_id.trim();
            if team.is_empty() {
                return Err(RunnerError::MissingBinding {
                    env,
                    field: "team_id",
                    what: "the Vercel team (team_...)",
                });
            }
            let vercel_project = pc.project_id.trim();
            if vercel_project.is_empty() {
                return Err(RunnerError::MissingBinding {
                    env,
                    field: "project_id",
                    what: "the Vercel project (prj_...)",
                });
            }
            let provider = match crate::provider_factory::provider_for_inventory(pc) {
                Ok(Provider::Vercel(provider)) => provider,
                Ok(_) => return Err(provider_error(&env, "unexpected provider kind".into())),
                Err(why) => return Err(classify_provider_error(&env, why)),
            };
            let binding = vercel_job::JobBinding::new(project, team, vercel_project)
                .map_err(|error| invalid(format!("{error:#}")))?;
            vercel_job::VercelSelectedRunner::new(
                provider,
                binding,
                Box::new(vercel_job::DirSnapshot::new(root)),
                handle,
            )
            .map(|runner| Runner::Vercel(Box::new(runner)))
            .map_err(|error| provider_error(&env, format!("{error:#}")))
        }
        _ => Err(RunnerError::UnsupportedProvider {
            env,
            provider: pc.provider.trim().to_owned(),
        }),
    }
}

fn provider_error(env: &str, detail: String) -> RunnerError {
    RunnerError::Provider {
        env: env.to_owned(),
        detail,
    }
}

/// `provider_for_inventory` reports an unresolved token as `"API token <ref> could
/// not be resolved"` (the ref name only); every other failure is a provider-spec
/// problem.
fn classify_provider_error(env: &str, why: String) -> RunnerError {
    if why.ends_with("could not be resolved") {
        RunnerError::NoToken {
            env: env.to_owned(),
            detail: why,
        }
    } else {
        provider_error(env, why)
    }
}

/// The ledger directories that exist for `kind`, one per project.
fn ledger_projects(kind: EnvProviderKind) -> Vec<String> {
    let probe = match kind {
        EnvProviderKind::Sprites => sprites_job::AttemptStore::for_project("x")
            .dir()
            .to_path_buf(),
        EnvProviderKind::Vercel => vercel_job::AttemptStore::for_project("x")
            .dir()
            .to_path_buf(),
        _ => return Vec::new(),
    };
    let Some(root) = probe.parent().map(PathBuf::from) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut projects: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    projects.sort();
    projects
}

/// True when `project`'s ledger holds an unsettled attempt: a non-terminal row,
/// or a terminal one whose cleanup is still pending. A cheap local file scan, so
/// the daemon resolves no secret and opens no connection for an idle ledger.
fn has_pending_work(kind: EnvProviderKind, project: &str) -> bool {
    use sprites_job::Cleanup;
    match kind {
        EnvProviderKind::Sprites => sprites_job::AttemptStore::for_project(project)
            .records()
            .map(|rows| {
                rows.iter()
                    .any(|row| !row.is_terminal() || row.cleanup == Cleanup::Pending)
            })
            .unwrap_or(false),
        EnvProviderKind::Vercel => vercel_job::AttemptStore::for_project(project)
            .records()
            .map(|rows| {
                rows.iter()
                    .any(|row| !row.is_terminal() || row.cleanup == Cleanup::Pending)
            })
            .unwrap_or(false),
        _ => false,
    }
}

/// The envs a startup pass reconciles for, as `(name, provider table)`: the
/// configured `[jobs] runner` first (it wins a ledger both it and another env
/// could settle), then every other Sprites or Vercel env by name. An env of any
/// other provider kind never owns a job ledger and is left out.
fn reconcile_targets(cfg: &Config) -> Vec<(String, EnvProviderConfig)> {
    let configured = cfg.jobs.runner.trim();
    if !configured.is_empty() && !cfg.env.contains_key(configured) {
        tracing::warn!(
            target: "thegn::jobs",
            env = configured,
            "[jobs] runner names an env that is not configured; reconciling the other envs only"
        );
    }
    let mut names: Vec<&String> = cfg.env.keys().collect();
    names.sort();
    let first = names.iter().position(|name| name.as_str() == configured);
    if let Some(index) = first {
        let name = names.remove(index);
        names.insert(0, name);
    }
    names
        .into_iter()
        .filter_map(|name| {
            let provider = &cfg.env.get(name)?.provider;
            matches!(
                EnvProviderKind::of(&provider.provider),
                EnvProviderKind::Sprites | EnvProviderKind::Vercel
            )
            .then(|| (name.clone(), provider.clone()))
        })
        .collect()
}

/// Start one background reconcile pass over the job ledgers of every Sprites or
/// Vercel env (the configured `[jobs] runner` first): for each project ledger
/// with unsettled attempts, build the runner and run `reconcile_pending`,
/// logging what it settled. Ledgers left by an env whose runner changed, or by
/// another project's runner, are covered too. Inert (no thread, no I/O) when no
/// such env exists. Sprites and Vercel runners are stable-channel (every other
/// provider env was already removed by the channel clamp). Must be called inside
/// the daemon's runtime; the work runs on the blocking pool, never on the event
/// loop.
pub(crate) fn spawn_startup_reconcile(cfg: &Config) {
    let targets = reconcile_targets(cfg);
    if targets.is_empty() {
        return;
    }
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || reconcile_all(&targets, &handle));
}

/// One pass over every target env's ledgers. A ledger (kind, project) is
/// settled once, by the first env that can build a runner for it; an env whose
/// provider config or credentials no longer resolve is skipped with a log and
/// never fails the pass.
fn reconcile_all(targets: &[(String, EnvProviderConfig)], handle: &tokio::runtime::Handle) {
    let mut done: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for (env_name, pc) in targets {
        reconcile_configured(env_name, pc, handle, &mut done);
    }
}

/// One pass over the ledgers of one env's provider kind, skipping ledgers in
/// `done` and adding each one it settles.
fn reconcile_configured(
    env_name: &str,
    pc: &EnvProviderConfig,
    handle: &tokio::runtime::Handle,
    done: &mut std::collections::HashSet<(String, String)>,
) {
    let kind = EnvProviderKind::of(&pc.provider);
    for project in ledger_projects(kind) {
        let ledger = (pc.provider.trim().to_ascii_lowercase(), project.clone());
        if done.contains(&ledger) || !has_pending_work(kind, &project) {
            continue;
        }
        let runner = match build_from(env_name, pc, &project, Path::new("."), handle.clone()) {
            Ok(runner) => runner,
            Err(error) => {
                tracing::warn!(
                    target: "thegn::jobs",
                    env = env_name,
                    project = project.as_str(),
                    %error,
                    "cannot build the selected runner; its pending attempts stay unsettled"
                );
                continue;
            }
        };
        done.insert(ledger);
        // `reconcile_pending` is async; this is a blocking-pool thread, so
        // blocking on the runtime handle is the sanctioned bridge. Reattach runs
        // after it, on the rows it left unsettled.
        let reconciled = handle.block_on(runner.reconcile_pending());
        if let Err(error) = handle.block_on(runner.reattach_orphans()) {
            tracing::warn!(
                target: "thegn::jobs",
                project = project.as_str(),
                %error,
                "selected-job reattach pass failed"
            );
        }
        match reconciled {
            Ok(settled) => tracing::info!(
                target: "thegn::jobs",
                project = project.as_str(),
                alias = runner.alias(),
                settled = settled.len(),
                "reconciled pending selected-job attempts"
            ),
            Err(error) => tracing::warn!(
                target: "thegn::jobs",
                project = project.as_str(),
                alias = runner.alias(),
                %error,
                "selected-job reconcile failed"
            ),
        }
    }
}

#[cfg(test)]
#[path = "selected_runner_tests.rs"]
mod tests;
