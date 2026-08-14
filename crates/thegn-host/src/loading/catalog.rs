//! Backend-aware loading-step plans + slow-step hints.
//!
//! Which steps a worktree bring-up actually has depends on the resolved
//! backend/placement combo: a host shell has no container step, an OCI
//! sandbox may pull/build an image and join a VPN sidecar, a remote worktree
//! connects first. [`plan_for`] is the one table mapping a resolved target to
//! its step plan, replacing the historical hardcoded
//! `["sandbox", "container", "shell"]` — and every plan it emits ends in the
//! `"shell"` step, preserving the `is_shell_wait` arbitration contract
//! (locked by tests here and in `loading/mod.rs`).
//!
//! [`slow_hint`] is the companion table: per-[`StepKind`] elapsed thresholds
//! after which the splash shows a "this is expected, here's why" sub-line
//! under the active step. Evaluated at draw time from the step's tracked
//! `started_at` — no state, no extra messages.

use std::time::Duration;

use crate::chrome::{StepKind, StepState};
use crate::loading::plan::LoadPlan;

/// The label of the terminal shell-attach step. `loading::is_shell_wait`
/// gates the splash-clear / watchdog machinery on a step list ENDING in this
/// exact label; every plan below closes with it.
pub(crate) const SHELL_LABEL: &str = "shell";

/// How the resolved backend runs the worktree's process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BackendClass {
    /// Bare host shell (`Backend::None`).
    Host,
    /// Host-toolchain namespace sandbox (bwrap / systemd-run / win-native);
    /// the label is the backend's display name.
    HostToolchain(String),
    /// OCI container runtime (podman/docker/…); the label is the runtime name.
    Oci(String),
}

/// Where the worktree's process runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteClass {
    Local,
    /// Remote placement; the label is a short host descriptor (`ssh:host`).
    /// Not constructed on the seed path today — remote flows stream their own
    /// provisioner plans — but part of the catalog's per-combo contract
    /// (locked by the shape tests) for producers that resolve a host early.
    #[allow(dead_code)]
    Remote(String),
}

/// A resolved bring-up target: everything [`plan_for`] needs to shape the
/// step plan. Optional phases appear only when their flag holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedTarget {
    pub backend: BackendClass,
    pub remote: RemoteClass,
    /// OCI image ref, when known (labels the image step).
    pub image: Option<String>,
    /// A Dockerfile/devcontainer build precedes container create.
    pub needs_build: bool,
    /// A VPN sidecar joins the container's netns.
    pub vpn: bool,
    /// An sshfs/sync projection mounts before the shell (remote host shell).
    pub mount: bool,
    /// An environment wrap (direnv/devenv/devShell) warms before the shell;
    /// the label names it.
    pub env_wrap: Option<String>,
}

impl ResolvedTarget {
    /// A local bare-host target — the shape of the most common tab.
    #[cfg(test)]
    pub(crate) fn host_local() -> Self {
        Self {
            backend: BackendClass::Host,
            remote: RemoteClass::Local,
            image: None,
            needs_build: false,
            vpn: false,
            mount: false,
            env_wrap: None,
        }
    }
}

/// A best-effort resolved target for the materialize SEED, derived purely
/// from config + worktree remoteness (no probing, no DB — this runs on the
/// event loop). `None` when the target can't be classified cheaply (remote
/// flows stream their own plans; `backend = "auto"` resolves off-thread) —
/// the caller then seeds the generic three-step shape and lets the observer
/// refine it.
///
/// `override_backend` is the worktree's saved `sandbox_backend` (the DB column,
/// carried on the loop via `model.active_sandbox_backend` — no read here). A
/// valid, non-`auto` override wins over the global `[sandbox] backend`, so an
/// uncontained worktree (`none`) seeds the host plan even under a sandboxed
/// default, and a pinned worktree seeds its own backend. Empty/`auto`/unknown
/// falls back to config.
pub(crate) fn seed_target(
    cfg: &thegn_core::config::Config,
    remote: bool,
    override_backend: Option<&str>,
) -> Option<ResolvedTarget> {
    if remote {
        return None;
    }
    let host = ResolvedTarget {
        backend: BackendClass::Host,
        remote: RemoteClass::Local,
        image: None,
        needs_build: false,
        vpn: false,
        mount: false,
        env_wrap: None,
    };
    if !cfg.sandbox.enabled {
        return Some(host);
    }
    use thegn_core::sandbox::Backend as B;
    // Prefer a valid, non-`auto` per-worktree override over the global default;
    // an empty/`auto`/unparseable value falls back to config.
    let effective = override_backend
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "auto")
        .and_then(|s| thegn_core::config::SandboxBackend::from_str_validated(s).ok())
        .unwrap_or(cfg.sandbox.backend);
    // `auto` (None) means the chain resolves off-thread — unknowable here.
    let b = B::from_config(effective)?;
    let backend = match b {
        B::None => BackendClass::Host,
        B::Bwrap | B::Systemd | B::WinAppContainer | B::WinJobObject => {
            BackendClass::HostToolchain(b.label().into())
        }
        _ => BackendClass::Oci(b.label().into()),
    };
    Some(ResolvedTarget {
        image: (matches!(backend, BackendClass::Oci(_)) && !cfg.sandbox.image.is_empty())
            .then(|| cfg.sandbox.image.clone()),
        backend,
        ..host
    })
}

/// The generic materialize seed for an unclassifiable target (`auto` backend,
/// remote flows): the historical `[sandbox, container, shell]` shape, now
/// kind-tagged so the observer's events refine these rows in place instead of
/// stacking duplicates.
pub(crate) fn generic_seed() -> Vec<crate::chrome::LoadStep> {
    LoadPlan::new()
        .step_kinded("sandbox", StepState::Active, StepKind::Resolve)
        .step_kinded("container", StepState::Pending, StepKind::Create)
        .step_kinded(SHELL_LABEL, StepState::Pending, StepKind::Shell)
        .into_steps()
}

/// The step plan for a resolved target: first step active, the rest pending,
/// always ending in the [`SHELL_LABEL`] step. This is the single source of
/// truth for "which steps does this host/sandbox combo show".
pub(crate) fn plan_for(t: &ResolvedTarget) -> LoadPlan {
    let mut steps: Vec<(String, StepKind)> = Vec::new();
    if let RemoteClass::Remote(host) = &t.remote {
        steps.push((format!("connect {host}"), StepKind::Connect));
    }
    match &t.backend {
        BackendClass::Host => {
            if matches!(t.remote, RemoteClass::Local) {
                steps.push(("sandbox".into(), StepKind::Resolve));
            }
            if t.mount {
                steps.push(("mount workdir".into(), StepKind::Mount));
            }
        }
        BackendClass::HostToolchain(label) => {
            steps.push(("sandbox".into(), StepKind::Resolve));
            steps.push((format!("namespace ({label})"), StepKind::Create));
        }
        BackendClass::Oci(runtime) => {
            if matches!(t.remote, RemoteClass::Local) {
                steps.push(("sandbox".into(), StepKind::Resolve));
            }
            let image = match &t.image {
                Some(img) => format!("image {img}"),
                None => "image".into(),
            };
            steps.push((image, StepKind::Image));
            if t.needs_build {
                steps.push(("build image".into(), StepKind::Build));
            }
            steps.push((format!("container ({runtime})"), StepKind::Create));
            if t.vpn {
                steps.push(("vpn sidecar".into(), StepKind::Vpn));
            }
        }
    }
    if let Some(wrap) = &t.env_wrap {
        steps.push((format!("env ({wrap})"), StepKind::Env));
    }
    steps.push((SHELL_LABEL.into(), StepKind::Shell));

    let mut plan = LoadPlan::new();
    for (i, (label, kind)) in steps.into_iter().enumerate() {
        let state = if i == 0 {
            StepState::Active
        } else {
            StepState::Pending
        };
        plan = plan.step_kinded(label, state, kind);
    }
    plan
}

/// The "this is taking a while, and that's (probably) fine" hint for an
/// active step of `kind` that has been running for `elapsed`. `None` below
/// the step-kind's first threshold or for kinds with nothing useful to say.
/// The splash renders this as the active step's sub-line when the producer
/// gave no more specific `detail`.
///
/// Each kind maps to an ASCENDING list of `(threshold_secs, hint)` tiers: the
/// highest tier whose threshold is met wins, so a stalled step ESCALATES its
/// message the longer it hangs — a reassuring "this is expected" early, then a
/// "this looks wedged; press Esc to cancel" once it crosses into
/// unusual-latency territory (which is what the frozen 5-minute `sandbox`
/// spinner needed). Sampled at draw time from the step's `started_at`; no state.
pub(crate) fn slow_hint(kind: StepKind, elapsed: Duration) -> Option<&'static str> {
    let tiers: &[(u64, &'static str)] = match kind {
        StepKind::Resolve => &[
            (
                8,
                "resolving the sandbox backend — probing container runtimes",
            ),
            (45, "runtime slow to answer — press Esc to run on host"),
        ],
        StepKind::Image => &[
            (
                15,
                "network-bound — a cold image pull can take a couple of minutes",
            ),
            (150, "pull unusually slow — press Esc to run on host"),
        ],
        StepKind::Build => &[
            (30, "first Dockerfile build can take several minutes"),
            (300, "build unusually slow — press Esc to run on host"),
        ],
        StepKind::Create => &[
            (10, "container runtime is slow to answer — it may be wedged"),
            (45, "runtime looks wedged — press Esc to run on host"),
        ],
        StepKind::Connect => &[
            (
                10,
                "host is slow to answer — transient failures are retried",
            ),
            (60, "host not answering — press Esc to run on host"),
        ],
        StepKind::Vpn => &[(15, "waiting for the VPN sidecar to come up")],
        StepKind::Env => &[
            (
                20,
                "building the dev environment — a cold cache can take minutes",
            ),
            (240, "env build unusually slow — press Esc to run on host"),
        ],
        StepKind::Provision => &[
            (30, "cold sandbox boot can take a couple of minutes"),
            (300, "sandbox boot slow — press Esc to run on host"),
        ],
        StepKind::Shell => &[
            (5, "waiting for the login shell — rc files may be slow"),
            (30, "shell not answering — press Esc to run on host"),
        ],
        StepKind::Mount | StepKind::Other => return None,
    };
    tiers
        .iter()
        .rev()
        .find(|(threshold_secs, _)| elapsed >= Duration::from_secs(*threshold_secs))
        .map(|(_, hint)| *hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::LoadStep;

    fn labels(steps: &[LoadStep]) -> Vec<&str> {
        steps.iter().map(|s| s.label.as_str()).collect()
    }

    fn assert_plan_invariants(steps: &[LoadStep]) {
        // The catalog rule that keeps `is_shell_wait` sound: exactly one
        // "shell" step, and it is last.
        let shells = steps.iter().filter(|s| s.label == SHELL_LABEL).count();
        assert_eq!(shells, 1, "exactly one shell step: {:?}", labels(steps));
        assert_eq!(steps.last().unwrap().label, SHELL_LABEL, "shell is last");
        assert!(crate::loading::is_shell_wait(steps));
        // First step active, the rest pending.
        assert_eq!(steps[0].state, StepState::Active);
        assert!(steps[1..].iter().all(|s| s.state == StepState::Pending));
    }

    #[test]
    fn host_local_plan_has_no_container_step() {
        let steps = plan_for(&ResolvedTarget::host_local()).into_steps();
        assert_eq!(labels(&steps), vec!["sandbox", "shell"]);
        assert_plan_invariants(&steps);
    }

    #[test]
    fn host_toolchain_plan_creates_a_namespace() {
        let t = ResolvedTarget {
            backend: BackendClass::HostToolchain("bwrap".into()),
            ..ResolvedTarget::host_local()
        };
        let steps = plan_for(&t).into_steps();
        assert_eq!(
            labels(&steps),
            vec!["sandbox", "namespace (bwrap)", "shell"]
        );
        assert_plan_invariants(&steps);
        assert_eq!(steps[1].kind, StepKind::Create);
    }

    #[test]
    fn oci_local_plan_full_shape() {
        let t = ResolvedTarget {
            backend: BackendClass::Oci("podman".into()),
            image: Some("debian:stable".into()),
            needs_build: true,
            vpn: true,
            env_wrap: Some("direnv".into()),
            ..ResolvedTarget::host_local()
        };
        let steps = plan_for(&t).into_steps();
        assert_eq!(
            labels(&steps),
            vec![
                "sandbox",
                "image debian:stable",
                "build image",
                "container (podman)",
                "vpn sidecar",
                "env (direnv)",
                "shell",
            ]
        );
        assert_plan_invariants(&steps);
    }

    #[test]
    fn oci_remote_plan_connects_first_and_skips_local_resolve() {
        let t = ResolvedTarget {
            backend: BackendClass::Oci("podman".into()),
            remote: RemoteClass::Remote("ssh:gpu-box".into()),
            image: Some("dev:latest".into()),
            ..ResolvedTarget::host_local()
        };
        let steps = plan_for(&t).into_steps();
        assert_eq!(
            labels(&steps),
            vec![
                "connect ssh:gpu-box",
                "image dev:latest",
                "container (podman)",
                "shell",
            ]
        );
        assert_plan_invariants(&steps);
        assert_eq!(steps[0].kind, StepKind::Connect);
    }

    #[test]
    fn remote_host_shell_plan_connects_and_mounts() {
        let t = ResolvedTarget {
            remote: RemoteClass::Remote("ssh:box".into()),
            mount: true,
            ..ResolvedTarget::host_local()
        };
        let steps = plan_for(&t).into_steps();
        assert_eq!(
            labels(&steps),
            vec!["connect ssh:box", "mount workdir", "shell"]
        );
        assert_plan_invariants(&steps);
    }

    #[test]
    fn every_combo_ends_in_exactly_one_shell() {
        // Sweep the flag space; the invariant must hold for every combination.
        for backend in [
            BackendClass::Host,
            BackendClass::HostToolchain("bwrap".into()),
            BackendClass::Oci("docker".into()),
        ] {
            for remote in [RemoteClass::Local, RemoteClass::Remote("ssh:h".into())] {
                for (needs_build, vpn, mount, env_wrap) in [
                    (false, false, false, None),
                    (true, true, true, Some("devenv".to_string())),
                ] {
                    let t = ResolvedTarget {
                        backend: backend.clone(),
                        remote: remote.clone(),
                        image: Some("img".into()),
                        needs_build,
                        vpn,
                        mount,
                        env_wrap,
                    };
                    assert_plan_invariants(&plan_for(&t).into_steps());
                }
            }
        }
    }

    #[test]
    fn seed_target_classifies_cheaply_or_declines() {
        let mut cfg = thegn_core::config::Config::default();
        // Remote flows stream their own plans: no seed classification.
        assert_eq!(seed_target(&cfg, true, None), None);
        // Sandboxing off ⇒ host shell.
        cfg.sandbox.enabled = false;
        assert_eq!(
            seed_target(&cfg, false, None).unwrap().backend,
            BackendClass::Host
        );
        // `auto` can't be classified without probing ⇒ generic seed.
        cfg.sandbox.enabled = true;
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        assert_eq!(seed_target(&cfg, false, None), None);
        // An explicit OCI backend seeds the rich plan, with the image ref.
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Podman;
        cfg.sandbox.image = "debian:stable".into();
        let t = seed_target(&cfg, false, None).unwrap();
        assert_eq!(t.backend, BackendClass::Oci("podman-rootless".into()));
        assert_eq!(t.image.as_deref(), Some("debian:stable"));
        let labels: Vec<String> = plan_for(&t)
            .into_steps()
            .iter()
            .map(|s| s.label.clone())
            .collect();
        assert_eq!(
            labels,
            vec![
                "sandbox",
                "image debian:stable",
                "container (podman-rootless)",
                "shell"
            ]
        );
        // Bwrap classifies as a host-toolchain namespace.
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Bwrap;
        assert_eq!(
            seed_target(&cfg, false, None).unwrap().backend,
            BackendClass::HostToolchain("bwrap".into())
        );
    }

    #[test]
    fn seed_target_honors_per_worktree_override() {
        let mut cfg = thegn_core::config::Config::default();
        cfg.sandbox.enabled = true;
        // Global default is a host-toolchain sandbox…
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Bwrap;
        // …but an uncontained worktree (`none`) seeds the bare host plan —
        // no namespace/container step. This is the regression under fix.
        let t = seed_target(&cfg, false, Some("none")).unwrap();
        assert_eq!(t.backend, BackendClass::Host);
        assert_eq!(labels(&plan_for(&t).into_steps()), vec!["sandbox", "shell"]);
        // `host` alias resolves the same way.
        assert_eq!(
            seed_target(&cfg, false, Some("host")).unwrap().backend,
            BackendClass::Host
        );
        // A pinned backend wins even when the global config is `auto`
        // (which alone would decline to the generic seed).
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        assert_eq!(
            seed_target(&cfg, false, Some("podman")).unwrap().backend,
            BackendClass::Oci("podman-rootless".into())
        );
        // Empty / `auto` / unparseable override ⇒ fall back to config (`auto`
        // ⇒ generic seed).
        assert_eq!(seed_target(&cfg, false, Some("")), None);
        assert_eq!(seed_target(&cfg, false, Some("auto")), None);
        assert_eq!(seed_target(&cfg, false, Some("nonsense")), None);
    }

    #[test]
    fn slow_hint_thresholds() {
        use std::time::Duration as D;
        // Below threshold: quiet. At/above: the hint.
        assert_eq!(slow_hint(StepKind::Image, D::from_secs(14)), None);
        assert!(slow_hint(StepKind::Image, D::from_secs(15)).is_some());
        assert_eq!(slow_hint(StepKind::Create, D::from_secs(9)), None);
        assert!(slow_hint(StepKind::Create, D::from_secs(10)).is_some());
        assert!(slow_hint(StepKind::Shell, D::from_secs(5)).is_some());
        assert!(slow_hint(StepKind::Connect, D::from_secs(10)).is_some());
        assert!(slow_hint(StepKind::Build, D::from_secs(30)).is_some());
        assert!(slow_hint(StepKind::Vpn, D::from_secs(15)).is_some());
        assert!(slow_hint(StepKind::Env, D::from_secs(20)).is_some());
        assert!(slow_hint(StepKind::Provision, D::from_secs(30)).is_some());
        // The Resolve step (the frozen "sandbox" spinner) now explains itself.
        assert_eq!(slow_hint(StepKind::Resolve, D::from_secs(7)), None);
        assert!(slow_hint(StepKind::Resolve, D::from_secs(8)).is_some());
        // Kinds with nothing useful to say stay quiet forever.
        assert_eq!(slow_hint(StepKind::Other, D::from_secs(600)), None);
        assert_eq!(slow_hint(StepKind::Mount, D::from_secs(600)), None);
    }

    #[test]
    fn slow_hint_escalates_and_offers_an_escape() {
        use std::time::Duration as D;
        // Early tier: reassuring. Late tier: names a likely wedge + the Esc
        // affordance. The highest crossed threshold wins.
        let early = slow_hint(StepKind::Resolve, D::from_secs(10)).unwrap();
        let late = slow_hint(StepKind::Resolve, D::from_secs(60)).unwrap();
        assert_ne!(early, late, "the message escalates past the second tier");
        assert!(!early.contains("Esc"), "early hint is reassuring: {early}");
        assert!(late.contains("Esc"), "late hint offers a way out: {late}");
        // Every kind that can genuinely hang gets an Esc escape at its top tier.
        for (kind, secs) in [
            (StepKind::Resolve, 45),
            (StepKind::Image, 150),
            (StepKind::Create, 45),
            (StepKind::Shell, 30),
            (StepKind::Connect, 60),
        ] {
            assert!(
                slow_hint(kind, D::from_secs(secs)).unwrap().contains("Esc"),
                "{kind:?} top tier offers cancel"
            );
        }
    }
}
