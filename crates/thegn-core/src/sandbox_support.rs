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
    /// A caveat that is **not** about state, so it can accompany even a `Ready`
    /// row — today only [`Backend::verified`]. Kept separate from `remedy`
    /// because "we never checked this runtime's verbs" is orthogonal to
    /// installed/running: an unverified backend can be perfectly installed and
    /// still fail every pane, and folding the two would either hide the caveat
    /// on a `Ready` row or lose the remedy on a stopped one.
    pub caveat: Option<String>,
}

/// The concrete next action for a backend in `state`, when there is one.
/// Public so `thegn doctor`'s sandbox probes print the same remedy the
/// support report shows (and so the OS-specific wording, which a user
/// follows verbatim, can be asserted in tests).
pub fn remedy(backend: Backend, state: BackendState) -> Option<String> {
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
                caveat: caveat_for(backend, state),
            })
        })
        .collect()
}

/// The caveat for `backend`, which unlike [`remedy_for`] can accompany a `Ready`
/// row.
///
/// Says plainly what `ready` does and does not mean for an unverified runtime:
/// it was reached by a PATH probe, because thegn has no liveness verb for it, so
/// it reports that the binary exists and nothing more.
///
/// Suppressed for `Unsupported`, which is the stronger and completely different
/// statement: that backend cannot run on this OS however it is configured, so
/// whether thegn's verbs for it were ever tested is irrelevant. Printing both
/// would put two unrelated reasons under one row and bury the one that decides
/// it — `wsl` on a Mac is not a verification problem.
pub fn caveat_for(backend: Backend, state: BackendState) -> Option<String> {
    (!backend.verified() && state != BackendState::Unsupported).then(|| {
        format!(
            "unverified: no liveness check, and thegn's `{}` verbs were never tested against the \
             real runtime — `ready` means on PATH, not working",
            backend.binary()
        )
    })
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

    #[test]
    fn only_the_unverified_backends_carry_a_caveat() {
        // The ratchet that matters. `verified()` is a hand-maintained list, so
        // the risk is not that smol/wsl lose their caveat — it is that a real
        // backend silently GAINS one and starts telling users their working
        // sandbox is untrustworthy. Assert the exact set, both directions.
        let unverified: Vec<Backend> = [
            Backend::Podman,
            Backend::PodmanRootful,
            Backend::Docker,
            Backend::Smol,
            Backend::Bwrap,
            Backend::Systemd,
            Backend::Apple,
            Backend::Wsl,
            Backend::WinAppContainer,
            Backend::WinJobObject,
            Backend::None,
        ]
        .into_iter()
        .filter(|b| !b.verified())
        .collect();
        assert_eq!(unverified, vec![Backend::Smol, Backend::Wsl]);

        for b in [Backend::Smol, Backend::Wsl] {
            let c = caveat_for(b, BackendState::Ready).unwrap_or_default();
            assert!(c.contains("unverified"), "{b:?}: {c}");
            // It must say what `ready` actually means, or the row still implies
            // a guarantee — that was the whole defect.
            assert!(c.contains("PATH"), "{b:?}: {c}");
            // And point at the reason: there is no liveness verb for it.
            assert!(c.contains("liveness"), "{b:?}: {c}");
            // But NOT when the backend cannot run here at all: `wsl` on a Mac is
            // decided by the OS, and a verification note under that row would
            // bury the reason that actually applies.
            assert_eq!(caveat_for(b, BackendState::Unsupported), None, "{b:?}");
        }
        for b in [Backend::Podman, Backend::None, Backend::Apple] {
            for st in [
                BackendState::Ready,
                BackendState::NotRunning,
                BackendState::NotInstalled,
            ] {
                assert_eq!(caveat_for(b, st), None, "{b:?} {st:?}");
            }
        }
    }

    #[test]
    fn an_unverified_backend_keeps_both_its_remedy_and_its_caveat() {
        // The reason `caveat` is a separate field: an unverified backend can
        // also be stopped or missing. Folding it into `remedy` would drop one of
        // the two, and which one it dropped would depend on the state.
        let rows = support_report(&["smol".to_string()], &Placement::Local, None);
        let row = &rows[0];
        assert!(row.caveat.is_some(), "caveat is independent of state");
        if row.state != BackendState::Ready {
            assert!(
                row.remedy.is_some(),
                "a non-ready row still says how to fix it: {:?}",
                row.state
            );
        }
    }

    #[test]
    fn an_unverified_backend_is_never_reached_by_default() {
        // The caveat is honest only because nothing lands here by accident: it
        // is always something the user named. If one ever enters the default
        // chain, "you selected it explicitly" becomes a lie.
        let chain = crate::config_defaults::default_backend_chain();
        for b in [Backend::Smol, Backend::Wsl] {
            assert!(
                !chain.iter().any(|n| Backend::parse(n) == Some(b)),
                "{b:?} must not be in the default chain: {chain:?}"
            );
        }
    }
}
