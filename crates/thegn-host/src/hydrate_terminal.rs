//! Small hydration helpers extracted from `hydrate.rs` (pinned at the file-size
//! cap): the sidebar's terminal list and the active worktree's sandbox backend
//! for the tab-bar `(backend)` chip. Both log on a DB read error instead of
//! swallowing it — a silent failure is how the sidebar/chip went blank.

use thegn_core::config::SandboxBackend;
use thegn_core::db::Db;
use thegn_core::models::TerminalRow;
use thegn_core::store::WorkspaceStore;

/// The terminals to show in the sidebar. On a read error, log and return empty
/// (the section then shows its empty-state hint) rather than silently blanking.
pub(crate) fn sidebar_terminals(db: &Db) -> Vec<TerminalRow> {
    match db.terminals() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(target: "thegn::hydrate", error = %e, "terminals() read failed; sidebar Terminals section will be empty");
            Vec::new()
        }
    }
}

/// The sandbox backend for the active worktree's tab-bar `(backend)` chip: the
/// containment its last launch ACTUALLY entered (argv-derived, recorded by
/// `launch_spec_full`), or empty when it has never launched or ran on the host.
///
/// It deliberately does NOT fall back to the backend config would resolve to.
/// That fallback rendered a prediction as fact, which is the same class of claim
/// that let a bare host shell display as a container — a chip that is briefly
/// empty is honest; one that names a sandbox the pane isn't in is not.
/// `cfg_backend` is retained for call-site compatibility and is unused. Logs on
/// error.
pub(crate) fn active_backend(db: &Db, path: &str, _cfg_backend: SandboxBackend) -> String {
    match db.worktree_observed(path) {
        Ok(Some(b)) if !b.trim().is_empty() && b.trim() != "host" => b,
        // Nothing observed yet = never launched. Show NOTHING rather than the
        // backend config would resolve to: that prediction was displayed as
        // fact, and it is exactly the class of claim that let a host pane read
        // as sandboxed. The chip fills in the moment a pane actually launches.
        Ok(_) => String::new(),
        Err(e) => {
            tracing::warn!(target: "thegn::hydrate", error = %e, "worktree_observed() read failed; location chip may be blank");
            String::new()
        }
    }
}

/// The tab-bar env cluster for a **terminal** tab, as `(placement_kind,
/// placement_label, sandbox_backend)` — the same triple the worktree path fills
/// from `resolve_env`. A terminal's environment is defined by its own
/// connection + sandbox (the wizard's Host/Sandbox pick), NOT the workspace /
/// global `[sandbox] default_env`: a terminal group carries an empty path, so
/// resolving the cwd's env made a plain local shell inherit (and mislabel
/// itself as) the workspace's default provider env (e.g. `machine0`).
///
/// `row` is the active terminal's DB row, or `None` for a just-created terminal
/// whose row isn't loaded yet (treated as a local shell). Local shells show an
/// explicit `[local]` chip plus their `(backend)` when sandboxed; remote
/// terminals show the transport `[ssh]`/`[mosh]` with the host as the detail
/// label. The backend is filtered with the same rule as the sidebar detail line
/// and `tabbar_env::env_chips` (`""`/`none`/`host` → empty).
pub(crate) fn terminal_env(row: Option<&TerminalRow>) -> (Option<String>, Option<String>, String) {
    let Some(row) = row else {
        // Not yet persisted: a fresh terminal is a local shell until told otherwise.
        return (Some("local".into()), Some("local".into()), String::new());
    };
    let (_, host_label, is_local) =
        crate::sidebar::terminal_host(&row.connection_string, &row.kind);
    if is_local {
        // The OBSERVED containment, never `row.sandbox_backend` (the pick): a
        // pick that resolved to a bare host shell must not render as a sandbox.
        let backend = row.observed_backend.trim();
        let backend = if backend.is_empty() || backend == "none" || backend == "host" {
            String::new()
        } else {
            backend.to_string()
        };
        return (Some("local".into()), Some("local".into()), backend);
    }
    // Remote: the transport verb is the terse chip; the host is the detail label.
    let conn = row.connection_string.trim();
    let transport = if conn.starts_with("mosh ") || conn == "mosh" {
        "mosh"
    } else {
        "ssh"
    };
    (Some(transport.into()), Some(host_label), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row whose pick and observed containment agree — the honoured case.
    fn row(kind: &str, connection: &str, sandbox: &str) -> TerminalRow {
        picked_vs_observed(kind, connection, sandbox, sandbox)
    }

    /// A row where the pick and what actually launched may DIFFER, which is the
    /// case the chip used to get wrong.
    fn picked_vs_observed(
        kind: &str,
        connection: &str,
        picked: &str,
        observed: &str,
    ) -> TerminalRow {
        TerminalRow {
            id: 1,
            name: "snappy-shark".into(),
            kind: kind.into(),
            connection_string: connection.into(),
            folder_id: None,
            created_at: 0,
            last_active: 0,
            position: 0,
            sandbox_backend: picked.into(),
            env_name: String::new(),
            observed_backend: observed.into(),
        }
    }

    #[test]
    fn local_uncontained_shows_local_no_backend() {
        // The reported bug: a local uncontained terminal must read `[local]`,
        // never inherit the workspace/global default provider env.
        for sandbox in ["", "host", "none"] {
            let (kind, label, backend) = terminal_env(Some(&row("local", "", sandbox)));
            assert_eq!(kind.as_deref(), Some("local"), "sandbox={sandbox:?}");
            assert_eq!(label.as_deref(), Some("local"), "sandbox={sandbox:?}");
            assert_eq!(backend, "", "sandbox={sandbox:?}");
        }
    }

    #[test]
    fn local_sandboxed_shows_local_and_backend() {
        let (kind, label, backend) = terminal_env(Some(&row("local", "", "podman-rootless")));
        assert_eq!(kind.as_deref(), Some("local"));
        assert_eq!(label.as_deref(), Some("local"));
        assert_eq!(backend, "podman-rootless");
    }

    #[test]
    fn a_pick_that_degraded_to_the_host_never_shows_as_contained() {
        // THE reported bug: a terminal created with an explicit rootless-podman
        // pick, on a host with no podman machine running, spawned a bare shell
        // and still displayed `podman-rootless`. The chip reads what the launch
        // entered, so it must now read empty (host) while the pick survives in
        // `sandbox_backend` for the next resolution.
        let r = picked_vs_observed("local", "", "podman-rootless", "host");
        let (kind, label, backend) = terminal_env(Some(&r));
        assert_eq!(kind.as_deref(), Some("local"));
        assert_eq!(label.as_deref(), Some("local"));
        assert_eq!(backend, "", "a host shell must never display a container");
        assert_eq!(
            r.sandbox_backend, "podman-rootless",
            "the pick must survive so a later launch can honour it"
        );
    }

    #[test]
    fn a_never_launched_terminal_claims_nothing() {
        // Picked a sandbox, never launched: no observation yet, so no claim.
        let (_, _, backend) = terminal_env(Some(&picked_vs_observed("local", "", "docker", "")));
        assert_eq!(backend, "");
    }

    #[test]
    fn remote_ssh_shows_transport_and_host() {
        let (kind, label, backend) = terminal_env(Some(&row("remote", "ssh dave@prod", "")));
        assert_eq!(kind.as_deref(), Some("ssh"));
        assert_eq!(label.as_deref(), Some("prod"));
        assert_eq!(backend, "");
    }

    #[test]
    fn remote_mosh_transport() {
        let (kind, _label, _backend) = terminal_env(Some(&row("remote", "mosh root@box", "")));
        assert_eq!(kind.as_deref(), Some("mosh"));
    }

    #[test]
    fn missing_row_is_local() {
        let (kind, label, backend) = terminal_env(None);
        assert_eq!(kind.as_deref(), Some("local"));
        assert_eq!(label.as_deref(), Some("local"));
        assert_eq!(backend, "");
    }
}
