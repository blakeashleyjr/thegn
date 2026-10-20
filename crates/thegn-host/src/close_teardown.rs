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
    let selected = thegn_core::db::Db::open()
        .ok()
        .and_then(|db| db.effective_env(&worktree.to_string_lossy(), &repo_root.to_string_lossy()));
    let env = cfg.resolve_env(&repo_root, &loc, worktree, selected.as_deref());
    sandbox_teardown(cfg, &env, &loc, path);
}

#[cfg(test)]
mod tests {
    use super::{Route, route};
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
}
