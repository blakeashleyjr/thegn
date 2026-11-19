use thegn_core::placement::Placement;
use thegn_core::remote::GitLoc;
use thegn_core::store::WorkspaceStore;

/// What a close needs about a worktree, read from the caller's handle BEFORE
/// its row is deleted. DB reads only: no fs, no git, no open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloseSnapshot {
    pub(crate) location: Result<Option<String>, String>,
    pub(crate) repo_root: Option<String>,
    pub(crate) env: Result<Option<String>, String>,
}

pub(crate) fn capture(db: &thegn_core::db::Db, path: &str) -> CloseSnapshot {
    let location = db.location_for(path).map_err(|error| format!("{error:#}"));
    let repo_root = db.repo_root_for(path).ok().flatten();
    let env = db
        .effective_env_checked(path, repo_root.as_deref().unwrap_or_default())
        .map_err(|error| format!("{error:#}"));
    CloseSnapshot {
        location,
        repo_root,
        env,
    }
}

pub(crate) enum Route {
    Placed,
    LocalByPath,
}

pub(crate) fn route(placement: &Placement) -> Route {
    match placement {
        Placement::Ssh(_) => Route::Placed,
        Placement::Local | Placement::Provider(_) | Placement::K8s(_) => Route::LocalByPath,
    }
}

/// The env selection a background close tears down; an unreadable selection
/// produces a warning because path-only cleanup can leave a remote sandbox running.
fn close_selection(
    path: &str,
    read: anyhow::Result<Option<String>>,
) -> Result<Option<String>, String> {
    match read {
        Ok(selected) => Ok(selected),
        Err(error) => Err(format!(
            "Environment selection could not be read while closing {path}; path-only cleanup may leave its SSH sandbox container running ({error:#})"
        )),
    }
}

/// The location a background close tears down; an unreadable location produces a warning because path-only cleanup
/// can leave a remote sandbox running.
fn close_location(path: &str, resolved: Result<GitLoc, String>) -> Result<GitLoc, String> {
    resolved.map_err(|error| {
        format!(
            "Worktree location could not be read while closing {path}; path-only cleanup may leave its SSH sandbox container running ({error})"
        )
    })
}

/// Deliver warnings from off-loop teardown work to stderr or the active TUI.
fn warn_visible_with(text: &str, tui_active: bool, show: impl FnOnce(&str)) {
    thegn_core::msg::warn(text);
    if tui_active {
        show(text);
    }
}

/// Report off-loop teardown warnings on stderr outside the TUI, or in its
/// status line and toast once the TUI is active.
pub(crate) fn warn_visible(text: &str) {
    warn_visible_with(text, thegn_core::msg::tui_active(), |text| {
        if let Some(notify) = crate::notify::global() {
            notify.emit_status(text);
            notify.emit_toast(text, thegn_core::notification::Priority::Notice);
        }
    });
}

fn placed_teardown_warning(path: &str, name: &str, error: &dyn std::fmt::Display) -> String {
    format!(
        "Sandbox teardown incomplete while closing {path}: container {name} may still be running on its SSH host ({error})"
    )
}

pub(crate) fn sandbox_teardown(
    cfg: &thegn_core::config::Config,
    env: &thegn_core::env::Environment,
    loc: &GitLoc,
    path: &str,
) {
    match route(&env.placement) {
        Route::Placed => {
            let name = thegn_core::sandbox::container_name_for_config_profile(path, &cfg.profile);
            if let Err(error) = thegn_core::sandbox::teardown_placed_checked(
                &env.sandbox,
                &env.placement,
                loc,
                &name,
            ) {
                tracing::warn!(target: "thegn::lifecycle", worktree = path, %error, "sandbox teardown incomplete during close");
                warn_visible(&placed_teardown_warning(path, &name, &error));
            }
        }
        Route::LocalByPath => thegn_core::sandbox::teardown_by_path(path),
    }
}

/// Mirror of `GitLoc::from_checked_lookup` over a close snapshot.
fn snapshot_location(
    path: &str,
    location: &Result<Option<String>, String>,
) -> Result<GitLoc, String> {
    match location {
        Ok(Some(location)) if !location.trim().is_empty() && location.trim() != "local" => {
            let parsed = GitLoc::from_db(path, Some(location.as_str()));
            if matches!(&parsed, GitLoc::Local(_)) {
                return Err("unrecognised worktree location".into());
            }
            Ok(parsed)
        }
        Ok(location) => Ok(GitLoc::from_db(path, location.as_deref())),
        Err(error) => Err(error.clone()),
    }
}

pub(crate) fn background_close_with(
    cfg: Option<&thegn_core::config::Config>,
    path: &str,
    snap: CloseSnapshot,
) {
    crate::agent::deregister_vpn(path);
    crate::agent::deproject(path);
    if let Err(error) = crate::agent::deprovision_sync(path) {
        tracing::warn!(target: "thegn::lifecycle", worktree = path, %error, "provider sync failed during background close");
    }
    crate::agent_teardown::checkpoint_on_close_with(
        path,
        snap.repo_root.as_deref(),
        snap.env.clone(),
    );
    crate::bridge_sup::disconnect_path(path);

    let Some(cfg) = cfg else {
        thegn_core::sandbox::teardown_by_path(path);
        return;
    };
    let worktree = std::path::Path::new(path);
    let loc = match close_location(path, snapshot_location(path, &snap.location)) {
        Ok(loc) => loc,
        Err(warning) => {
            tracing::warn!(target: "thegn::lifecycle", worktree = path, %warning,
                "worktree location unreadable during close; tearing down by path only");
            warn_visible(&warning);
            thegn_core::sandbox::teardown_by_path(path);
            return;
        }
    };
    let repo_root = snap
        .repo_root
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            thegn_core::repo::main_worktree(worktree).unwrap_or_else(|| worktree.to_path_buf())
        });
    let selected = match close_selection(path, snap.env.map_err(anyhow::Error::msg)) {
        Ok(selected) => selected,
        Err(warning) => {
            tracing::warn!(target: "thegn::lifecycle", worktree = path, %warning,
                "environment selection unreadable during close; tearing down by path only");
            if let Some(notify) = crate::notify::global() {
                notify.emit_toast(&warning, thegn_core::notification::Priority::Notice);
            }
            thegn_core::sandbox::teardown_by_path(path);
            return;
        }
    };
    let env = cfg.resolve_env(
        &repo_root,
        &loc,
        worktree,
        &thegn_core::sandbox_pin::EnvSelection {
            name: selected.clone(),
            profile: None,
        },
    );
    sandbox_teardown(cfg, &env, &loc, path);
}

#[cfg(test)]
mod tests {
    use super::{
        CloseSnapshot, Route, capture, close_location, close_selection, placed_teardown_warning,
        route, snapshot_location, warn_visible_with,
    };
    use thegn_core::db::Db;
    use thegn_core::placement::{Placement, ProviderPlacement, SshPlacement, TransportKind};
    use thegn_core::remote::GitLoc;
    use thegn_core::store::WorkspaceStore;

    #[test]
    fn ssh_placement_routes_to_the_placed_teardown() {
        let placement = Placement::Ssh(SshPlacement::plain(
            "remote.example".into(),
            22,
            false,
            TransportKind::Ssh,
        ));
        assert!(matches!(route(&placement), Route::Placed));
    }

    #[test]
    fn local_and_provider_route_by_path() {
        let provider = Placement::Provider(ProviderPlacement {
            provider: "test".into(),
            id: "sandbox".into(),
            interactive_prefix: Vec::new(),
            control_prefix: Vec::new(),
            up_command: Vec::new(),
            down_command: Vec::new(),
        });
        assert!(matches!(route(&Placement::Local), Route::LocalByPath));
        assert!(matches!(route(&provider), Route::LocalByPath));
    }

    #[test]
    fn close_selection_refuses_an_unreadable_selection() {
        let Err(warning) = close_selection("/wt", Err(anyhow::anyhow!("unavailable"))) else {
            panic!("unreadable selection must report the possible remote residual");
        };
        assert!(warning.contains("SSH sandbox container running"));
        assert!(warning.contains("unavailable"));
    }

    #[test]
    fn close_selection_passes_a_read_through() {
        assert_eq!(
            close_selection("/wt", Ok(Some("box".into()))),
            Ok(Some("box".into()))
        );
        assert_eq!(close_selection("/wt", Ok(None)), Ok(None));
    }

    #[test]
    fn close_location_refuses_an_unreadable_location() {
        let Err(warning) = close_location(
            "/wt",
            Err("close teardown refused: cannot read where /wt lives (unrecognised worktree location)".into()),
        ) else {
            panic!("unreadable location must report the possible remote residual");
        };
        assert!(warning.contains("SSH sandbox container running"));
        assert!(warning.contains("unrecognised worktree location"));
    }

    #[test]
    fn close_location_passes_a_location_through() {
        assert!(matches!(
            close_location("/wt", Ok(GitLoc::Local("/wt".into()))),
            Ok(GitLoc::Local(_))
        ));
    }

    #[test]
    fn snapshot_location_mirrors_checked_lookup() {
        let provider = r#"{"control_prefix":["sprite","exec"],"path":"/workspace"}"#;
        let cases: [(Result<Option<String>, String>, bool, bool); 5] = [
            (Ok(None), true, false),
            (Ok(Some("local".into())), true, false),
            (Ok(Some(provider.into())), false, true),
            (Ok(Some("garbage".into())), false, false),
            (Err("unavailable".into()), false, false),
        ];
        for (location, local, provider) in cases {
            let result = snapshot_location("/wt", &location);
            if local {
                assert!(matches!(result, Ok(GitLoc::Local(_))));
            } else if provider {
                assert!(matches!(result, Ok(GitLoc::Provider { .. })));
            } else if let Err(error) = location {
                assert!(matches!(&result, Err(e) if *e == error), "{result:?}");
            } else {
                assert!(
                    matches!(&result, Err(e) if e == "unrecognised worktree location"),
                    "{result:?}"
                );
            }
        }
    }

    #[test]
    fn capture_preserves_location_and_env_after_worktree_deletion() {
        let db = Db::open_memory().unwrap();
        let path = "/wt/provider";
        let location = r#"{"control_prefix":["sprite","exec"],"path":"/workspace"}"#;
        db.put_worktree("tab", "/repo", path, "feat", Some(location), None)
            .unwrap();
        db.set_worktree_env(path, "machine0").unwrap();

        let snapshot = capture(&db, path);
        db.del_worktree(path).unwrap();

        assert_eq!(
            snapshot,
            CloseSnapshot {
                location: Ok(Some(location.into())),
                repo_root: Some("/repo".into()),
                env: Ok(Some("machine0".into())),
            }
        );
    }

    #[test]
    fn warn_visible_toasts_only_while_the_tui_runs() {
        let mut shown: Option<String> = None;
        warn_visible_with("x", true, |text| shown = Some(text.to_string()));
        assert_eq!(shown.as_deref(), Some("x"));

        let mut shown: Option<String> = None;
        warn_visible_with("x", false, |text| shown = Some(text.to_string()));
        assert_eq!(shown, None);
    }

    #[test]
    fn placed_teardown_warning_names_the_container_and_cause() {
        let warning = placed_teardown_warning("/wt", "tg-c", &"ssh: connection refused");
        assert!(warning.contains("/wt"));
        assert!(warning.contains("tg-c"));
        assert!(warning.contains("may still be running"));
        assert!(warning.contains("connection refused"));
    }
}
