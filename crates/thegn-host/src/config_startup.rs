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
//! runtime effects installed. Any failure is a typed hard error: there is no
//! fallback to defaults. The one first-run default is an absent *implicit*
//! config file (reported as `AdmissionHealth::FirstRunDefault`).
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

use thegn_core::config_admission::AdmittedConfig;
use thegn_core::config_admission_store::{AdmissionStore, FailureReport};
use thegn_core::host_definition_snapshot::HostDefinitionsSnapshot;

use crate::config_capture::{
    CaptureFailure, CapturedCliInputs, ConfigCaptureSeed, HostStoreFailure, bounded_detail,
};

/// One process's frozen capture plus its live generation store.
pub(crate) struct ProcessAdmission {
    seed: Mutex<ConfigCaptureSeed>,
    actor: thegn_core::db::MigrationActor,
    store: AdmissionStore,
}

type Admit<'a> = dyn Fn(
        &ConfigCaptureSeed,
        thegn_core::db::MigrationActor,
    ) -> Result<crate::config_capture::CapturedConfig, CaptureFailure>
    + 'a;

/// Outcome of one reload attempt.
pub(crate) enum ReloadOutcome {
    Published(Arc<AdmittedConfig>),
    /// The first report of a failure (worth one diagnostic).
    Failed(CaptureFailure),
    /// A repeat of the failure already reported; stay quiet.
    FailedCoalesced,
    /// Another publication won the compare-and-swap; nothing to do.
    Superseded,
    /// No startup admission happened in this process.
    Unavailable,
}

impl ProcessAdmission {
    /// Admit once and publish generation 1. Runtime effects are installed by
    /// the caller only after this returns `Ok`.
    fn admit_initial(
        seed: ConfigCaptureSeed,
        actor: thegn_core::db::MigrationActor,
        admit: &Admit<'_>,
    ) -> Result<(Self, Arc<AdmittedConfig>), CaptureFailure> {
        let captured = admit(&seed, actor)?;
        let process = Self {
            seed: Mutex::new(seed),
            actor,
            store: AdmissionStore::new(),
        };
        let published = process
            .store
            .publish(captured.into_admitted(), 0)
            .expect("a fresh store accepts generation 1");
        Ok((process, published))
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
            admit(&seed, self.actor)
        };
        match result {
            Ok(captured) => match self.store.publish(captured.into_admitted(), observed) {
                Ok(published) => ReloadOutcome::Published(published),
                Err(_) => ReloadOutcome::Superseded,
            },
            Err(failure) => match self.store.record_failure(failure_category(&failure)) {
                FailureReport::First => ReloadOutcome::Failed(failure),
                FailureReport::Coalesced => ReloadOutcome::FailedCoalesced,
            },
        }
    }

    pub(crate) fn store(&self) -> &AdmissionStore {
        &self.store
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

/// Strict host capture from the state store, run after every non-DB layer was
/// admitted: install that candidate's migration policy, open (and, when
/// authorized, migrate) the store, and read every host definition strictly.
fn capture_hosts_from_store(
    cfg: &thegn_core::config::Config,
    actor: thegn_core::db::MigrationActor,
) -> Result<HostDefinitionsSnapshot, CaptureFailure> {
    use thegn_core::store::HostStore;
    thegn_core::db::install_migration_policy(&cfg.database, actor).map_err(|error| {
        CaptureFailure::Hosts(HostStoreFailure::MigrationPolicy(bounded_detail(
            &error.to_string(),
        )))
    })?;
    let db = thegn_core::db::Db::open().map_err(|error| {
        CaptureFailure::Hosts(
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
        )
    })?;
    db.host_defs_checked()
        .map_err(|error| CaptureFailure::Hosts(HostStoreFailure::Definitions(error)))
}

fn production_admit(
    seed: &ConfigCaptureSeed,
    actor: thegn_core::db::MigrationActor,
) -> Result<crate::config_capture::CapturedConfig, CaptureFailure> {
    seed.admit_staged(
        &crate::platform::config_file_capture::startup_reader(),
        |cfg| capture_hosts_from_store(cfg, actor),
    )
}

/// Admit this process's configuration once and publish generation 1.
///
/// Must run after `profile::reroot` (the capture binds the rerooted profile
/// roots) and before any DB consumer, provider, agent or sandbox launch. A
/// second call (e.g. `open` falling through to the interactive launch)
/// returns the already-published current generation.
pub(crate) fn admit_process(
    inputs: ProcessInputs<'_>,
) -> Result<Arc<AdmittedConfig>, CaptureFailure> {
    if let Some(process) = PROCESS.get() {
        return process.store.current().map_err(|_| {
            CaptureFailure::Admission(
                thegn_core::config_admission::ConfigAdmissionError::TransientIo,
            )
        });
    }
    let seed = ConfigCaptureSeed::capture_process(CapturedCliInputs {
        config: inputs.config,
        profile: inputs.profile,
        profile_paths: None,
        overrides: inputs.overrides,
    })?;
    let (process, published) =
        ProcessAdmission::admit_initial(seed, inputs.actor, &production_admit)?;
    // Startup is single-threaded here; a lost race would mean two admissions
    // in one process, which the early return above already prevents.
    let _ = PROCESS.set(process); // best-effort: first-set-wins: only one startup admission runs per process
    published.config().install_admitted_runtime();
    // Advisory (deprecation/compatibility) diagnostics: already bounded and
    // redacted by admission; errors never reach here.
    for diagnostic in published.trace().diagnostics() {
        thegn_core::config::config_warn(&diagnostic.message);
    }
    Ok(published)
}

/// The process store, once startup admission has published.
pub(crate) fn store() -> Option<&'static AdmissionStore> {
    PROCESS.get().map(ProcessAdmission::store)
}

/// The latest admitted snapshot for display/projection consumers (hydration,
/// status). A degraded-but-retained generation is included on purpose; new
/// authority must go through [`AdmissionStore::authorize`] / `current`.
pub(crate) fn display() -> Option<Arc<AdmittedConfig>> {
    store().and_then(AdmissionStore::display)
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

/// The typed admission category a capture failure is recorded under.
fn failure_category(
    failure: &CaptureFailure,
) -> thegn_core::config_admission::ConfigAdmissionError {
    use crate::config_capture::ConfigFileReadError as Read;
    use thegn_core::config_admission::ConfigAdmissionError as E;
    match failure {
        CaptureFailure::Admission(error) => error.clone(),
        CaptureFailure::Source(Read::TooLarge) => E::Oversized,
        CaptureFailure::Source(Read::Changed) => E::TransientIo,
        CaptureFailure::Source(_) => E::Unreadable,
        CaptureFailure::Input(_) => E::EnvironmentInvalid,
        CaptureFailure::State(_) | CaptureFailure::Hosts(_) => E::HostInvalid,
    }
}

#[cfg(test)]
#[path = "config_startup_tests.rs"]
mod tests;
