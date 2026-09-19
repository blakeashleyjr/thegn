//! Process configuration admission: the one place a thegn process turns its
//! config sources into an authority-bearing [`AdmittedConfig`] generation.
//!
//! Startup (`admit_process`) captures the process inputs once (selected
//! config path, profile roots, the environment keys the mapper reads, CLI
//! `--set` overrides), admits every non-DB layer strictly, only then installs
//! the admitted `[database]` migration policy and opens the state store (so a
//! legitimate older schema is upgraded, but never before the trusted layers
//! were proven valid), captures host definitions strictly from the migrated
//! store, admits the final candidate, and publishes it as generation 1 in the
//! process [`AdmissionStore`]. Only after publication are the admitted
//! runtime effects installed. A trusted-layer failure is a typed hard error
//! naming the source and key: there is no fallback to defaults. The one
//! first-run default is an absent *implicit* config file. A host layer that
//! cannot be captured (newer schema, refused migration, bad stored row) is
//! not a refusal: the generation is published host-less and
//! non-authoritative, with a persistent banner, so recovery stays possible.
//!
//! Reload (`reload`) reuses the frozen process capture — environment and CLI
//! overrides are process inputs and do not change under a running process —
//! re-reads the files and host definitions, and publishes by compare-and-swap
//! against the generation it observed when it began. A failed reload keeps
//! the prior generation for display and marks the store degraded, which makes
//! [`AdmissionStore::authorize`] refuse every revision until a reload
//! succeeds; repeated identical failures coalesce.
//!
//! Blocking file/SQLite work: call from startup before the input/render loop
//! or from an off-loop thread, never from a loop handler.

use std::sync::{Arc, Mutex, OnceLock};

use thegn_core::config_admission::{AdmissionHealth, AdmittedConfig};
use thegn_core::config_admission_store::{AdmissionStore, FailureReport, StoreHealth};

use crate::config_capture::{
    CaptureFailure, CapturedCliInputs, CapturedConfig, ConfigCaptureSeed, HostLayer,
    HostStoreFailure, bounded_detail,
};

/// Operator-facing notes that outlive a single status line.
#[derive(Default)]
struct Notes {
    /// The last reload failure, until a reload publishes again.
    reload_failure: Option<String>,
    /// Why the current generation has no host layer, if it has none.
    hosts_unavailable: Option<String>,
    /// Set once a reload changed `[database]`, which only takes effect on
    /// restart (the migration policy is process-lifetime).
    database_restart: bool,
}

/// One process's frozen capture plus its live generation store.
pub(crate) struct ProcessAdmission {
    seed: Mutex<ConfigCaptureSeed>,
    actor: thegn_core::db::MigrationActor,
    store: AdmissionStore,
    /// The `[database]` policy installed at startup (process-lifetime).
    startup_database: thegn_core::config::DatabaseConfig,
    notes: Mutex<Notes>,
}

/// `install_policy` is true exactly once per process: at startup.
type Admit<'a> = dyn Fn(
        &ConfigCaptureSeed,
        thegn_core::db::MigrationActor,
        bool,
    ) -> Result<CapturedConfig, CaptureFailure>
    + 'a;

/// Outcome of one reload attempt.
pub(crate) enum ReloadOutcome {
    Published(Arc<AdmittedConfig>),
    /// The first report of a failure (worth one diagnostic).
    Failed(CaptureFailure),
    /// A repeat of the failure already reported; stay quiet.
    FailedCoalesced,
    /// Another publication won the compare-and-swap; the store holds a newer
    /// generation (use [`display`]).
    Superseded,
    /// No startup admission happened in this process.
    Unavailable,
}

fn fingerprint(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl ProcessAdmission {
    /// Admit once and publish generation 1. Runtime effects are installed by
    /// the caller only after this returns `Ok`.
    fn admit_initial(
        seed: ConfigCaptureSeed,
        actor: thegn_core::db::MigrationActor,
        admit: &Admit<'_>,
    ) -> Result<(Self, Arc<AdmittedConfig>), CaptureFailure> {
        let (admitted, hosts_unavailable) = admit(&seed, actor, true)?.into_parts();
        let process = Self {
            seed: Mutex::new(seed),
            actor,
            store: AdmissionStore::new(),
            startup_database: admitted.config().database.clone(),
            notes: Mutex::new(Notes {
                hosts_unavailable: hosts_unavailable.map(|reason| reason.to_string()),
                ..Notes::default()
            }),
        };
        let published = process
            .store
            .publish(admitted, 0)
            .expect("a fresh store accepts generation 1");
        Ok((process, published))
    }

    fn notes(&self) -> std::sync::MutexGuard<'_, Notes> {
        self.notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn reload_with(&self, admit: &Admit<'_>) -> ReloadOutcome {
        // The CAS base is the generation current when capture BEGINS, so a
        // slower concurrent reload can never overwrite a newer publication.
        let observed = self
            .store
            .display()
            .map_or(0, |current| current.revision().generation);
        let result = {
            let seed = self
                .seed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Never re-install the migration policy on reload: it is
            // process-lifetime, and re-canonicalizing a pinned executable
            // mid-rebuild would refuse every later launch.
            admit(&seed, self.actor, false)
        };
        match result {
            Ok(captured) => {
                let (admitted, hosts_unavailable) = captured.into_parts();
                let database_changed = admitted.config().database != self.startup_database;
                match self.store.publish(admitted, observed) {
                    Ok(published) => {
                        let mut notes = self.notes();
                        notes.reload_failure = None;
                        notes.hosts_unavailable = hosts_unavailable.map(|r| r.to_string());
                        if database_changed && !notes.database_restart {
                            notes.database_restart = true;
                            thegn_core::config::config_warn(
                                "[database] changed: the migration policy takes effect after a restart",
                            );
                        }
                        ReloadOutcome::Published(published)
                    }
                    Err(_) => ReloadOutcome::Superseded,
                }
            }
            Err(failure) => {
                let text = failure.to_string();
                let report = self
                    .store
                    .record_failure(failure_category(&failure), fingerprint(&text));
                self.notes().reload_failure = Some(text);
                match report {
                    FailureReport::First => ReloadOutcome::Failed(failure),
                    FailureReport::Coalesced => ReloadOutcome::FailedCoalesced,
                }
            }
        }
    }

    pub(crate) fn store(&self) -> &AdmissionStore {
        &self.store
    }

    /// The persistent statusbar banner, or `None` when everything is healthy.
    fn banner(&self) -> Option<String> {
        let notes = self.notes();
        match self.store.health() {
            StoreHealth::Degraded { generation, .. } => Some(format!(
                "CONFIG RELOAD FAILED (showing gen {generation}, launches refused): {}",
                notes
                    .reload_failure
                    .as_deref()
                    .unwrap_or("see `thegn config validate`")
            )),
            StoreHealth::Current { .. } => notes.hosts_unavailable.as_ref().map(|reason| {
                format!("CONFIG: host definitions unavailable, launches refused: {reason}")
            }),
            StoreHealth::Empty => None,
        }
    }
}

static PROCESS: OnceLock<ProcessAdmission> = OnceLock::new();

/// Borrowed CLI inputs for [`admit_process`].
pub(crate) struct ProcessInputs<'a> {
    pub(crate) config: Option<&'a std::path::Path>,
    pub(crate) profile: Option<&'a str>,
    pub(crate) overrides: &'a [String],
    pub(crate) actor: thegn_core::db::MigrationActor,
}

/// Capture the host-definition layer from the state store, after every
/// non-DB layer was admitted. At startup (`install_policy`) the admitted
/// `[database]` policy is installed before the first open, so an authorized
/// controller upgrades a legitimate older schema here.
///
/// No failure here is a refusal: a policy that cannot be installed, a
/// refused migration, a newer schema, an unopenable store or an invalid
/// stored row all make the host layer *unavailable*. The generation is then
/// published host-less and non-authoritative, which keeps doctor, logs,
/// notify hooks, stdio bridges and `thegn host rm` (the recovery for a bad
/// row) working while new launches are refused and the reason is shown.
fn capture_hosts_from_store(
    cfg: &thegn_core::config::Config,
    actor: thegn_core::db::MigrationActor,
    install_policy: bool,
) -> HostLayer {
    use thegn_core::store::HostStore;
    if install_policy
        && let Err(error) = thegn_core::db::install_migration_policy(&cfg.database, actor)
    {
        return HostLayer::Unavailable(HostStoreFailure::MigrationPolicy(bounded_detail(
            &error.to_string(),
        )));
    }
    let db = match thegn_core::db::Db::open() {
        Ok(db) => db,
        Err(error) => {
            return HostLayer::Unavailable(
                match error.downcast_ref::<thegn_core::db::MigrationAccessError>() {
                    Some(thegn_core::db::MigrationAccessError::NewerSchema {
                        observed,
                        supported,
                    }) => HostStoreFailure::NewerSchema {
                        observed: *observed,
                        supported: *supported,
                    },
                    _ => HostStoreFailure::Open(bounded_detail(&format!("{error:#}"))),
                },
            );
        }
    };
    match db.host_defs_checked() {
        Ok(hosts) => HostLayer::Admitted(hosts),
        Err(error) => HostLayer::Unavailable(HostStoreFailure::Definitions(error)),
    }
}

fn production_admit(
    seed: &ConfigCaptureSeed,
    actor: thegn_core::db::MigrationActor,
    install_policy: bool,
) -> Result<CapturedConfig, CaptureFailure> {
    seed.admit_staged(
        &crate::platform::config_file_capture::startup_reader(),
        |cfg| Ok(capture_hosts_from_store(cfg, actor, install_policy)),
    )
}

/// Admit this process's configuration once and publish generation 1.
///
/// Must run after `profile::reroot` (the capture binds the rerooted profile
/// roots) and before any DB consumer, provider, agent or sandbox launch. A
/// second call (e.g. `open` falling through to the interactive launch)
/// returns the already-published generation.
pub(crate) fn admit_process(
    inputs: ProcessInputs<'_>,
) -> Result<Arc<AdmittedConfig>, CaptureFailure> {
    if let Some(process) = PROCESS.get() {
        return process.store.display().ok_or(CaptureFailure::Admission(
            thegn_core::config_admission::ConfigAdmissionError::TransientIo,
            None,
        ));
    }
    let seed = ConfigCaptureSeed::capture_process(CapturedCliInputs {
        config: inputs.config,
        profile: inputs.profile,
        profile_paths: None,
        overrides: inputs.overrides,
    })?;
    let (process, published) =
        ProcessAdmission::admit_initial(seed, inputs.actor, &production_admit)?;
    let hosts_note = process.notes().hosts_unavailable.clone();
    if PROCESS.set(process).is_err() {
        // Unreachable in practice (startup is single-threaded and the early
        // return above covers a second call); refuse rather than publish a
        // second, divergent process store.
        return Err(CaptureFailure::Admission(
            thegn_core::config_admission::ConfigAdmissionError::TransientIo,
            None,
        ));
    }
    published.config().install_admitted_runtime();
    // Advisory diagnostics (deprecations, clamped values): already bounded
    // and redacted by admission; errors never reach here.
    for diagnostic in published.trace().diagnostics() {
        thegn_core::config::config_warn(&diagnostic.message);
    }
    if let Some(reason) = hosts_note {
        thegn_core::config::config_warn(&format!(
            "host definitions unavailable ({reason}); running without stored hosts — launches are refused until this is fixed"
        ));
    }
    Ok(published)
}

/// The process store, once startup admission has published.
pub(crate) fn store() -> Option<&'static AdmissionStore> {
    PROCESS.get().map(ProcessAdmission::store)
}

/// The latest admitted snapshot for display/projection consumers (hydration,
/// status). A degraded or host-less generation is included on purpose; new
/// authority must go through [`AdmissionStore::authorize`] / `current`.
pub(crate) fn display() -> Option<Arc<AdmittedConfig>> {
    store().and_then(AdmissionStore::display)
}

/// The persistent statusbar banner for a degraded configuration, if any.
pub(crate) fn banner() -> Option<String> {
    PROCESS.get().and_then(ProcessAdmission::banner)
}

/// Whether `admitted` may authorize a new launch: it must be the store's
/// current, healthy generation with its host layer.
pub(crate) fn require_launchable(admitted: &AdmittedConfig) -> Result<(), String> {
    if admitted.health() == AdmissionHealth::HostsUnavailable {
        return Err(banner().unwrap_or_else(|| {
            "configuration has no admitted host definitions; launches are refused".into()
        }));
    }
    match store() {
        Some(store) => store
            .authorize(&admitted.revision())
            .map(|_| ())
            .map_err(|error| error.to_string()),
        None => Ok(()),
    }
}

/// The configuration for cleanup/teardown that an earlier admitted
/// generation already authorized (VPN deregistration, provider sync-back,
/// close checkpoints, landed-worktree removal). It is the last admitted
/// generation even when a later reload failed — cleanup must stay possible
/// while new authority is refused — and never a default. Only a process
/// without startup admission (unit tests) uses the legacy layered load.
pub(crate) fn cleanup_config() -> thegn_core::config::Config {
    match display() {
        Some(admitted) => admitted.config().clone(),
        None => {
            thegn_core::config::Config::load_layered(&thegn_core::config::ProcessEnv, &[], None)
        }
    }
}

/// Re-admit from the frozen process capture and publish by CAS. Installs the
/// admitted runtime effects only for a published generation.
pub(crate) fn reload() -> ReloadOutcome {
    let Some(process) = PROCESS.get() else {
        return ReloadOutcome::Unavailable;
    };
    let outcome = process.reload_with(&production_admit);
    if let ReloadOutcome::Published(published) = &outcome {
        published.config().install_admitted_runtime();
    }
    outcome
}

/// What a reload outcome delivers to the interactive loop. A `Superseded`
/// reload still delivers — the generation that won the CAS (a wizard host
/// add, a daemon launch) — so the loop's keymap/theme/sidebar never stay on
/// a stale generation. A coalesced failure delivers nothing (the persistent
/// banner already shows it).
pub(crate) fn loop_update(
    outcome: ReloadOutcome,
    current: impl FnOnce() -> Option<Arc<AdmittedConfig>>,
) -> Option<Result<Arc<AdmittedConfig>, String>> {
    match outcome {
        ReloadOutcome::Published(admitted) => Some(Ok(admitted)),
        ReloadOutcome::Failed(error) => Some(Err(error.to_string())),
        ReloadOutcome::Superseded => current().map(Ok),
        ReloadOutcome::FailedCoalesced | ReloadOutcome::Unavailable => None,
    }
}

/// Run the same capture and non-DB admission a configured verb runs, without
/// touching the state DB, and name what it refuses (source + key path). Used
/// by `config validate` and `doctor` so they report exactly what startup
/// would refuse. On success, returns the admission-only warnings (values the
/// runtime clamps) that the file validators do not report. Must run after
/// `profile::reroot`.
pub(crate) fn check_sources(
    config: Option<&std::path::Path>,
    overrides: &[String],
) -> Result<Vec<String>, String> {
    let seed = ConfigCaptureSeed::capture_process(CapturedCliInputs {
        config,
        profile: None,
        profile_paths: None,
        overrides,
    })
    .map_err(|error| error.to_string())?;
    let captured = seed
        .admit_staged(
            &crate::platform::config_file_capture::startup_reader(),
            |_| {
                Ok(HostLayer::Admitted(
                    thegn_core::host_definition_snapshot::HostDefinitionsSnapshot::empty(
                        thegn_core::db::SCHEMA_VERSION,
                    ),
                ))
            },
        )
        .map_err(|error| error.to_string())?;
    let (admitted, _) = captured.into_parts();
    Ok(admitted
        .trace()
        .diagnostics()
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("value is out of range"))
        .map(|diagnostic| diagnostic.message.clone())
        .collect())
}

/// The typed admission category a capture failure is recorded under.
fn failure_category(
    failure: &CaptureFailure,
) -> thegn_core::config_admission::ConfigAdmissionError {
    use crate::config_capture::ConfigFileReadError as Read;
    use thegn_core::config_admission::ConfigAdmissionError as E;
    match failure {
        CaptureFailure::Admission(error, _) => error.clone(),
        CaptureFailure::Source(Read::TooLarge) => E::Oversized,
        CaptureFailure::Source(Read::Changed) => E::TransientIo,
        CaptureFailure::Source(_) => E::Unreadable,
        CaptureFailure::Input(_) => E::EnvironmentInvalid,
        CaptureFailure::State(_) => E::HostInvalid,
    }
}

#[cfg(test)]
#[path = "config_startup_tests.rs"]
mod tests;
