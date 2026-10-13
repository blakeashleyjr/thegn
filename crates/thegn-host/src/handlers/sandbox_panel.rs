//! Shared sandbox-panel target resolution and guarded container actions.

use termwiz::terminal::TerminalWaker;
use thegn_core::config::SandboxConfig;
use thegn_core::remote::GitLoc;
use thegn_core::sandbox::{Backend, ContainerInfo};

use crate::hydrate::RefreshKind;
use crate::layout::PanelWidth;
use crate::menu::{self, MenuChoice, MenuOverlay};

pub(crate) fn visible_containers<'a>(
    containers: &'a [ContainerInfo],
    active: &str,
    show_all: bool,
) -> Vec<&'a ContainerInfo> {
    if show_all {
        containers.iter().collect()
    } else {
        containers
            .iter()
            .filter(|container| container.ours && container.name == active)
            .collect()
    }
}

pub(crate) fn list_drawn(width: PanelWidth, show_all: bool) -> bool {
    match width {
        PanelWidth::Full => true,
        PanelWidth::Half => show_all,
        PanelWidth::Normal => false,
    }
}

pub(crate) fn target<'a>(
    containers: &'a [ContainerInfo],
    active: &str,
    show_all: bool,
    width: PanelWidth,
    cursor: usize,
) -> Option<&'a ContainerInfo> {
    if list_drawn(width, show_all) {
        let visible = visible_containers(containers, active, show_all);
        visible
            .get(cursor.min(visible.len().saturating_sub(1)))
            .copied()
    } else {
        containers
            .iter()
            .find(|container| container.ours && container.name == active)
    }
}

pub(crate) enum SandboxKey {
    NoTarget(String),
    Refused(String),
    Logs(ContainerInfo),
    Confirm(MenuOverlay),
}

pub(crate) fn key(
    c: char,
    containers: &[ContainerInfo],
    active: &str,
    show_all: bool,
    width: PanelWidth,
    cursor: usize,
) -> SandboxKey {
    let Some(container) = target(containers, active, show_all, width, cursor) else {
        return SandboxKey::NoTarget("no container to act on (OCI sandboxes only)".into());
    };
    if c == 'l' {
        return SandboxKey::Logs(container.clone());
    }
    if !container.ours {
        return SandboxKey::Refused(format!(
            "{} is not a thegn container — read-only here (use {} directly)",
            container.name, container.backend
        ));
    }
    let verb = if c == 's' { "stop" } else { "restart" };
    let target = if container.name == active {
        "this worktree's sandbox"
    } else {
        "this container"
    };
    SandboxKey::Confirm(menu::confirm_menu(
        format!("{verb} container {}?", container.name),
        format!("This kills the panes running in {target}."),
        if c == 's' {
            "sandbox-stop"
        } else {
            "sandbox-restart"
        },
        format!("{}\u{1f}{}", container.backend, container.name),
        true,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ControlRequest {
    pub name: String,
    pub backend: Backend,
    pub action: &'static str,
}

pub(crate) fn control_status(req: &ControlRequest) -> String {
    format!("{}ing {}…", req.action, req.name)
}

pub(crate) fn resolve_choice(
    choice: &MenuChoice,
    containers: &[ContainerInfo],
) -> Option<Result<ControlRequest, String>> {
    let (arg, action) = match choice {
        MenuChoice::Confirm {
            tag: "sandbox-stop",
            arg,
        } => (arg, "stop"),
        MenuChoice::Confirm {
            tag: "sandbox-restart",
            arg,
        } => (arg, "restart"),
        _ => return None,
    };
    let Some((backend_name, name)) = arg.split_once('\u{1f}') else {
        return Some(Err("container confirmation is malformed".into()));
    };
    let Some(container) = containers
        .iter()
        .find(|container| container.name == name && container.backend == backend_name)
    else {
        return Some(Err(format!(
            "container {name} ({backend_name}) is no longer available"
        )));
    };
    if !container.ours {
        return Some(Err(format!(
            "{} is not a thegn container — read-only here (use {} directly)",
            container.name, container.backend
        )));
    }
    let Some(backend) = Backend::parse(&container.backend) else {
        return Some(Err(format!(
            "{}: unknown container backend {}",
            container.name, container.backend
        )));
    };
    Some(Ok(ControlRequest {
        name: container.name.clone(),
        backend,
        action,
    }))
}

pub(crate) fn spawn_control(
    cfg_sb: SandboxConfig,
    wt: std::path::PathBuf,
    req: ControlRequest,
    refresh_tx: tokio::sync::mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
) {
    tokio::task::spawn_blocking(move || {
        let loc = GitLoc::for_worktree(&wt);
        let ok = thegn_core::sandbox::container_control(
            &cfg_sb,
            &loc,
            req.backend,
            &req.name,
            req.action,
        );
        let message = if ok {
            format!("{}: {} ✓", req.name, req.action)
        } else {
            format!("{}: {} failed", req.name, req.action)
        };
        let priority = if ok {
            thegn_core::notification::Priority::Info
        } else {
            thegn_core::notification::Priority::Alert
        };
        if let Err(error) = refresh_tx.send(RefreshKind::Toast { message, priority }) {
            tracing::debug!(target: "thegn::sandbox", %error, "container action toast could not be delivered");
        }
        if let Err(error) = refresh_tx.send(RefreshKind::Model) {
            tracing::debug!(target: "thegn::sandbox", %error, "container action refresh could not be delivered");
        }
        if let Err(error) = waker.wake() {
            tracing::debug!(target: "thegn::sandbox", %error, "container action wake failed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::MenuOutcome;
    use termwiz::input::{KeyCode, Modifiers};

    fn container(name: &str, ours: bool, backend: &str) -> ContainerInfo {
        ContainerInfo {
            name: name.into(),
            image: "image".into(),
            status: "running".into(),
            ours,
            backend: backend.into(),
            cpu: String::new(),
            mem: String::new(),
            net: String::new(),
            containment: "worktree+caches".into(),
            mounts: String::new(),
        }
    }

    fn fixtures() -> Vec<ContainerInfo> {
        vec![
            container("thegn-repo-home", true, "podman"),
            container("postgres-dev", false, "docker"),
        ]
    }

    #[test]
    fn foreign_container_refuses_stop_and_restart() {
        let containers = fixtures();
        for action in ['s', 'r'] {
            match key(
                action,
                &containers,
                "thegn-repo-home",
                true,
                PanelWidth::Full,
                1,
            ) {
                SandboxKey::Refused(status) => assert!(status.contains("postgres-dev")),
                _ => panic!("foreign container action was not refused"),
            }
        }
        match key(
            'l',
            &containers,
            "thegn-repo-home",
            true,
            PanelWidth::Full,
            1,
        ) {
            SandboxKey::Logs(container) => assert_eq!(container.name, "postgres-dev"),
            _ => panic!("logs should work on a foreign container"),
        }
    }

    #[test]
    fn own_container_stop_needs_confirm() {
        let mut menu = match key(
            's',
            &fixtures(),
            "thegn-repo-home",
            true,
            PanelWidth::Full,
            0,
        ) {
            SandboxKey::Confirm(menu) => menu,
            _ => panic!("own container stop should ask for confirmation"),
        };
        assert_eq!(
            menu.handle_key(&KeyCode::Char('y'), Modifiers::NONE),
            MenuOutcome::Pick(MenuChoice::Confirm {
                tag: "sandbox-stop",
                arg: "podman\u{1f}thegn-repo-home".into(),
            })
        );
    }

    #[test]
    fn scope_filters_full_list() {
        let containers = fixtures();
        let visible = visible_containers(&containers, "thegn-repo-home", false);
        assert_eq!(visible.len(), 1);
        assert_eq!(
            target(&containers, "thegn-repo-home", false, PanelWidth::Full, 5)
                .map(|container| container.name.as_str()),
            Some("thegn-repo-home")
        );
    }

    #[test]
    fn half_all_scope_targets_the_cursor_row() {
        let containers = fixtures();
        assert_eq!(
            target(&containers, "thegn-repo-home", true, PanelWidth::Half, 1)
                .map(|container| container.name.as_str()),
            Some("postgres-dev")
        );
    }

    #[test]
    fn resolve_choice_rechecks_ownership_at_pick_time() {
        let vanished = MenuChoice::Confirm {
            tag: "sandbox-restart",
            arg: "podman\u{1f}x".into(),
        };
        assert!(resolve_choice(&vanished, &[]).is_some_and(|result| result.is_err()));
        let changed_owner = MenuChoice::Confirm {
            tag: "sandbox-restart",
            arg: "docker\u{1f}postgres-dev".into(),
        };
        assert!(resolve_choice(&changed_owner, &fixtures()).is_some_and(|result| result.is_err()));
        let unrelated = MenuChoice::Confirm {
            tag: "env-remove",
            arg: "podman\u{1f}x".into(),
        };
        assert!(resolve_choice(&unrelated, &fixtures()).is_none());
    }

    #[test]
    fn resolve_choice_matches_the_confirmed_backend_and_fails_closed() {
        let containers = vec![
            container("thegn-shared", true, "podman"),
            container("thegn-shared", true, "docker"),
        ];
        let choice = MenuChoice::Confirm {
            tag: "sandbox-stop",
            arg: "docker\u{1f}thegn-shared".into(),
        };
        assert_eq!(
            resolve_choice(&choice, &containers),
            Some(Ok(ControlRequest {
                name: "thegn-shared".into(),
                backend: Backend::Docker,
                action: "stop",
            }))
        );

        let unknown_backend = MenuChoice::Confirm {
            tag: "sandbox-stop",
            arg: "future-runtime\u{1f}thegn-shared".into(),
        };
        let unknown = vec![container("thegn-shared", true, "future-runtime")];
        assert!(
            resolve_choice(&unknown_backend, &unknown).is_some_and(
                |result| result.is_err_and(|error| error.contains("unknown container backend"))
            )
        );
    }
}
