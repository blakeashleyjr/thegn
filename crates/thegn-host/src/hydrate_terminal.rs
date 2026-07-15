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
/// value a launched pane recorded, else — when the DB has nothing yet — the
/// backend the config resolves to (what a launch WOULD record), so the chip
/// shows the intended sandbox before the first sandboxed pane. `auto`/`none`
/// config resolves to empty, matching a plain local worktree. Logs on error.
pub(crate) fn active_backend(db: &Db, path: &str, cfg_backend: SandboxBackend) -> String {
    match db.worktree_sandbox(path) {
        Ok(Some(b)) if !b.trim().is_empty() => b,
        Ok(_) => thegn_core::sandbox::Backend::from_config(cfg_backend)
            .filter(|b| *b != thegn_core::sandbox::Backend::None)
            .map(|b| b.label().to_string())
            .unwrap_or_default(),
        Err(e) => {
            tracing::warn!(target: "thegn::hydrate", error = %e, "worktree_sandbox() read failed; location chip may be blank");
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
        let backend = row.sandbox_backend.trim();
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

    fn row(kind: &str, connection: &str, sandbox: &str) -> TerminalRow {
        TerminalRow {
            id: 1,
            name: "snappy-shark".into(),
            kind: kind.into(),
            connection_string: connection.into(),
            folder_id: None,
            created_at: 0,
            last_active: 0,
            position: 0,
            sandbox_backend: sandbox.into(),
            env_name: String::new(),
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
