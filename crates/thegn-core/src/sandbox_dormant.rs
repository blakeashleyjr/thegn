//! **A stopped runtime is a question, not a silent downgrade.**
//!
//! `sandbox_support::BackendState` already separates `NotRunning` (a client is
//! installed, its daemon/VM is not answering) from `NotInstalled`. Selection
//! folds both into "absent" and walks on, so a Mac with podman installed but no
//! `podman machine` running — or a Linux box with `dockerd` stopped — silently
//! opened a host shell with no kernel boundary. The distinction was surfaced
//! only in the onboarding wizard, which is the one moment the user is NOT trying
//! to launch anything.
//!
//! This module supplies the two things the launch path needs to turn that into a
//! choice: [`start_argv`] (the command that would actually start each runtime,
//! as opposed to `sandbox_support::remedy_for`'s human sentence) and
//! [`decide`] (what to do about it, given `[sandbox] on_dormant`). Both are
//! pure — the probing and the spawning belong to the caller, off the event loop.

use crate::config::OnDormant;
use crate::sandbox::Backend;
use crate::sandbox_backend::HostOs;
use crate::sandbox_support::{BackendState, BackendSupport};

/// What the launch path should do about a dormant runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DormantAction {
    /// Nothing is dormant, or policy says don't interfere: carry on (which may
    /// mean a truthfully-labelled host shell).
    Proceed,
    /// Start the runtime, then re-probe and re-resolve.
    Start(DormantRuntime),
    /// Put the choice to the user: start / run on host / cancel.
    Ask(DormantRuntime),
    /// Refuse to launch rather than run uncontained.
    Cancel(DormantRuntime),
}

/// A runtime that is installed but not running, with the command that starts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DormantRuntime {
    pub backend: Backend,
    /// Config-facing name, as written in `backend_chain` (e.g. `podman-rootless`).
    pub name: String,
    /// The human sentence (`sandbox_support::remedy_for`), for display.
    pub remedy: String,
    /// The argv that starts it, when one is known. `None` ⇒ the user must do it
    /// themselves (rootful podman needs a password we will not prompt for).
    pub start_argv: Option<Vec<String>>,
}

impl DormantRuntime {
    /// The start command as a display string, for a menu row or a status line.
    pub fn start_display(&self) -> Option<String> {
        self.start_argv.as_ref().map(|a| a.join(" "))
    }
}

/// The command that starts `backend` on `os`, or `None` when there isn't one we
/// can run unattended.
///
/// `have` answers "is this binary on PATH?" — passed in rather than probed here
/// so the mapping stays pure and testable. macOS Docker is the interesting case:
/// there is no `dockerd` to start, so the runtime is whichever VM the user
/// installed — colima if present, else Docker Desktop, which is a GUI app and
/// must be launched with `open`. Getting this wrong is worse than saying
/// nothing, so an unknown combination returns `None` and the user is shown the
/// remedy sentence instead.
pub fn start_argv(
    backend: Backend,
    os: HostOs,
    have: &dyn Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let v = |parts: &[&str]| Some(parts.iter().map(|s| s.to_string()).collect());
    match (backend, os) {
        // `podman machine start` is the same verb on every desktop OS; on Linux
        // rootless podman needs no machine, so a "not running" there is a user
        // service (`systemctl --user start podman.socket`).
        (Backend::Podman, HostOs::Linux) => v(&["systemctl", "--user", "start", "podman.socket"]),
        (Backend::Podman, _) => v(&["podman", "machine", "start"]),
        (Backend::Docker, HostOs::MacOs) => {
            if have("colima") {
                v(&["colima", "start"])
            } else {
                // Docker Desktop: a GUI app, so `open -a` rather than a daemon verb.
                v(&["open", "-a", "Docker"])
            }
        }
        (Backend::Docker, HostOs::Linux) => v(&["systemctl", "start", "docker"]),
        (Backend::Apple, HostOs::MacOs) => v(&["container", "system", "start"]),
        // Rootful podman needs a password; we will not drive an interactive sudo
        // from a launch path, so this stays the user's job.
        (Backend::PodmanRootful, _) => None,
        _ => None,
    }
}

/// The first runtime in the report that is installed but not running — the one
/// worth offering, since the chain would have taken it had it been up.
pub fn first_dormant(report: &[BackendSupport]) -> Option<&BackendSupport> {
    report.iter().find(|r| r.state == BackendState::NotRunning)
}

/// Build the offer for a dormant backend.
pub fn runtime_for(
    support: &BackendSupport,
    os: HostOs,
    have: &dyn Fn(&str) -> bool,
) -> DormantRuntime {
    DormantRuntime {
        backend: support.backend,
        name: support.name.clone(),
        remedy: support.remedy.clone().unwrap_or_default(),
        start_argv: start_argv(support.backend, os, have),
    }
}

/// What to do when a launch is about to degrade to the host.
///
/// `wanted_containment` is whether this launch actually asked for a sandbox: an
/// `auto`/`host` launch landing on the host is the configured outcome, not a
/// degradation, and must never raise a prompt. `dormant` is the offer, if any —
/// with nothing dormant there is nothing to start, so even `cancel` proceeds
/// (refusing to launch would strand a user whose runtime simply isn't installed).
pub fn decide(
    policy: OnDormant,
    wanted_containment: bool,
    dormant: Option<DormantRuntime>,
) -> DormantAction {
    let Some(rt) = dormant else {
        return DormantAction::Proceed;
    };
    if !wanted_containment {
        return DormantAction::Proceed;
    }
    match policy {
        OnDormant::Host => DormantAction::Proceed,
        OnDormant::Cancel => DormantAction::Cancel(rt),
        // Nothing to run unattended ⇒ downgrade `start` to `ask`, so the user is
        // told what to do rather than silently getting the host anyway.
        OnDormant::Start if rt.start_argv.is_some() => DormantAction::Start(rt),
        OnDormant::Start | OnDormant::Ask => DormantAction::Ask(rt),
    }
}

#[cfg(test)]
#[path = "sandbox_dormant_tests.rs"]
mod sandbox_dormant_tests;
