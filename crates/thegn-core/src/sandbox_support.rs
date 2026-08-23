//! Per-backend **support report** — the human-facing answer to "why am I not
//! sandboxed?", shared by `thegn doctor` and the onboarding wizard.
//!
//! Selection (`sandbox_backend::pick_backend`) only ever needs one bit
//! per backend: usable or not. A person needs more, because the remedies are
//! completely different — "not installed" means install something, "installed but
//! not running" means start a service you already have, and "unsupported here"
//! means stop expecting it on this OS. Collapsing those three into a bare
//! "present: false" (what both surfaces showed before) is what left a first-time
//! macOS user with a pane that died repeatedly and no way to find out why.
//!
//! The distinction lives ONLY here. Selection keeps folding installed-but-down
//! into `RuntimeProbe::Absent`, so the chain stays a two-state walk and this
//! module stays a pure presentation layer over the same probe cache.

use crate::capabilities::{Capabilities, IsolationClass};
use crate::placement::{Placement, RuntimeProbe};
use crate::sandbox::Backend;
use crate::sandbox_backend::{
    HostOs, available, backend_installed_locally, backend_runs_on, backend_suitable_on, host_os,
};

/// Why a backend is or isn't usable here, richest-to-poorest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendState {
    /// Usable right now.
    Ready,
    /// Its client is installed, but the daemon/service it needs isn't answering
    /// (stopped `dockerd`, Docker Desktop quit, Apple's `container` services not
    /// started). The case that used to masquerade as `Ready`.
    NotRunning,
    /// Supported on this OS, but nothing is installed.
    NotInstalled,
    /// Cannot run on this OS at all, however it is configured.
    Unsupported,
    /// A remote placement we could not reach, so its runtimes are unknown.
    Unreachable,
}

impl BackendState {
    /// Whether a pane could actually be sandboxed by this backend right now.
    pub fn usable(self) -> bool {
        self == BackendState::Ready
    }
}

/// One row of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSupport {
    pub backend: Backend,
    /// The config-facing name, as written in `backend_chain`.
    pub name: String,
    pub state: BackendState,
    /// What isolation it *would* give if it were usable — so a user can weigh
    /// starting a stopped runtime against staying on the host.
    pub isolation: Option<IsolationClass>,
    /// The concrete next action, when there is one.
    pub remedy: Option<String>,
}

/// How to make `backend` usable, for the states where the user can do something.
/// Test-only window onto [`remedy_for`], so the OS-specific wording (which a
/// user follows verbatim) can be asserted without standing up a full report.
#[cfg(test)]
pub(crate) fn remedy_for_test(backend: Backend, state: BackendState) -> Option<String> {
    remedy_for(backend, state)
}

fn remedy_for(backend: Backend, state: BackendState) -> Option<String> {
    match (backend, state) {
        (_, BackendState::Ready) | (_, BackendState::Unsupported) => None,
        (Backend::Apple, BackendState::NotRunning) => {
            Some("start it with `container system start`".into())
        }
        // macOS has no `dockerd` to start — the runtime is whichever VM is
        // installed, and colima is the common one. Naming systemd there sent Mac
        // users looking for a service that does not exist.
        (Backend::Docker, BackendState::NotRunning) => Some(match host_os() {
            HostOs::MacOs => "start it with `colima start`, or open Docker Desktop".into(),
            HostOs::Windows => "start Docker Desktop".into(),
            _ => "start Docker (`systemctl start docker`, or Docker Desktop)".to_string(),
        }),
        (Backend::Podman, BackendState::NotRunning) => {
            Some("start it with `podman machine start`".into())
        }
        (Backend::PodmanRootful, BackendState::NotRunning) => {
            Some("needs passwordless `sudo podman` (`sudo -n podman version`)".into())
        }
        (_, BackendState::NotRunning) => Some("its service is not responding".into()),
        (Backend::Apple, BackendState::NotInstalled) => {
            Some("install Apple's `container` CLI".into())
        }
        (b, BackendState::NotInstalled) => Some(format!("install `{}`", b.binary())),
        (_, BackendState::Unreachable) => Some("host unreachable — check the connection".into()),
    }
}

/// Classify one backend for `placement`.
///
/// Ordering matters: OS support is checked before installation, so a Linux box
/// that happens to ship an unrelated `container` binary reports `Unsupported`
/// rather than teasing the user with a macOS-only backend.
pub fn classify(backend: Backend, placement: &Placement, os: HostOs) -> BackendState {
    if backend == Backend::None {
        return BackendState::Ready; // the host shell is always available
    }
    if placement.is_local() && !backend_runs_on(backend, os) {
        return BackendState::Unsupported;
    }
    if !backend_suitable_on(backend, placement, os) {
        return BackendState::Unsupported;
    }
    match available(placement, backend) {
        RuntimeProbe::Present => BackendState::Ready,
        RuntimeProbe::Unreachable => BackendState::Unreachable,
        // The split that selection throws away. Win-native backends have no
        // binary to find (they are OS APIs), so "absent" there is never
        // "not installed" — it can only mean the wrong OS, already handled above.
        RuntimeProbe::Absent if backend.binary().is_empty() => BackendState::Unsupported,
        RuntimeProbe::Absent if backend_installed_locally(backend) => BackendState::NotRunning,
        RuntimeProbe::Absent => BackendState::NotInstalled,
    }
}

/// The full report for `chain`, in chain order, plus the isolation each backend
/// would provide. Rides the probe cache, so this is cheap once the resolver has
/// run — and re-runnable: call it again after starting a runtime and the row
/// flips, because a `Ready` answer is never stale and the caller can clear the
/// cache to re-ask a negative one.
pub fn support_report(
    chain: &[String],
    placement: &Placement,
    oci_runtime: Option<&str>,
) -> Vec<BackendSupport> {
    chain
        .iter()
        .filter_map(|name| {
            let backend = Backend::parse(name)?;
            let state = classify(backend, placement, host_os());
            Some(BackendSupport {
                backend,
                name: name.clone(),
                state,
                isolation: Some(
                    Capabilities::from_parts(backend, placement, false, oci_runtime).isolation,
                ),
                remedy: remedy_for(backend, state),
            })
        })
        .collect()
}

/// The first usable backend in the report, if any — what selection will land on.
pub fn first_ready(report: &[BackendSupport]) -> Option<&BackendSupport> {
    report.iter().find(|r| r.state.usable())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_support_is_decided_before_installation() {
        // `apple` on Linux is Unsupported even though a Linux box could well
        // have some unrelated `container` executable on PATH.
        assert_eq!(
            classify(Backend::Apple, &Placement::Local, HostOs::Linux),
            BackendState::Unsupported
        );
        // …and bwrap on macOS, which is what a Mac was probing every 30s.
        assert_eq!(
            classify(Backend::Bwrap, &Placement::Local, HostOs::MacOs),
            BackendState::Unsupported
        );
        assert_eq!(
            classify(Backend::WinJobObject, &Placement::Local, HostOs::Linux),
            BackendState::Unsupported
        );
    }

    #[test]
    fn host_backend_is_always_ready() {
        for os in [HostOs::Linux, HostOs::MacOs, HostOs::Windows] {
            assert_eq!(
                classify(Backend::None, &Placement::Local, os),
                BackendState::Ready,
                "the host shell has no prerequisites ({os:?})"
            );
        }
    }

    #[test]
    fn remedies_name_a_concrete_action_for_fixable_states() {
        // The two states a user can act on both produce an instruction…
        let down = remedy_for(Backend::Apple, BackendState::NotRunning).unwrap();
        assert!(down.contains("container system start"), "{down}");
        let missing = remedy_for(Backend::Docker, BackendState::NotInstalled).unwrap();
        assert!(missing.contains("docker"), "{missing}");
        assert!(
            remedy_for(Backend::Docker, BackendState::NotRunning)
                .unwrap()
                .to_lowercase()
                .contains("start")
        );
        // …and the two that cannot be acted on do not invent one.
        assert_eq!(remedy_for(Backend::Apple, BackendState::Ready), None);
        assert_eq!(remedy_for(Backend::Bwrap, BackendState::Unsupported), None);
    }

    #[test]
    fn report_follows_chain_order_and_skips_unknown_names() {
        let chain: Vec<String> = ["bwrap", "not-a-backend", "none"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let rows = support_report(&chain, &Placement::Local, None);
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["bwrap", "none"],
            "unparseable chain entries are dropped, order otherwise preserved"
        );
        assert!(first_ready(&rows).is_some(), "`none` is always ready");
    }
}
