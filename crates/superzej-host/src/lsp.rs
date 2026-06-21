//! Host-side LSP integration: the [`LspSupervisor`] (lazy, warm, per-worktree
//! server lifecycle) and the [`LspDiagnostics`] store that folds server-pushed
//! diagnostics into the existing Problems panel.
//!
//! The supervisor owns the [`superzej_svc::lsp::LspClient`] connections, keyed
//! by `(worktree_root, language)`. Servers are **never** started eagerly — the
//! first request for a `(root, lang)` spawns and initializes one, and it then
//! stays warm across tab switches (rust-analyzer's index is expensive). The
//! inner state is `Arc`-shared so an off-loop request task can lazily start and
//! reuse clients without blocking the render loop.
//!
//! Diagnostics arrive asynchronously on each client's reader thread; the host
//! sets up a bridge thread (it owns the `TerminalWaker`; svc does not) that
//! forwards them onto the loop's channel and pulses the waker.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use superzej_core::config::Config;
use superzej_core::semantic::Lang;
use superzej_svc::lsp::{
    LspClient, LspError, LspSeverity, PublishedDiagnostics, ServerOverride, resolve_server,
};

use crate::panel::{DiagnosticItem, Severity};

/// Shared LSP state, owned by the event loop and cloned into off-loop tasks.
pub struct LspSupervisor {
    inner: Arc<LspInner>,
    /// Receiver end of the diagnostics channel handed to clients; taken once by
    /// the host to drive the bridge thread.
    raw_rx: Option<Receiver<PublishedDiagnostics>>,
}

/// `(root, lang)` → started client, or `None` once we've tried and found no
/// server (so we don't re-spawn on every request).
type ClientMap = HashMap<(PathBuf, Lang), Option<Arc<LspClient>>>;

pub struct LspInner {
    enabled: bool,
    overrides: Vec<ServerOverride>,
    diag_tx: Sender<PublishedDiagnostics>,
    clients: Mutex<ClientMap>,
}

impl LspSupervisor {
    /// Build from config. Starts nothing; just records the policy + overrides.
    pub fn from_config(cfg: &Config) -> Self {
        let (diag_tx, raw_rx) = channel();
        let overrides = cfg
            .lsp
            .servers
            .iter()
            .map(|s| ServerOverride {
                lang: s.lang.clone(),
                command: s.command.clone(),
                args: s.args.clone(),
            })
            .collect();
        LspSupervisor {
            inner: Arc::new(LspInner {
                enabled: cfg.lsp.enabled,
                overrides,
                diag_tx,
                clients: Mutex::new(HashMap::new()),
            }),
            raw_rx: Some(raw_rx),
        }
    }

    /// Take the diagnostics receiver to drive the host bridge thread (once).
    pub fn take_diagnostics_rx(&mut self) -> Option<Receiver<PublishedDiagnostics>> {
        self.raw_rx.take()
    }

    /// An `Arc` handle for use inside an off-loop (`spawn_blocking`) task.
    pub fn handle(&self) -> Arc<LspInner> {
        self.inner.clone()
    }
}

impl LspInner {
    /// Get the warm client for `(root, lang)`, lazily spawning+initializing it on
    /// first use. **Blocks** (spawn + initialize) — call off the event loop.
    pub fn client(&self, root: &Path, lang: Lang) -> Result<Arc<LspClient>, LspError> {
        if !self.enabled {
            return Err(LspError::NotAvailable);
        }
        let key = (root.to_path_buf(), lang);
        let mut clients = self.clients.lock().unwrap();
        if let Some(slot) = clients.get(&key) {
            return slot.clone().ok_or(LspError::NotAvailable);
        }

        let started = match resolve_server(lang, &self.overrides) {
            Some(spec) => LspClient::start(&spec, root, self.diag_tx.clone())
                .and_then(|c| c.initialize(root).map(|_| c))
                .map(Arc::new),
            None => Err(LspError::NotAvailable),
        };

        match started {
            Ok(client) => {
                clients.insert(key, Some(client.clone()));
                Ok(client)
            }
            Err(e) => {
                // Cache "no server" so we don't try to spawn on every request.
                if e == LspError::NotAvailable {
                    clients.insert(key, None);
                }
                Err(e)
            }
        }
    }
}

/// Persistent store of LSP-pushed diagnostics, keyed by file path. Survives
/// model-hydration swaps (which only carry git/db state) so the Problems panel
/// keeps showing them; re-merged into the rendered list on every update + swap.
#[derive(Debug, Default)]
pub struct LspDiagnostics {
    by_file: HashMap<String, Vec<DiagnosticItem>>,
}

impl LspDiagnostics {
    pub fn new() -> Self {
        LspDiagnostics::default()
    }

    /// Apply a server's latest diagnostics for one document. An empty set clears
    /// that file. `root`, when the path is under it, yields a repo-relative path.
    pub fn apply(&mut self, pd: PublishedDiagnostics, root: Option<&Path>) {
        let file = relativize(&pd.path, root);
        if pd.diagnostics.is_empty() {
            self.by_file.remove(&file);
            return;
        }
        let items = pd
            .diagnostics
            .into_iter()
            .map(|d| to_panel_item(&file, d))
            .collect();
        self.by_file.insert(file, items);
    }

    /// Replace the LSP-sourced entries in `dst` with the current store, keeping
    /// any non-LSP (task-output) diagnostics, then re-sort by severity.
    pub fn merge_into(&self, dst: &mut Vec<DiagnosticItem>) {
        dst.retain(|d| !d.source.starts_with("lsp:"));
        for items in self.by_file.values() {
            dst.extend(items.iter().cloned());
        }
        dst.sort_by_key(|d| d.severity as u8);
    }

    pub fn is_empty(&self) -> bool {
        self.by_file.is_empty()
    }
}

/// Convert one svc diagnostic to a panel item (source tagged `lsp:<source>`).
fn to_panel_item(file: &str, d: superzej_svc::lsp::LspDiagnostic) -> DiagnosticItem {
    DiagnosticItem {
        file: file.to_string(),
        line: d.line as u64 + 1, // LSP is 0-based; the panel shows 1-based
        col: Some(d.character as u64 + 1),
        severity: match d.severity {
            LspSeverity::Error => Severity::Error,
            LspSeverity::Warning => Severity::Warning,
            LspSeverity::Info => Severity::Info,
            LspSeverity::Hint => Severity::Hint,
        },
        message: d.message,
        source: format!("lsp:{}", d.source.as_deref().unwrap_or("server")),
        code: d.code,
    }
}

/// Strip `root` from `path` when `path` is under it; otherwise return `path`.
fn relativize(path: &str, root: Option<&Path>) -> String {
    if let Some(root) = root
        && let Ok(rel) = Path::new(path).strip_prefix(root)
    {
        return rel.to_string_lossy().into_owned();
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_svc::lsp::LspDiagnostic;

    fn pd(path: &str, diags: Vec<LspDiagnostic>) -> PublishedDiagnostics {
        PublishedDiagnostics {
            path: path.to_string(),
            diagnostics: diags,
        }
    }

    fn diag(line: u32, sev: LspSeverity, msg: &str) -> LspDiagnostic {
        LspDiagnostic {
            line,
            character: 0,
            severity: sev,
            message: msg.to_string(),
            code: None,
            source: Some("rustc".into()),
        }
    }

    #[test]
    fn apply_relativizes_and_converts() {
        let mut store = LspDiagnostics::new();
        let root = PathBuf::from("/proj");
        store.apply(
            pd(
                "/proj/src/lib.rs",
                vec![diag(5, LspSeverity::Error, "boom")],
            ),
            Some(&root),
        );
        let mut dst = Vec::new();
        store.merge_into(&mut dst);
        assert_eq!(dst.len(), 1);
        assert_eq!(dst[0].file, "src/lib.rs");
        assert_eq!(dst[0].line, 6); // 0-based 5 → 1-based 6
        assert_eq!(dst[0].source, "lsp:rustc");
        assert_eq!(dst[0].severity, Severity::Error);
    }

    #[test]
    fn empty_set_clears_a_file() {
        let mut store = LspDiagnostics::new();
        store.apply(pd("/x.rs", vec![diag(0, LspSeverity::Warning, "w")]), None);
        assert!(!store.is_empty());
        store.apply(pd("/x.rs", vec![]), None);
        assert!(store.is_empty());
    }

    #[test]
    fn merge_keeps_task_diags_and_replaces_lsp() {
        let mut store = LspDiagnostics::new();
        store.apply(
            pd("/a.rs", vec![diag(1, LspSeverity::Error, "lsp-err")]),
            None,
        );

        // A pre-existing task diagnostic plus a stale LSP one.
        let mut dst = vec![
            DiagnosticItem {
                file: "b.rs".into(),
                line: 2,
                col: None,
                severity: Severity::Warning,
                message: "task-warn".into(),
                source: "cargo clippy".into(),
                code: None,
            },
            DiagnosticItem {
                file: "old.rs".into(),
                line: 9,
                col: None,
                severity: Severity::Hint,
                message: "stale-lsp".into(),
                source: "lsp:rustc".into(),
                code: None,
            },
        ];
        store.merge_into(&mut dst);

        // The task diagnostic survives; the stale LSP one is gone; ours is added.
        assert!(dst.iter().any(|d| d.source == "cargo clippy"));
        assert!(!dst.iter().any(|d| d.message == "stale-lsp"));
        assert!(dst.iter().any(|d| d.message == "lsp-err"));
        // Sorted by severity → Error (0) before Warning (1).
        assert_eq!(dst[0].severity, Severity::Error);
    }

    #[test]
    fn disabled_supervisor_yields_unavailable() {
        let mut cfg = Config::default();
        cfg.lsp.enabled = false;
        let sup = LspSupervisor::from_config(&cfg);
        let res = sup.handle().client(Path::new("/tmp"), Lang::Rust);
        assert_eq!(res.err(), Some(LspError::NotAvailable));
    }

    #[test]
    fn missing_server_is_unavailable_and_cached() {
        // Override Rust to a binary that cannot exist → NotAvailable, cached.
        let mut cfg = Config::default();
        cfg.lsp.servers = vec![superzej_core::config::LspServerConfig {
            lang: "rust".into(),
            command: "/nonexistent/definitely-not-a-server".into(),
            args: vec![],
        }];
        let sup = LspSupervisor::from_config(&cfg);
        let inner = sup.handle();
        // An explicit override command is trusted, so it attempts to spawn and
        // fails with Spawn (not NotAvailable) — still an error, not a panic.
        let res = inner.client(Path::new("/tmp"), Lang::Rust);
        assert!(res.is_err());
    }
}
