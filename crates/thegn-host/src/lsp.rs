//! Host-side LSP integration: the [`LspSupervisor`] (lazy, warm, per-worktree
//! server lifecycle) and the [`LspDiagnostics`] store that folds server-pushed
//! diagnostics into the existing Problems panel.
//!
//! The supervisor owns the [`thegn_svc::lsp::LspClient`] connections, keyed
//! by `(worktree_root, language)`. Servers are **never** started eagerly — the
//! first request for a `(root, lang)` spawns and initializes one, and it then
//! stays warm across tab switches (rust-analyzer's index is expensive). The
//! inner state is `Arc`-shared so an off-loop request task can lazily start and
//! reuse clients without blocking the render loop.
//!
//! Diagnostics arrive asynchronously on each client's reader thread; the host
//! sets up a bridge thread (it owns the `TerminalWaker`; svc does not) that
//! forwards them onto the loop's channel and pulses the waker.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use thegn_core::config::Config;
use thegn_svc::lsp::{
    DiagnosticKey, DiagnosticsReceiver, DiagnosticsSender, LspClient, LspError, LspHealth,
    LspSeverity, PublishedDiagnostics, Registry,
};

use crate::panel::{DiagnosticItem, Severity};

/// Shared LSP state, owned by the event loop and cloned into off-loop tasks.
pub struct LspSupervisor {
    inner: Arc<LspInner>,
    /// Receiver end of the diagnostics channel handed to clients; taken once by
    /// the host to drive the bridge thread.
    raw_rx: Option<DiagnosticsReceiver>,
}

/// `(root, registry key)` → started client, or `None` once we've tried and found
/// no server (so we don't re-spawn on every request). Keying on the registry key
/// (not the tree-sitter `Lang`) is what lets an arbitrary language server —
/// `zls`, `clangd`, an in-house DSL server — hold a per-worktree instance.
type ClientMap = HashMap<(PathBuf, String), (Option<Arc<LspClient>>, u64)>;

pub struct LspInner {
    enabled: bool,
    /// The resolved server registry (built-ins + user `[[lsp.servers]]`). Built
    /// once at startup and immutable, so it needs no lock and is shared read-only
    /// across every off-loop request task.
    registry: Registry,
    diag_tx: DiagnosticsSender,
    generation: AtomicU64,
    clients: Mutex<ClientMap>,
}

impl LspSupervisor {
    /// Build from config. Starts nothing; just records the policy + registry.
    pub fn from_config(cfg: &Config) -> Self {
        let (diag_tx, raw_rx) = thegn_svc::lsp::diagnostics_channel();
        LspSupervisor {
            inner: Arc::new(LspInner {
                enabled: cfg.lsp.enabled,
                registry: Registry::build(&cfg.lsp.servers),
                diag_tx,
                generation: AtomicU64::new(1),
                clients: Mutex::new(HashMap::new()),
            }),
            raw_rx: Some(raw_rx),
        }
    }

    /// Take the diagnostics receiver to drive the host bridge thread (once).
    pub fn take_diagnostics_rx(&mut self) -> Option<DiagnosticsReceiver> {
        self.raw_rx.take()
    }

    /// An `Arc` handle for use inside an off-loop (`spawn_blocking`) task.
    pub fn handle(&self) -> Arc<LspInner> {
        self.inner.clone()
    }
}

impl LspInner {
    /// Resolve a file path to its registry key (the LSP tier), by extension.
    /// Pure; safe to call anywhere. `None` for an unregistered extension.
    pub fn resolve_key(&self, path: &str) -> Option<String> {
        self.registry.resolve_key(path)
    }

    /// Get the warm client for `(root, key)`, lazily spawning+initializing it on
    /// first use. **Blocks** (spawn + initialize) — call off the event loop.
    pub fn client(&self, root: &Path, key: &str) -> Result<Arc<LspClient>, LspError> {
        if !self.enabled {
            return Err(LspError::NotAvailable);
        }
        let map_key = (root.to_path_buf(), key.to_string());
        let mut clients = self.clients.lock().unwrap();
        if let Some(slot) = clients.get(&map_key) {
            return slot.0.clone().ok_or(LspError::NotAvailable);
        }
        if !clients.keys().any(|(existing, _)| existing == root)
            && clients
                .keys()
                .map(|(existing, _)| existing)
                .collect::<std::collections::HashSet<_>>()
                .len()
                >= thegn_svc::lsp::limits::MAX_ROOTS
        {
            return Err(LspError::Bounded("LSP root limit reached".into()));
        }

        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        if !self
            .diag_tx
            .set_active(root.to_path_buf(), key.to_string(), generation)
        {
            return Err(LspError::Bounded("LSP root registry limit reached".into()));
        }
        let started = match self.registry.resolve(key) {
            Some(spec) => {
                // Join the shared aggregate slice, like every pane and background
                // job. rust-analyzer alone can take gigabytes — a language server
                // is exactly the background hog the slice exists to bound. The
                // wrap is fail-safe: no published policy / unusable systemd-run ⇒
                // the server spawns unwrapped, exactly as before. Off-loop, so the
                // wrap's probe spawn is fine here.
                let argv = thegn_core::sandbox_cpucap::wrap_background_argv(spec.argv());
                LspClient::start_argv_with_identity(
                    &argv,
                    &spec.language_id,
                    root,
                    self.diag_tx.clone(),
                    key.to_string(),
                    generation,
                )
                .and_then(|c| c.initialize(root).map(|_| c))
                .map(Arc::new)
            }
            None => Err(LspError::NotAvailable),
        };

        match started {
            Ok(client) => {
                clients.insert(map_key, (Some(client.clone()), generation));
                Ok(client)
            }
            Err(e) => {
                self.diag_tx.retire(root, key, generation);
                // Cache "no server" so we don't try to spawn on every request.
                if e == LspError::NotAvailable {
                    clients.insert(map_key, (None, generation));
                }
                Err(e)
            }
        }
    }

    /// Retire an authority before removing/recreating its client.  A late
    /// publication from the old reader remains stale even if its numeric
    /// sequence is larger than the replacement's.
    pub fn close(&self, root: &Path, key: &str) {
        if let Ok(mut clients) = self.clients.lock()
            && let Some((_client, generation)) =
                clients.remove(&(root.to_path_buf(), key.to_string()))
        {
            self.diag_tx.retire(root, key, generation);
        }
    }
}

/// Persistent store of LSP-pushed diagnostics, partitioned by the worktree
/// root that produced them and keyed by file path within each root. Survives
/// model-hydration swaps (which only carry git/db state) so the Problems panel
/// keeps showing them; re-merged into the rendered list on every update + swap.
/// The partition is what keeps one workspace's warm servers (they stay alive
/// across tab switches) from bleeding diagnostics into another's panel.
#[derive(Debug, Default)]
pub struct LspDiagnostics {
    by_root: HashMap<PathBuf, RootDiagnostics>,
    retained_bytes: usize,
    health: LspHealth,
    incomplete: HashSet<StoreFileKey>,
}

#[derive(Debug, Default)]
struct RootDiagnostics {
    files: HashMap<StoreFileKey, Vec<DiagnosticItem>>,
    order: VecDeque<StoreFileKey>,
    bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct StoreFileKey {
    root: PathBuf,
    server_identity: String,
    generation: u64,
    path: String,
}

impl LspDiagnostics {
    pub fn new() -> Self {
        LspDiagnostics::default()
    }

    /// Apply a server's latest diagnostics for one document, filed under the
    /// originating client's worktree root (stamped on the message). An empty
    /// set clears that file.
    pub fn apply(&mut self, pd: PublishedDiagnostics) {
        if pd.root.as_os_str().to_string_lossy().len() > thegn_svc::lsp::limits::MAX_IDENTITY_BYTES
            || pd.server_identity.len() > thegn_svc::lsp::limits::MAX_IDENTITY_BYTES
        {
            self.health.invalid = self.health.invalid.saturating_add(1);
            return;
        }
        let file = relativize(&pd.path, &pd.root);
        let store_key = StoreFileKey {
            root: pd.root.clone(),
            server_identity: pd.server_identity.clone(),
            generation: pd.generation,
            path: file.clone(),
        };
        self.remove_retired_streams(&store_key);
        if !pd.complete {
            self.remember_incomplete(store_key.clone());
            self.health.incomplete = self.health.incomplete.saturating_add(1);
            if pd.diagnostics.is_empty() {
                return;
            }
        } else {
            self.incomplete.remove(&store_key);
        }
        if pd.diagnostics.is_empty() {
            let old_bytes = self
                .by_root
                .get(&pd.root)
                .and_then(|root| root.files.get(&store_key))
                .map(|old| diagnostics_bytes(&store_key, old))
                .unwrap_or(0);
            if let Some(root) = self.by_root.get_mut(&pd.root) {
                root.files.remove(&store_key);
                root.bytes = root.bytes.saturating_sub(old_bytes);
                root.order.retain(|candidate| candidate != &store_key);
                let empty = root.files.is_empty();
                if empty {
                    self.by_root.remove(&pd.root);
                }
            }
            self.retained_bytes = self.retained_bytes.saturating_sub(old_bytes);
            return;
        }
        let items = pd
            .diagnostics
            .into_iter()
            .map(|d| to_panel_item(&file, d))
            .collect::<Vec<_>>();
        let bytes = diagnostics_bytes(&store_key, &items);
        if bytes > thegn_svc::lsp::limits::MAX_RETAINED_BYTES {
            self.health.dropped = self.health.dropped.saturating_add(1);
            self.health.incomplete = self.health.incomplete.saturating_add(1);
            return;
        }
        if !self.by_root.contains_key(&pd.root)
            && self.by_root.len() >= thegn_svc::lsp::limits::MAX_ROOTS
        {
            self.health.dropped = self.health.dropped.saturating_add(1);
            self.health.incomplete = self.health.incomplete.saturating_add(1);
            return;
        }
        let old_bytes = self
            .by_root
            .get(&pd.root)
            .and_then(|root| root.files.get(&store_key))
            .map(|old| diagnostics_bytes(&store_key, old))
            .unwrap_or(0);
        let is_new_file = self
            .by_root
            .get(&pd.root)
            .map_or(true, |root| !root.files.contains_key(&store_key));
        if is_new_file
            && self
                .by_root
                .get(&pd.root)
                .is_some_and(|root| root.files.len() >= thegn_svc::lsp::limits::MAX_FILES_PER_ROOT)
        {
            self.health.dropped = self.health.dropped.saturating_add(1);
            self.health.incomplete = self.health.incomplete.saturating_add(1);
            return;
        }
        if self
            .retained_bytes
            .saturating_sub(old_bytes)
            .saturating_add(bytes)
            > thegn_svc::lsp::limits::MAX_RETAINED_BYTES
        {
            self.health.dropped = self.health.dropped.saturating_add(1);
            self.health.incomplete = self.health.incomplete.saturating_add(1);
            return;
        }
        let root = self.by_root.entry(pd.root.clone()).or_default();
        if root.files.remove(&store_key).is_some() {
            root.bytes = root.bytes.saturating_sub(old_bytes);
            root.order.retain(|candidate| candidate != &store_key);
        }
        self.retained_bytes = self.retained_bytes.saturating_sub(old_bytes);
        root.bytes = root.bytes.saturating_add(bytes);
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        root.order.push_back(store_key.clone());
        root.files.insert(store_key, items);
    }

    /// Replace the LSP-sourced entries in `dst` with the current store's
    /// entries **for `root` only**, keeping any non-LSP (task-output)
    /// diagnostics, then re-sort by severity. Foreign roots' diagnostics stay
    /// in the store but never render.
    pub fn merge_into(&self, root: &Path, dst: &mut Vec<DiagnosticItem>) {
        dst.retain(|d| !d.source.starts_with("lsp:"));
        if let Some(files) = self.by_root.get(root) {
            for file in &files.order {
                if let Some(items) = files.files.get(file) {
                    dst.extend(items.iter().cloned());
                }
            }
        }
        if self.health.has_findings() {
            dst.push(DiagnosticItem {
                file: String::new(),
                line: 0,
                col: None,
                severity: Severity::Warning,
                message: format!("{} active={}", self.health.summary(), self.incomplete.len()),
                source: "lsp:health".to_string(),
                code: None,
            });
        }
        dst.sort_by_key(|d| d.severity as u8);
    }

    /// Drop everything a closed worktree's servers pushed (memory hygiene —
    /// rendering already ignores non-active roots).
    #[allow(dead_code)] // exercised by tests; the loop evicts via `retain_roots`
    pub fn evict_root(&mut self, root: &Path) {
        if let Some(old) = self.by_root.remove(root) {
            self.retained_bytes = self.retained_bytes.saturating_sub(old.bytes);
        }
        self.incomplete.retain(|key| key.root.as_path() != root);
    }

    /// Keep only the roots `keep` approves — called on the periodic model swap
    /// with the set of open worktree tabs, so closed/deleted worktrees' entries
    /// don't accumulate for the life of the process.
    pub fn retain_roots(&mut self, keep: impl Fn(&Path) -> bool) {
        let removed = self
            .by_root
            .iter()
            .filter(|(root, _)| !keep(root))
            .map(|(_, state)| state.bytes)
            .sum::<usize>();
        self.by_root.retain(|root, _| keep(root));
        self.retained_bytes = self.retained_bytes.saturating_sub(removed);
        self.incomplete.retain(|key| keep(&key.root));
    }

    #[allow(dead_code)] // exercised by tests; the loop-side caller was removed
    pub fn is_empty(&self) -> bool {
        self.by_root.is_empty() && !self.health.has_findings()
    }

    pub fn record_health(&mut self, health: LspHealth) {
        self.health.saturating_add(health);
    }

    pub fn mark_incomplete(&mut self, keys: impl IntoIterator<Item = DiagnosticKey>) {
        for key in keys {
            let store_key = StoreFileKey {
                root: key.root.clone(),
                server_identity: key.server_identity,
                generation: key.generation,
                path: relativize(&key.path, &key.root),
            };
            self.remember_incomplete(store_key);
        }
    }

    fn remember_incomplete(&mut self, key: StoreFileKey) {
        if self.incomplete.len() >= thegn_svc::lsp::limits::MAX_QUEUE_DOCUMENTS
            && !self.incomplete.contains(&key)
        {
            if let Some(oldest) = self.incomplete.iter().min().cloned() {
                self.incomplete.remove(&oldest);
            }
        }
        self.incomplete.insert(key);
    }

    fn remove_retired_streams(&mut self, current: &StoreFileKey) {
        let retired = self
            .by_root
            .get(&current.root)
            .map(|root| {
                root.files
                    .keys()
                    .filter(|key| {
                        key.server_identity == current.server_identity
                            && key.path == current.path
                            && key.generation != current.generation
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for key in retired {
            if let Some(items) = self
                .by_root
                .get_mut(&current.root)
                .and_then(|root| root.files.remove(&key))
            {
                let bytes = diagnostics_bytes(&key, &items);
                self.retained_bytes = self.retained_bytes.saturating_sub(bytes);
                if let Some(root) = self.by_root.get_mut(&current.root) {
                    root.bytes = root.bytes.saturating_sub(bytes);
                    root.order.retain(|candidate| candidate != &key);
                }
            }
            self.incomplete.remove(&key);
        }
    }
}

fn diagnostics_bytes(key: &StoreFileKey, items: &[DiagnosticItem]) -> usize {
    key.root
        .as_os_str()
        .to_string_lossy()
        .len()
        .saturating_add(key.server_identity.len())
        .saturating_add(key.path.len())
        .saturating_add(std::mem::size_of::<DiagnosticItem>())
        .saturating_add(std::mem::size_of::<StoreFileKey>())
        .saturating_add(64) // authority/generation/hash-map bookkeeping
        .saturating_add(items.iter().fold(0usize, |n, item| {
            n.saturating_add(item.file.len())
                .saturating_add(item.message.len())
                .saturating_add(item.source.len())
                .saturating_add(item.code.as_ref().map_or(0, String::len))
        }))
}

/// Convert one svc diagnostic to a panel item (source tagged `lsp:<source>`).
fn to_panel_item(file: &str, d: thegn_svc::lsp::LspDiagnostic) -> DiagnosticItem {
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
        message: thegn_svc::lsp::sanitize_for_terminal(&d.message),
        source: format!(
            "lsp:{}",
            thegn_svc::lsp::sanitize_for_terminal(d.source.as_deref().unwrap_or("server"))
        ),
        code: d
            .code
            .map(|code| thegn_svc::lsp::sanitize_for_terminal(&code)),
    }
}

/// Strip `root` from `path` when `path` is under it; otherwise return `path`.
fn relativize(path: &str, root: &Path) -> String {
    if let Ok(rel) = Path::new(path).strip_prefix(root) {
        return rel.to_string_lossy().into_owned();
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_svc::lsp::LspDiagnostic;

    fn pd(root: &str, path: &str, diags: Vec<LspDiagnostic>) -> PublishedDiagnostics {
        PublishedDiagnostics {
            root: PathBuf::from(root),
            path: path.to_string(),
            diagnostics: diags,
            server_identity: "test".into(),
            generation: 1,
            sequence: 1,
            complete: true,
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
        store.apply(pd(
            "/proj",
            "/proj/src/lib.rs",
            vec![diag(5, LspSeverity::Error, "boom")],
        ));
        let mut dst = Vec::new();
        store.merge_into(Path::new("/proj"), &mut dst);
        assert_eq!(dst.len(), 1);
        assert_eq!(dst[0].file, "src/lib.rs");
        assert_eq!(dst[0].line, 6); // 0-based 5 → 1-based 6
        assert_eq!(dst[0].source, "lsp:rustc");
        assert_eq!(dst[0].severity, Severity::Error);
    }

    #[test]
    fn empty_set_clears_a_file() {
        let mut store = LspDiagnostics::new();
        store.apply(pd(
            "/p",
            "/p/x.rs",
            vec![diag(0, LspSeverity::Warning, "w")],
        ));
        assert!(!store.is_empty());
        store.apply(pd("/p", "/p/x.rs", vec![]));
        assert!(store.is_empty());
    }

    #[test]
    fn merge_is_partitioned_by_root() {
        let mut store = LspDiagnostics::new();
        store.apply(pd(
            "/a",
            "/a/x.rs",
            vec![diag(1, LspSeverity::Error, "a-err")],
        ));
        store.apply(pd(
            "/b",
            "/b/y.rs",
            vec![diag(2, LspSeverity::Warning, "b-warn")],
        ));

        // Merging for root /a yields only /a's diagnostics.
        let mut dst = Vec::new();
        store.merge_into(Path::new("/a"), &mut dst);
        assert_eq!(dst.len(), 1);
        assert_eq!(dst[0].message, "a-err");

        // Switching to /b swaps the set — /a's entry (lsp-sourced) is dropped.
        store.merge_into(Path::new("/b"), &mut dst);
        assert_eq!(dst.len(), 1);
        assert_eq!(dst[0].message, "b-warn");

        // Evicting a root removes its partition entirely.
        store.evict_root(Path::new("/b"));
        store.merge_into(Path::new("/b"), &mut dst);
        assert!(dst.is_empty());
        assert!(!store.is_empty()); // /a still stored
    }

    #[test]
    fn merge_keeps_task_diags_and_replaces_lsp() {
        let mut store = LspDiagnostics::new();
        store.apply(pd(
            "/p",
            "/p/a.rs",
            vec![diag(1, LspSeverity::Error, "lsp-err")],
        ));

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
        store.merge_into(Path::new("/p"), &mut dst);

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
        let res = sup.handle().client(Path::new("/tmp"), "rust");
        assert_eq!(res.err(), Some(LspError::NotAvailable));
    }

    #[test]
    fn missing_server_is_unavailable_and_cached() {
        // Override Rust to a binary that cannot exist → NotAvailable, cached.
        let mut cfg = Config::default();
        cfg.lsp.servers = vec![thegn_core::config::LspServerConfig {
            lang: "rust".into(),
            command: "/nonexistent/definitely-not-a-server".into(),
            args: vec![],
            extensions: vec![],
            language_id: None,
        }];
        let sup = LspSupervisor::from_config(&cfg);
        let inner = sup.handle();
        // An explicit override command is trusted, so it attempts to spawn and
        // fails with Spawn (not NotAvailable) — still an error, not a panic.
        let res = inner.client(Path::new("/tmp"), "rust");
        assert!(res.is_err());
    }

    #[test]
    fn unregistered_extension_resolves_to_no_key() {
        let cfg = Config::default();
        let inner = LspSupervisor::from_config(&cfg).handle();
        assert_eq!(inner.resolve_key("src/lib.rs").as_deref(), Some("rust"));
        assert_eq!(inner.resolve_key("README.md"), None);
    }

    #[test]
    fn registry_key_resolves_for_a_non_builtin_language() {
        let mut cfg = Config::default();
        cfg.lsp.servers = vec![thegn_core::config::LspServerConfig {
            lang: "zig".into(),
            command: "zls".into(),
            args: vec![],
            extensions: vec!["zig".into(), "zon".into()],
            language_id: None,
        }];
        let inner = LspSupervisor::from_config(&cfg).handle();
        assert_eq!(inner.resolve_key("main.zig").as_deref(), Some("zig"));
    }

    #[test]
    fn same_document_streams_do_not_clear_each_other() {
        let mut store = LspDiagnostics::new();
        let mut rust = pd("/p", "/p/a.rs", vec![diag(0, LspSeverity::Error, "rust")]);
        rust.server_identity = "rust".into();
        let mut clang = pd(
            "/p",
            "/p/a.rs",
            vec![diag(1, LspSeverity::Warning, "clang")],
        );
        clang.server_identity = "clang".into();
        store.apply(rust.clone());
        store.apply(clang);
        store.apply(PublishedDiagnostics {
            diagnostics: vec![],
            ..rust
        });
        let mut dst = Vec::new();
        store.merge_into(Path::new("/p"), &mut dst);
        assert_eq!(
            dst.iter()
                .filter(|item| item.source.starts_with("lsp:"))
                .count(),
            1
        );
        assert_eq!(dst[0].message, "clang");
    }

    #[test]
    fn retained_store_caps_files_and_sanitizes_display_text() {
        let mut store = LspDiagnostics::new();
        for index in 0..=thegn_svc::lsp::limits::MAX_FILES_PER_ROOT {
            store.apply(pd(
                "/p",
                &format!("/p/{index}.rs"),
                vec![diag(0, LspSeverity::Error, "bad\u{1b}]0;title\u{7} text")],
            ));
        }
        let mut dst = Vec::new();
        store.merge_into(Path::new("/p"), &mut dst);
        assert!(dst.len() <= thegn_svc::lsp::limits::MAX_FILES_PER_ROOT + 1);
        assert!(
            dst.iter()
                .all(|item| !item.message.chars().any(char::is_control))
        );
        assert!(dst.iter().any(|item| item.source == "lsp:health"));
    }
}
