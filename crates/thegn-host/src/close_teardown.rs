use thegn_core::placement::Placement;
use thegn_core::remote::GitLoc;
use thegn_core::store::WorkspaceStore;

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

pub(crate) fn background_close(cfg: Option<&thegn_core::config::Config>, path: &str) {
    crate::agent::deregister_vpn(path);
    crate::agent::deproject(path);
    if let Err(error) = crate::agent::deprovision_sync(path) {
        tracing::warn!(target: "thegn::lifecycle", worktree = path, %error, "provider sync failed during background close");
    }
    crate::agent::checkpoint_on_close(path);
    crate::bridge_sup::disconnect_path(path);

    let Some(cfg) = cfg else {
        thegn_core::sandbox::teardown_by_path(path);
        return;
    };
    let worktree = std::path::Path::new(path);
    let loc = GitLoc::for_worktree(worktree);
    let repo_root =
        thegn_core::repo::main_worktree(worktree).unwrap_or_else(|| worktree.to_path_buf());
    let read = thegn_core::db::Db::open().and_then(|db| {
        db.effective_env_checked(&worktree.to_string_lossy(), &repo_root.to_string_lossy())
    });
    let selected = match close_selection(path, read) {
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
    let env = cfg.resolve_env(&repo_root, &loc, worktree, selected.as_deref());
    sandbox_teardown(cfg, &env, &loc, path);
}

#[cfg(test)]
mod tests {
    use super::{Route, close_selection, placed_teardown_warning, route, warn_visible_with};
    use thegn_core::placement::{Placement, ProviderPlacement, SshPlacement, TransportKind};

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
