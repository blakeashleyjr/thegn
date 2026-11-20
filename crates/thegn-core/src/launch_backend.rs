//! The launch's backend request and candidate walk, shared by launch and the Info screen (batch 178 lane 3).

use crate::config::{IsolationFloor, SandboxBackend, SandboxConfig};
use crate::env::Environment;
use crate::info_model::{BackendRow, Field, Source, keys};
use crate::placement::Placement;
use crate::sandbox::Backend;
use crate::seam::Kind as _;

#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub sandbox: SandboxConfig,
    pub explicit: Option<Backend>,
    pub from_pick: bool,
    pub host_toolchain_dropped: bool,
}

/// Resolve the backend choice that launch will use before the resolver walks it.
pub fn launch_request(
    sb: SandboxConfig,
    placement: &Placement,
    backend_choice: Option<&str>,
    choice_is_explicit: bool,
) -> LaunchRequest {
    let mut sb = sb;
    let mut explicit_backend = Backend::from_config(sb.backend).filter(|b| *b != Backend::None);
    // A fresh/explicit choice always wins over config; a non-explicit DB value
    // only overrides when config is "auto" (an explicit `backend = "bwrap"` must
    // beat a stale entry). An explicit choice may be "host"/"none" — keep those.
    // A saved backend is a LOCAL-session artifact (only the wizard writes it, for
    // a local choice), so it must not resurrect a backend on a remote/provider
    // placement — those default to `none` (the sandbox brings its own isolation).
    let mut from_pick = false;
    let config_is_auto = sb.backend == SandboxBackend::Auto;
    if placement.is_local()
        && let Some(saved) = backend_choice.map(str::trim)
        && !saved.is_empty()
        && (choice_is_explicit || (config_is_auto && saved != "auto"))
        && let Ok(b) = SandboxBackend::from_str_validated(saved)
    {
        explicit_backend = Backend::from_config(b).filter(|b| *b != Backend::None);
        sb.backend = b;
        from_pick = true;
    }
    // A host-toolchain backend (bwrap, systemd-nspawn, win-native) only makes
    // sense on the LOCAL box — it isolates via host-namespace syscalls. On a
    // non-local placement (ssh / k8s / provider) the placement is already the
    // isolation boundary, so a nested bwrap is meaningless: probing for it over
    // the remote exec channel answers Unreachable and the resolver stalls, then
    // hard-fails "sandbox backend 'bwrap' could not be resolved". This value is
    // almost always inherited from the base `[sandbox]` (or a per-worktree choice
    // saved from a prior LOCAL session), not a deliberate ask for THIS env — so
    // drop it to Auto and let the chain pick an in-placement runtime or run
    // natively in the placement, instead of treating it as an explicit demand.
    let mut host_toolchain_dropped = false;
    if !placement.is_local() && explicit_backend.is_some_and(|b| b.is_host_toolchain()) {
        // Expected, not a problem the user must fix: running natively in the
        // placement IS correct here (the provider/pod/host is the sandbox). So
        // trace it for `THEGN_LOG` / debugging rather than warning on every open —
        // the noisy hard-failure this replaces is gone, and `config explain`
        // still reports the configured value honestly.
        tracing::debug!(
            target: "thegn::sandbox",
            backend = %sb.backend,
            placement = %placement.label(),
            "host-local backend can't nest in a non-local placement; running natively in the placement",
        );
        explicit_backend = None;
        sb.backend = SandboxBackend::Auto;
        host_toolchain_dropped = true;
    }
    LaunchRequest {
        sandbox: sb,
        explicit: explicit_backend,
        from_pick,
        host_toolchain_dropped,
    }
}

/// Whether a saved backend value is a deliberate per-worktree override.
pub fn saved_pick_is_override(saved: Option<&str>) -> bool {
    saved
        .map(str::trim)
        .is_some_and(|s| !s.is_empty() && s != "auto")
}

/// Expand the configured backend chain into the exact candidates launch walks.
pub fn candidates(sb: &SandboxConfig) -> Vec<SandboxConfig> {
    // Disabled sandboxing is an intentional host decision. Keep the final host
    // admission check below (so an explicitly demanded isolation floor still
    // applies), but do not walk or probe the configured backend chain.
    if !sb.enabled {
        let mut host = sb.clone();
        host.backend = SandboxBackend::None;
        return vec![host];
    }
    if sb.backend != SandboxBackend::Auto {
        return vec![sb.clone()];
    }
    let mut out = Vec::new();
    for name in &sb.backend_chain {
        if let Ok(backend) = SandboxBackend::from_str_validated(name) {
            let mut c = sb.clone();
            c.backend = backend;
            out.push(c);
        }
    }
    if !out.iter().any(|c| c.backend == SandboxBackend::None) {
        let mut c = sb.clone();
        c.backend = SandboxBackend::None;
        out.push(c);
    }
    out
}

/// Predict the backend and isolation fields shown by the Info screen.
pub fn predict(
    environment: &Environment,
    saved_pick: Option<&str>,
) -> (Vec<Field>, Vec<BackendRow>) {
    let req = launch_request(
        environment.sandbox.clone(),
        &environment.placement,
        saved_pick,
        saved_pick_is_override(saved_pick),
    );
    let exec = if crate::projection::for_environment(environment).is_some() {
        Placement::Local
    } else {
        environment.placement.clone()
    };
    let cands = candidates(&req.sandbox);
    let names: Vec<String> = cands
        .iter()
        .map(|c| c.backend.as_str().to_string())
        .collect();
    let floors: Vec<IsolationFloor> = cands
        .iter()
        .map(|c| crate::sandbox_floor::floor_in_force(c, false))
        .collect();
    let local = exec.is_local();
    let (rows, chosen) = if local {
        let oci = (!req.sandbox.oci_runtime.trim().is_empty())
            .then_some(req.sandbox.oci_runtime.as_str());
        let support = crate::sandbox_support::support_report(&names, &exec, oci);
        let chosen = choose(&support, &floors);
        let rows =
            support
                .iter()
                .enumerate()
                .map(|(index, row)| BackendRow {
                    name: row.name.clone(),
                    state: format!("{:?}", row.state).to_lowercase(),
                    isolation: row.isolation.map(|c| c.as_str().to_string()),
                    meets_floor: Some(row.isolation.is_none_or(|class| {
                        crate::sandbox_floor::class_meets(class, floors[index])
                    })),
                    chosen: chosen == Some(index),
                    remedy: row.remedy.clone(),
                })
                .collect();
        (rows, chosen)
    } else {
        (
            names
                .iter()
                .map(|name| BackendRow {
                    name: name.clone(),
                    state: "not probed".into(),
                    isolation: None,
                    meets_floor: None,
                    chosen: false,
                    remedy: None,
                })
                .collect(),
            None,
        )
    };

    let launch_source = if req.from_pick {
        Source::WorktreePick
    } else if req.host_toolchain_dropped {
        Source::Placement
    } else {
        Source::Env(environment.name.clone())
    };
    let mut fields = vec![Field::value(
        keys::SANDBOX_LAUNCH_BACKEND,
        "Launch backend",
        req.sandbox.backend.as_str(),
        launch_source,
    )];
    let selected = if !req.sandbox.enabled {
        Field::value(
            keys::SANDBOX_SELECTED_BACKEND,
            "Selected backend",
            "none (sandbox disabled)",
            Source::Env(environment.name.clone()),
        )
    } else if local {
        if let Some(index) = chosen {
            Field::value(
                keys::SANDBOX_SELECTED_BACKEND,
                "Selected backend",
                format!("{} (ready)", names[index]),
                Source::Probe,
            )
        } else {
            let floor = crate::sandbox_floor::floor_in_force(&req.sandbox, false);
            Field::unknown(
                keys::SANDBOX_SELECTED_BACKEND,
                "Selected backend",
                format!(
                    "no candidate is usable at floor {}; launch follows on_floor_miss = {}",
                    floor.as_str(),
                    crate::sandbox_floor::miss_policy_in_force(&req.sandbox).as_str()
                ),
            )
            .warned()
        }
    } else {
        Field::unknown(
            keys::SANDBOX_SELECTED_BACKEND,
            "Selected backend",
            format!(
                "decided inside {} at launch; not probed",
                environment.placement.label()
            ),
        )
    };
    fields.push(selected);
    let floor = crate::sandbox_floor::floor_in_force(&req.sandbox, false);
    fields.push(Field::value(
        keys::SANDBOX_ISOLATION_FLOOR,
        "Isolation floor",
        floor.as_str(),
        if req.sandbox.isolation_floor.is_some() {
            Source::Env(environment.name.clone())
        } else {
            Source::Builtin
        },
    ));
    fields.push(Field::value(
        keys::SANDBOX_ON_FLOOR_MISS,
        "On floor miss",
        crate::sandbox_floor::miss_policy_in_force(&req.sandbox).as_str(),
        Source::Env(environment.name.clone()),
    ));
    (fields, rows)
}

fn choose(
    rows: &[crate::sandbox_support::BackendSupport],
    floors: &[IsolationFloor],
) -> Option<usize> {
    rows.iter().enumerate().find_map(|(index, row)| {
        let floor = floors.get(index).copied()?;
        (row.state.usable()
            && row
                .isolation
                .is_none_or(|class| crate::sandbox_floor::class_meets(class, floor)))
        .then_some(index)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::IsolationClass;
    use crate::config::{DataMode, SandboxBackend};
    use crate::placement::{ProviderPlacement, SshPlacement, TransportKind};
    use crate::sandbox_support::{BackendState, BackendSupport};

    fn sandbox(backend: SandboxBackend) -> SandboxConfig {
        SandboxConfig {
            backend,
            ..SandboxConfig::default()
        }
    }

    #[test]
    fn candidates_keep_launch_chain_behavior() {
        let cases = [
            (
                false,
                SandboxBackend::Auto,
                vec!["bwrap"],
                vec![SandboxBackend::None],
            ),
            (
                true,
                SandboxBackend::Bwrap,
                vec!["podman"],
                vec![SandboxBackend::Bwrap],
            ),
            (
                true,
                SandboxBackend::Auto,
                vec!["bwrap", "none"],
                vec![SandboxBackend::Bwrap, SandboxBackend::None],
            ),
            (
                true,
                SandboxBackend::Auto,
                vec!["bwrap"],
                vec![SandboxBackend::Bwrap, SandboxBackend::None],
            ),
            (
                true,
                SandboxBackend::Auto,
                vec!["invalid"],
                vec![SandboxBackend::None],
            ),
        ];
        for (enabled, backend, chain, expected) in cases {
            let mut sb = sandbox(backend);
            sb.enabled = enabled;
            sb.backend_chain = chain.into_iter().map(str::to_string).collect();
            assert_eq!(
                candidates(&sb)
                    .iter()
                    .map(|c| c.backend)
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn launch_request_applies_only_local_picks_and_drops_host_toolchains() {
        let local = launch_request(
            sandbox(SandboxBackend::Auto),
            &Placement::Local,
            Some("bwrap"),
            false,
        );
        assert_eq!(local.sandbox.backend, SandboxBackend::Bwrap);
        assert_eq!(local.explicit, Some(Backend::Bwrap));
        assert!(local.from_pick);
        assert!(!local.host_toolchain_dropped);

        let ssh = Placement::Ssh(SshPlacement::plain(
            "host".into(),
            22,
            false,
            TransportKind::Ssh,
        ));
        let remote_pick = launch_request(sandbox(SandboxBackend::Auto), &ssh, Some("bwrap"), false);
        assert_eq!(remote_pick.sandbox.backend, SandboxBackend::Auto);
        assert_eq!(remote_pick.explicit, None);
        assert!(!remote_pick.from_pick);

        let explicit_remote =
            launch_request(sandbox(SandboxBackend::Bwrap), &ssh, Some("bwrap"), true);
        assert_eq!(explicit_remote.sandbox.backend, SandboxBackend::Auto);
        assert_eq!(explicit_remote.explicit, None);
        assert!(explicit_remote.host_toolchain_dropped);

        let auto_pick = launch_request(
            sandbox(SandboxBackend::Auto),
            &Placement::Local,
            Some("auto"),
            false,
        );
        assert_eq!(auto_pick.sandbox.backend, SandboxBackend::Auto);
        assert!(!auto_pick.from_pick);
    }

    #[test]
    fn saved_pick_override_requires_a_non_auto_value() {
        for saved in [None, Some(""), Some("  "), Some("auto")] {
            assert!(!saved_pick_is_override(saved));
        }
        for saved in [Some("bwrap"), Some(" none "), Some(" AUTO ")] {
            assert!(saved_pick_is_override(saved));
        }
    }

    fn support(
        backend: Backend,
        name: &str,
        state: BackendState,
        isolation: IsolationClass,
    ) -> BackendSupport {
        BackendSupport {
            backend,
            name: name.into(),
            state,
            isolation: Some(isolation),
            remedy: None,
            caveat: None,
        }
    }

    #[test]
    fn choose_skips_floor_misses_and_unusable_rows() {
        let rows = vec![
            support(
                Backend::Bwrap,
                "bwrap",
                BackendState::Ready,
                IsolationClass::SharedKernel,
            ),
            support(
                Backend::Docker,
                "docker",
                BackendState::Ready,
                IsolationClass::GuestKernel,
            ),
        ];
        assert_eq!(
            choose(&rows, &[IsolationFloor::Off, IsolationFloor::Off]),
            Some(0)
        );
        assert_eq!(
            choose(
                &rows,
                &[IsolationFloor::GuestKernel, IsolationFloor::GuestKernel]
            ),
            Some(1)
        );
        assert_eq!(
            choose(
                &rows,
                &[IsolationFloor::GuestKernel, IsolationFloor::UserspaceKernel]
            ),
            Some(1)
        );
        let none = vec![support(
            Backend::Bwrap,
            "bwrap",
            BackendState::NotInstalled,
            IsolationClass::SharedKernel,
        )];
        assert_eq!(choose(&none, &[IsolationFloor::Off]), None);
    }

    #[test]
    fn predict_does_not_probe_non_local_placements() {
        let environment = Environment {
            name: "remote".into(),
            placement: Placement::Provider(ProviderPlacement {
                provider: "sprite".into(),
                id: "id".into(),
                interactive_prefix: vec!["sprite".into()],
                control_prefix: vec!["sprite".into()],
                up_command: Vec::new(),
                down_command: Vec::new(),
            }),
            sandbox: sandbox(SandboxBackend::Auto),
            data: DataMode::InEnv,
            unresolved_selection: false,
        };
        let (fields, rows) = predict(&environment, None);
        assert!(!fields.is_empty());
        assert!(rows.iter().all(|row| row.meets_floor.is_none()));
        assert!(rows.iter().all(|row| row.state == "not probed"));
    }
}
