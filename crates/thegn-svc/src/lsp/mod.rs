//! Language Server Protocol client substrate (roadmap item 529).
//!
//! One [`LspClient`] owns a single long-lived language-server subprocess and
//! speaks JSON-RPC over its stdio. The transport is deliberately **synchronous
//! with a reader thread**: a dedicated thread parses framed messages, routes
//! responses to the waiting caller by request id, and forwards
//! `publishDiagnostics` notifications out a channel — the same off-loop-producer
//! shape the host already uses for PTY readers and fs-watchers.
//!
//! We hand-roll a minimal slice of the protocol types rather than depend on
//! `lsp-types`: it keeps the dependency footprint small and lets the mapping
//! layer parse server responses *defensively* (tolerating the Location vs
//! LocationLink and DocumentSymbol vs SymbolInformation unions, and assorted
//! server quirks) instead of failing on strict typed deserialization.
//!
//! Lifecycle (lazy start, warm reuse, shutdown-on-drop) is owned a layer up by
//! the host's `LspSupervisor`; this module is just one connection.

pub mod framing;
pub mod registry;

pub use registry::{Registry, RegistryEntry, Resolution, binary_on_path};

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

/// The LSP resource contract.  These are protocol safety limits, not user
/// configuration: every edge is inclusive and the first byte/item over an
/// edge is rejected or dropped with health recorded.
pub mod limits {
    pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
    pub const MAX_JSON_DEPTH: usize = 64;
    pub const MAX_JSON_NODES: usize = 65_536;
    pub const MAX_CONTAINER_ITEMS: usize = 4096;
    pub const MAX_SCALAR_STRING_BYTES: usize = 64 * 1024;
    pub const MAX_IDENTITY_BYTES: usize = 4 * 1024;
    pub const MAX_CODE_SOURCE_BYTES: usize = 4 * 1024;
    pub const MAX_PROJECTED_RESPONSE_BYTES: usize = 1024 * 1024;
    pub const MAX_HOVER_MARKDOWN_BYTES: usize = 256 * 1024;
    pub const MAX_RESULT_ITEMS: usize = 4096;
    pub const MAX_DIAGNOSTICS: usize = 512;
    pub const MAX_DIAGNOSTIC_BYTES: usize = 256 * 1024;
    pub const MAX_QUEUE_DOCUMENTS: usize = 256;
    pub const MAX_QUEUE_BYTES: usize = 8 * 1024 * 1024;
    pub const MAX_ROOTS: usize = 32;
    pub const MAX_FILES_PER_ROOT: usize = 4096;
    pub const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LspHealth {
    pub dropped: u64,
    pub truncated: u64,
    pub stale: u64,
    pub incomplete: u64,
    pub invalid: u64,
}

impl LspHealth {
    pub fn saturating_add(&mut self, other: LspHealth) {
        self.dropped = self.dropped.saturating_add(other.dropped);
        self.truncated = self.truncated.saturating_add(other.truncated);
        self.stale = self.stale.saturating_add(other.stale);
        self.incomplete = self.incomplete.saturating_add(other.incomplete);
        self.invalid = self.invalid.saturating_add(other.invalid);
    }

    pub fn has_findings(self) -> bool {
        self != Self::default()
    }

    pub fn summary(self) -> String {
        // Keep this deliberately static/bounded: it is rendered as a Problems
        // row and must never become a server-controlled text sink.
        format!(
            "LSP incomplete: dropped={} truncated={} stale={} incomplete={} invalid={}",
            self.dropped, self.truncated, self.stale, self.incomplete, self.invalid
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedDiagnostics {
    pub root: PathBuf,
    pub path: String,
    pub diagnostics: Vec<LspDiagnostic>,
    pub server_identity: String,
    pub generation: u64,
    pub sequence: u64,
    /// False means the publication was bounded or malformed and cannot
    /// authoritatively clear the previous document state.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DiagnosticKey {
    pub root: PathBuf,
    pub server_identity: String,
    pub generation: u64,
    pub path: String,
}

#[derive(Debug, Default)]
struct QueueState {
    pending: HashMap<DiagnosticKey, PublishedDiagnostics>,
    order: VecDeque<DiagnosticKey>,
    last_root: Option<PathBuf>,
    wake_pending: bool,
    bytes: usize,
    health: LspHealth,
    incomplete: HashMap<DiagnosticKey, ()>,
    last_sequence: HashMap<DiagnosticKey, u64>,
}

impl QueueState {
    fn mark_incomplete(&mut self, key: DiagnosticKey) {
        if self.incomplete.len() >= limits::MAX_QUEUE_DOCUMENTS
            && !self.incomplete.contains_key(&key)
        {
            if let Some(oldest) = self.incomplete.keys().min().cloned() {
                self.incomplete.remove(&oldest);
            }
        }
        self.incomplete.insert(key, ());
    }
}

/// A non-blocking latest-per-document diagnostics bus.  Reader threads never
/// wait for the UI: replacements are cheap, new keys use deterministic oldest
/// eviction, and every loss is sticky health rather than a silent clean state.
#[derive(Clone, Debug)]
pub struct DiagnosticsSender {
    state: Arc<Mutex<QueueState>>,
    active: Arc<Mutex<HashMap<(PathBuf, String), u64>>>,
    notify: Arc<Condvar>,
}

#[derive(Debug, Clone)]
pub struct DiagnosticsReceiver {
    state: Arc<Mutex<QueueState>>,
    active: Arc<Mutex<HashMap<(PathBuf, String), u64>>>,
    notify: Arc<Condvar>,
}

pub fn diagnostics_channel() -> (DiagnosticsSender, DiagnosticsReceiver) {
    let state = Arc::new(Mutex::new(QueueState::default()));
    let active = Arc::new(Mutex::new(HashMap::new()));
    let notify = Arc::new(Condvar::new());
    (
        DiagnosticsSender {
            state: state.clone(),
            active: active.clone(),
            notify: notify.clone(),
        },
        DiagnosticsReceiver {
            state,
            active,
            notify,
        },
    )
}

pub fn published_diagnostics_bytes(pd: &PublishedDiagnostics) -> usize {
    let diagnostic_bytes = pd.diagnostics.iter().fold(0usize, |n, d| {
        n.saturating_add(d.message.len())
            .saturating_add(d.code.as_ref().map_or(0, String::len))
            .saturating_add(d.source.as_ref().map_or(0, String::len))
            .saturating_add(std::mem::size_of::<LspDiagnostic>())
    });
    pd.root
        .as_os_str()
        .to_string_lossy()
        .len()
        .saturating_add(pd.path.len())
        .saturating_add(pd.server_identity.len())
        .saturating_add(diagnostic_bytes)
        .saturating_add(std::mem::size_of::<PublishedDiagnostics>())
}

impl DiagnosticsSender {
    pub fn set_active(&self, root: PathBuf, identity: String, generation: u64) -> bool {
        if let Ok(mut active) = self.active.lock() {
            while active.len() >= limits::MAX_ROOTS * limits::MAX_QUEUE_DOCUMENTS
                && !active.contains_key(&(root.clone(), identity.clone()))
            {
                let Some(retired) = active
                    .iter()
                    .filter(|(_, generation)| **generation == u64::MAX)
                    .map(|(authority, _)| authority.clone())
                    .min()
                else {
                    break;
                };
                active.remove(&retired);
            }
            if !active.keys().any(|(existing, _)| existing == &root) {
                while active
                    .keys()
                    .map(|(existing, _)| existing)
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    >= limits::MAX_ROOTS
                {
                    let Some(retired_root) = active
                        .iter()
                        .filter(|(_, generation)| **generation == u64::MAX)
                        .map(|((root, _), _)| root.clone())
                        .min()
                    else {
                        return false;
                    };
                    active.retain(|(existing, _), generation| {
                        existing != &retired_root || *generation != u64::MAX
                    });
                }
            }
            if !active.contains_key(&(root.clone(), identity.clone()))
                && active
                    .keys()
                    .filter(|(existing, _)| existing == &root)
                    .count()
                    >= limits::MAX_QUEUE_DOCUMENTS
            {
                return false;
            }
            active.insert((root, identity), generation);
            return true;
        }
        false
    }

    pub fn retire(&self, root: &Path, identity: &str, generation: u64) {
        if let Ok(mut active) = self.active.lock()
            && active.get(&(root.to_path_buf(), identity.to_string())) == Some(&generation)
        {
            active.insert((root.to_path_buf(), identity.to_string()), u64::MAX);
        }
    }

    pub fn record(&self, health: LspHealth) {
        if let Ok(mut state) = self.state.lock() {
            state.health.saturating_add(health);
            state.wake_pending = true;
        }
        self.notify.notify_all();
    }

    pub fn publish(&self, pd: PublishedDiagnostics) {
        let authority = (pd.root.clone(), pd.server_identity.clone());
        let key = DiagnosticKey {
            root: pd.root.clone(),
            server_identity: pd.server_identity.clone(),
            generation: pd.generation,
            path: pd.path.clone(),
        };
        let bytes = published_diagnostics_bytes(&pd);
        let complete = pd.complete;
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let active = self
            .active
            .lock()
            .ok()
            .and_then(|g| g.get(&authority).copied());
        let supervised = !pd.server_identity.is_empty() || pd.generation != 0;
        if supervised && active != Some(pd.generation) {
            state.bytes = state.bytes.saturating_sub(published_diagnostics_bytes(&pd));
            state.health.stale = state.health.stale.saturating_add(1);
            state.wake_pending = true;
            self.notify.notify_all();
            return;
        }
        if let Some(previous) = state.last_sequence.get(&key)
            && pd.sequence <= *previous
        {
            state.health.stale = state.health.stale.saturating_add(1);
            state.wake_pending = true;
            self.notify.notify_all();
            return;
        }
        if state.last_sequence.len() >= limits::MAX_QUEUE_DOCUMENTS
            && !state.last_sequence.contains_key(&key)
        {
            if let Some(oldest) = state.last_sequence.keys().min().cloned() {
                state.last_sequence.remove(&oldest);
            }
        }
        state.last_sequence.insert(key.clone(), pd.sequence);
        if let Some(old) = state.pending.get(&key) {
            let old_bytes = published_diagnostics_bytes(old);
            if state.bytes.saturating_sub(old_bytes).saturating_add(bytes) > limits::MAX_QUEUE_BYTES
            {
                state.mark_incomplete(key);
                state.health.dropped = state.health.dropped.saturating_add(1);
                state.health.incomplete = state.health.incomplete.saturating_add(1);
                state.wake_pending = true;
                self.notify.notify_all();
                return;
            }
            let _old = state
                .pending
                .insert(key.clone(), pd)
                .expect("checked above");
            state.bytes = state.bytes.saturating_sub(old_bytes);
            state.bytes = state.bytes.saturating_add(bytes);
            if complete {
                state.incomplete.remove(&key);
            } else {
                state.mark_incomplete(key);
                state.health.incomplete = state.health.incomplete.saturating_add(1);
            }
            state.wake_pending = true;
            self.notify.notify_all();
            return;
        }
        while state.pending.len() >= limits::MAX_QUEUE_DOCUMENTS
            || state.bytes.saturating_add(bytes) > limits::MAX_QUEUE_BYTES
        {
            let Some(evict) = oldest_key_for_root(&state, &key.root) else {
                break;
            };
            if let Some(old) = state.pending.remove(&evict) {
                state.bytes = state
                    .bytes
                    .saturating_sub(published_diagnostics_bytes(&old));
                state.order.retain(|candidate| candidate != &evict);
                state.mark_incomplete(evict);
                state.health.dropped = state.health.dropped.saturating_add(1);
                state.health.incomplete = state.health.incomplete.saturating_add(1);
            } else {
                break;
            }
        }
        if state.pending.len() >= limits::MAX_QUEUE_DOCUMENTS
            || state.bytes.saturating_add(bytes) > limits::MAX_QUEUE_BYTES
        {
            state.mark_incomplete(key);
            state.health.dropped = state.health.dropped.saturating_add(1);
            state.health.incomplete = state.health.incomplete.saturating_add(1);
            state.wake_pending = true;
            self.notify.notify_all();
            return;
        }
        state.bytes = state.bytes.saturating_add(bytes);
        state.order.push_back(key.clone());
        state.pending.insert(key.clone(), pd);
        if !complete {
            state.mark_incomplete(key);
            state.health.incomplete = state.health.incomplete.saturating_add(1);
        }
        state.wake_pending = true;
        self.notify.notify_all();
    }
}

pub trait DiagnosticSink: Send + Sync {
    fn publish(&self, publication: PublishedDiagnostics);
    fn record(&self, health: LspHealth);
}

impl DiagnosticSink for DiagnosticsSender {
    fn publish(&self, publication: PublishedDiagnostics) {
        DiagnosticsSender::publish(self, publication);
    }
    fn record(&self, health: LspHealth) {
        DiagnosticsSender::record(self, health);
    }
}

impl DiagnosticSink for Sender<PublishedDiagnostics> {
    fn publish(&self, publication: PublishedDiagnostics) {
        let _ = self.send(publication);
    }
    fn record(&self, _health: LspHealth) {}
}

fn oldest_key_for_root(state: &QueueState, incoming_root: &Path) -> Option<DiagnosticKey> {
    let root = state
        .pending
        .keys()
        .filter(|key| key.root == incoming_root)
        .map(|key| key.root.clone())
        .next()
        .or_else(|| {
            state
                .pending
                .keys()
                .fold(None, |best: Option<(PathBuf, usize)>, key| {
                    let count = state
                        .pending
                        .keys()
                        .filter(|other| other.root == key.root)
                        .count();
                    match best {
                        None => Some((key.root.clone(), count)),
                        Some((root, n)) if count > n || (count == n && key.root < root) => {
                            Some((key.root.clone(), count))
                        }
                        other => other,
                    }
                })
                .map(|(root, _)| root)
        });
    root.and_then(|root| state.order.iter().find(|key| key.root == root).cloned())
}

impl DiagnosticsReceiver {
    pub fn wait(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        while !state.wake_pending {
            state = match self.notify.wait(state) {
                Ok(state) => state,
                Err(_) => return,
            };
        }
        state.wake_pending = false;
    }

    /// Re-arm the bridge wake after the host deliberately stopped at its
    /// per-turn budget while queued publications remain.
    pub fn rearm_if_pending(&self) {
        if let Ok(mut state) = self.state.lock()
            && !state.order.is_empty()
        {
            state.wake_pending = true;
            self.notify.notify_all();
        }
    }

    pub fn try_recv(&self) -> Result<PublishedDiagnostics, TryRecvError> {
        let Ok(mut state) = self.state.lock() else {
            return Err(TryRecvError::Disconnected);
        };
        let index = state
            .order
            .iter()
            .position(|key| {
                state
                    .last_root
                    .as_ref()
                    .is_none_or(|root| &key.root != root)
            })
            .unwrap_or(0);
        let Some(key) = state.order.remove(index) else {
            return Err(TryRecvError::Empty);
        };
        let Some(pd) = state.pending.remove(&key) else {
            return Err(TryRecvError::Empty);
        };
        let supervised = !pd.server_identity.is_empty() || pd.generation != 0;
        let active = self.active.lock().ok().and_then(|active| {
            active
                .get(&(pd.root.clone(), pd.server_identity.clone()))
                .copied()
        });
        if supervised && active != Some(pd.generation) {
            state.health.stale = state.health.stale.saturating_add(1);
            state.wake_pending = true;
            self.notify.notify_all();
            return Err(TryRecvError::Empty);
        }
        state.last_root = Some(key.root.clone());
        state.bytes = state.bytes.saturating_sub(published_diagnostics_bytes(&pd));
        Ok(pd)
    }

    pub fn health(&self) -> LspHealth {
        self.state
            .lock()
            .map(|state| state.health)
            .unwrap_or_default()
    }

    pub fn take_health(&self) -> LspHealth {
        self.state
            .lock()
            .map(|mut state| std::mem::take(&mut state.health))
            .unwrap_or_default()
    }

    pub fn take_incomplete(&self) -> Vec<DiagnosticKey> {
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        state.incomplete.drain().map(|(key, _)| key).collect()
    }
}

/// Why an LSP operation could not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspError {
    /// No server is configured / installed for the language.
    NotAvailable,
    /// The server process failed to spawn.
    Spawn(String),
    /// No response arrived before the deadline.
    Timeout,
    /// The server returned an error, or the stream broke.
    Protocol(String),
    /// A local resource bound rejected a server publication or response.
    Bounded(String),
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LspError::NotAvailable => write!(f, "no language server available"),
            LspError::Spawn(e) => write!(f, "language server spawn failed: {e}"),
            LspError::Timeout => write!(f, "language server request timed out"),
            LspError::Protocol(e) => write!(f, "language server protocol error: {e}"),
            LspError::Bounded(e) => write!(f, "language server resource limit: {e}"),
        }
    }
}

impl std::error::Error for LspError {}

// ─── protocol value types (0-based, LSP-native, until the UI boundary) ──────

/// A 0-based document position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// A half-open document range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// A resolved location: a repo/abs file path plus a range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: String,
    pub range: Range,
}

impl Location {
    /// The 1-based start line, for UI / `open-file:` keys.
    pub fn line_1based(&self) -> u32 {
        self.range.start.line.saturating_add(1)
    }
}

/// A compact symbol kind (the subset we render; everything else → `Other`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    File,
    Module,
    Namespace,
    Class,
    Method,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    Struct,
    EnumMember,
    TypeParameter,
    Other,
}

impl SymbolKind {
    /// Map the LSP `SymbolKind` integer (1–26) to our compact kind.
    pub fn from_lsp(n: i64) -> SymbolKind {
        match n {
            1 => SymbolKind::File,
            2 => SymbolKind::Module,
            3 => SymbolKind::Namespace,
            5 => SymbolKind::Class,
            6 => SymbolKind::Method,
            8 => SymbolKind::Field,
            9 => SymbolKind::Constructor,
            10 => SymbolKind::Enum,
            11 => SymbolKind::Interface,
            12 => SymbolKind::Function,
            13 => SymbolKind::Variable,
            14 => SymbolKind::Constant,
            23 => SymbolKind::Struct,
            22 => SymbolKind::EnumMember,
            26 => SymbolKind::TypeParameter,
            _ => SymbolKind::Other,
        }
    }

    /// A short label ("fn", "struct", …) for the panel/search UIs.
    pub fn label(self) -> &'static str {
        match self {
            SymbolKind::File => "file",
            SymbolKind::Module => "mod",
            SymbolKind::Namespace => "ns",
            SymbolKind::Class => "class",
            SymbolKind::Method => "method",
            SymbolKind::Field => "field",
            SymbolKind::Constructor => "ctor",
            SymbolKind::Enum => "enum",
            SymbolKind::Interface => "interface",
            SymbolKind::Function => "fn",
            SymbolKind::Variable => "var",
            SymbolKind::Constant => "const",
            SymbolKind::Struct => "struct",
            SymbolKind::EnumMember => "variant",
            SymbolKind::TypeParameter => "typaram",
            SymbolKind::Other => "sym",
        }
    }
}

/// A symbol with its definition location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: SymbolKind,
    pub location: Location,
    pub container: Option<String>,
}

/// Diagnostic severity, mirroring LSP's 1–4 scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

impl LspSeverity {
    pub fn from_lsp(n: i64) -> LspSeverity {
        match n {
            1 => LspSeverity::Error,
            2 => LspSeverity::Warning,
            3 => LspSeverity::Info,
            _ => LspSeverity::Hint,
        }
    }
}

/// One diagnostic from `textDocument/publishDiagnostics`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDiagnostic {
    pub line: u32,
    pub character: u32,
    pub severity: LspSeverity,
    pub message: String,
    pub code: Option<String>,
    pub source: Option<String>,
}

/// Resolved hover content (already flattened to markdown).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverInfo {
    pub markdown: String,
    pub range: Option<Range>,
}

/// One signature option from `textDocument/signatureHelp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureInfo {
    pub label: String,
    pub doc: Option<String>,
}

/// One offered code action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeActionInfo {
    pub title: String,
    pub kind: Option<String>,
}

// ─── server spec ─────────────────────────────────────────────────────────────

/// A resolved, launchable language server: its registry key, the `didOpen`
/// languageId, and the command/args. Produced by [`registry::Registry::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// The registry key ("rust", "zig", …) this server serves.
    pub key: String,
    /// The `languageId` sent in `textDocument/didOpen`.
    pub language_id: String,
    pub command: String,
    pub args: Vec<String>,
}

impl ServerSpec {
    /// The full argv (`[command, args…]`), for spawning or wrapping.
    pub fn argv(&self) -> Vec<String> {
        let mut v = Vec::with_capacity(self.args.len() + 1);
        v.push(self.command.clone());
        v.extend(self.args.iter().cloned());
        v
    }
}

// ─── capability negotiation ──────────────────────────────────────────────────

/// An optional request method whose availability the server declares in its
/// `initialize` result. thegn never sends a request whose provider the server
/// did not declare — it fails with [`LspError::NotAvailable`] instead, flowing
/// into the consumer's documented fallback exactly as a missing server does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspMethod {
    DocumentSymbol,
    WorkspaceSymbol,
    Definition,
    References,
    Hover,
    SignatureHelp,
    CodeAction,
}

/// The negotiated capabilities of one server, parsed from its `initialize`
/// result. Every field defaults to `false`, so an absent or malformed
/// `capabilities` object gates *every* optional method off rather than sending
/// blind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    pub document_symbol: bool,
    pub workspace_symbol: bool,
    pub definition: bool,
    pub references: bool,
    pub hover: bool,
    pub signature_help: bool,
    pub code_action: bool,
}

impl ServerCapabilities {
    /// Parse from the `initialize` result. The `capabilities` object may be
    /// absent (⇒ nothing supported); each provider field is the LSP
    /// boolean-or-object union — `true` or any options object ⇒ supported,
    /// `false`/`null`/absent ⇒ not.
    pub fn from_initialize_result(result: &Value) -> ServerCapabilities {
        let caps = result.get("capabilities");
        let flag = |field: &str| caps.map(|c| provider_declared(c, field)).unwrap_or(false);
        ServerCapabilities {
            document_symbol: flag("documentSymbolProvider"),
            workspace_symbol: flag("workspaceSymbolProvider"),
            definition: flag("definitionProvider"),
            references: flag("referencesProvider"),
            hover: flag("hoverProvider"),
            signature_help: flag("signatureHelpProvider"),
            code_action: flag("codeActionProvider"),
        }
    }

    /// Whether the server declared support for `method`.
    pub fn supports(&self, method: LspMethod) -> bool {
        match method {
            LspMethod::DocumentSymbol => self.document_symbol,
            LspMethod::WorkspaceSymbol => self.workspace_symbol,
            LspMethod::Definition => self.definition,
            LspMethod::References => self.references,
            LspMethod::Hover => self.hover,
            LspMethod::SignatureHelp => self.signature_help,
            LspMethod::CodeAction => self.code_action,
        }
    }
}

/// Whether a provider field in a `capabilities` object counts as supported:
/// `true` or any options object ⇒ yes; `false`, `null`, or absent ⇒ no.
fn provider_declared(caps: &Value, field: &str) -> bool {
    match caps.get(field) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

// ─── uri ⇄ path ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProjectionHealth {
    truncated: bool,
    invalid: bool,
}

impl ProjectionHealth {
    fn record(self, sink: &dyn DiagnosticSink) {
        sink.record(LspHealth {
            truncated: u64::from(self.truncated),
            invalid: u64::from(self.invalid),
            ..LspHealth::default()
        });
    }
}

fn bounded_string(value: &str, limit: usize, health: &mut ProjectionHealth) -> Option<String> {
    if value.len() > limit {
        health.truncated = true;
        return None;
    }
    Some(sanitize_for_terminal(value))
}

/// Remove terminal controls, including OSC strings, before any projected text
/// reaches the host renderer.  Source identity is handled separately and is
/// never passed through this display projection.
pub fn sanitize_for_terminal(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&']') {
                let _ = chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        let _ = chars.next();
                        break;
                    }
                }
            } else if chars.peek() == Some(&'\\') {
                let _ = chars.next();
            }
            continue;
        }
        if c == '\u{7f}' || (c.is_control() && !matches!(c, '\n' | '\t')) {
            continue;
        }
        out.push(c);
    }
    out
}

/// A streaming preflight over raw JSON. It counts containers, keys, values and
/// decoded string expansion without constructing a `serde_json::Value`.
pub fn preflight_json(input: &[u8]) -> Result<(), &'static str> {
    let mut stack: Vec<(u8, usize, bool)> = Vec::new();
    let mut nodes = 0usize;
    let mut i = 0usize;
    while i < input.len() {
        match input[i] {
            b' ' | b'\n' | b'\r' | b'\t' | b':' => i += 1,
            b',' => {
                if let Some((_, commas, has_item)) = stack.last_mut() {
                    if *has_item {
                        *commas = commas.saturating_add(1);
                        *has_item = false;
                    }
                }
                i += 1;
            }
            b'{' | b'[' => {
                nodes = nodes.saturating_add(1);
                if nodes > limits::MAX_JSON_NODES {
                    return Err("json node limit");
                }
                if let Some((_, _, has_item)) = stack.last_mut() {
                    *has_item = true;
                }
                stack.push((input[i], 0, false));
                if stack.len() > limits::MAX_JSON_DEPTH {
                    return Err("json depth limit");
                }
                i += 1;
            }
            b'}' | b']' => {
                let expected = if input[i] == b'}' { b'{' } else { b'[' };
                let Some((kind, commas, has_item)) = stack.pop() else {
                    return Err("json nesting");
                };
                if kind != expected {
                    return Err("json nesting");
                }
                if commas.saturating_add(usize::from(has_item)) > limits::MAX_CONTAINER_ITEMS {
                    return Err("json container limit");
                }
                i += 1;
            }
            b'"' => {
                if let Some((_, _, has_item)) = stack.last_mut() {
                    *has_item = true;
                }
                nodes = nodes.saturating_add(1);
                if nodes > limits::MAX_JSON_NODES {
                    return Err("json node limit");
                }
                i += 1;
                let mut decoded = 0usize;
                let mut closed = false;
                while i < input.len() {
                    match input[i] {
                        b'"' => {
                            i += 1;
                            closed = true;
                            break;
                        }
                        b'\\' => {
                            i += 1;
                            let Some(escape) = input.get(i).copied() else {
                                return Err("json string");
                            };
                            if escape == b'u' {
                                if i + 4 >= input.len()
                                    || !input[i + 1..i + 5].iter().all(|b| b.is_ascii_hexdigit())
                                {
                                    return Err("json escape");
                                }
                                decoded = decoded.saturating_add(3);
                                i += 5;
                            } else if matches!(
                                escape,
                                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                            ) {
                                decoded = decoded.saturating_add(1);
                                i += 1;
                            } else {
                                return Err("json escape");
                            }
                        }
                        b if b < 0x20 => return Err("json control"),
                        _ => {
                            decoded = decoded.saturating_add(1);
                            i += 1;
                        }
                    }
                    if decoded > limits::MAX_SCALAR_STRING_BYTES {
                        return Err("json string limit");
                    }
                }
                if !closed {
                    return Err("json string");
                }
            }
            _ => {
                if let Some((_, _, has_item)) = stack.last_mut() {
                    *has_item = true;
                }
                nodes = nodes.saturating_add(1);
                if nodes > limits::MAX_JSON_NODES {
                    return Err("json node limit");
                }
                while i < input.len()
                    && !matches!(input[i], b' ' | b'\n' | b'\r' | b'\t' | b',' | b']' | b'}')
                {
                    i += 1;
                }
            }
        }
    }
    if stack.is_empty() {
        Ok(())
    } else {
        Err("json nesting")
    }
}

/// Encode an absolute filesystem path as a `file://` URI.
pub fn path_to_uri(path: &str) -> String {
    if path.len() > limits::MAX_IDENTITY_BYTES {
        return String::new();
    }
    format!("file://{}", percent_encode_path(path))
}

/// Decode a `file://` URI back to a filesystem path (best-effort).
pub fn uri_to_path(uri: &str) -> String {
    let body = uri.strip_prefix("file://").unwrap_or(uri);
    percent_decode(body)
}

fn bounded_uri_to_path(uri: &str) -> Option<String> {
    if uri.len() > limits::MAX_IDENTITY_BYTES {
        return None;
    }
    let body = uri.strip_prefix("file://")?;
    let bytes = body.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return None;
            }
        }
    }
    let path = percent_decode(body);
    (path.len() <= limits::MAX_IDENTITY_BYTES
        && !path.is_empty()
        && !path.chars().any(char::is_control))
    .then_some(path)
}

fn ensure_identity_uri(uri: &str) -> Result<(), LspError> {
    bounded_uri_to_path(uri)
        .map(|_| ())
        .ok_or_else(|| LspError::Bounded("LSP identity URI limit".into()))
}

/// Percent-encode everything outside the unreserved set, keeping `/` literal.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Reverse [`percent_encode_path`] (also decodes `%`-escapes other encoders emit).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ─── response mapping (pure, defensive) ──────────────────────────────────────

fn position_from_json(v: &Value) -> Position {
    Position {
        line: v.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
        character: v.get("character").and_then(Value::as_u64).unwrap_or(0) as u32,
    }
}

fn range_from_json(v: &Value) -> Range {
    Range {
        start: v.get("start").map(position_from_json).unwrap_or_default(),
        end: v.get("end").map(position_from_json).unwrap_or_default(),
    }
}

fn location_from_json(v: &Value) -> Option<Location> {
    // Plain `Location { uri, range }`.
    if let (Some(uri), Some(range)) = (v.get("uri"), v.get("range")) {
        return Some(Location {
            path: bounded_uri_to_path(uri.as_str()?)?,
            range: range_from_json(range),
        });
    }
    // `LocationLink { targetUri, targetSelectionRange | targetRange }`.
    if let Some(uri) = v.get("targetUri").and_then(Value::as_str) {
        let range = v
            .get("targetSelectionRange")
            .or_else(|| v.get("targetRange"))
            .map(range_from_json)
            .unwrap_or_default();
        return Some(Location {
            path: bounded_uri_to_path(uri)?,
            range,
        });
    }
    None
}

/// Parse a definition/references result: `Location | Location[] | LocationLink[]`.
pub fn parse_locations(result: &Value) -> Vec<Location> {
    let mut health = ProjectionHealth::default();
    parse_locations_limited(result, &mut health)
}

fn parse_locations_limited(result: &Value, health: &mut ProjectionHealth) -> Vec<Location> {
    if result
        .as_array()
        .is_some_and(|items| items.len() > limits::MAX_RESULT_ITEMS)
    {
        health.truncated = true;
    }
    match result {
        Value::Array(items) => {
            let mut bytes = 0usize;
            items
                .iter()
                .take(limits::MAX_RESULT_ITEMS)
                .filter_map(|item| {
                    let location = location_from_json(item)?;
                    let item_bytes = location
                        .path
                        .len()
                        .saturating_add(std::mem::size_of::<Location>());
                    if bytes.saturating_add(item_bytes) > limits::MAX_PROJECTED_RESPONSE_BYTES {
                        health.truncated = true;
                        return None;
                    }
                    bytes = bytes.saturating_add(item_bytes);
                    Some(location)
                })
                .collect()
        }
        Value::Object(_) => location_from_json(result).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Parse a `documentSymbol`/`workspace/symbol` result, handling both the
/// hierarchical `DocumentSymbol[]` and the flat `SymbolInformation[]` shapes.
pub fn parse_symbols(result: &Value, fallback_path: &str) -> Vec<SymbolInfo> {
    let mut health = ProjectionHealth::default();
    project_symbols(result, fallback_path, &mut health)
}

fn project_symbols(
    result: &Value,
    fallback_path: &str,
    health: &mut ProjectionHealth,
) -> Vec<SymbolInfo> {
    let Some(items) = result.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if items.len() > limits::MAX_CONTAINER_ITEMS {
        health.truncated = true;
    }
    let mut projected_bytes = 0usize;
    let mut stack: Vec<(&Value, Option<String>)> = items
        .iter()
        .take(limits::MAX_CONTAINER_ITEMS)
        .rev()
        .map(|item| (item, None))
        .collect();
    while let Some((item, container)) = stack.pop() {
        if out.len() >= limits::MAX_RESULT_ITEMS {
            health.truncated = true;
            break;
        }
        let Some(raw_name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(name) = bounded_string(raw_name, limits::MAX_SCALAR_STRING_BYTES, health) else {
            continue;
        };
        let location = if let Some(loc) = item.get("location").and_then(location_from_json) {
            loc
        } else {
            let range = item
                .get("selectionRange")
                .or_else(|| item.get("range"))
                .map(range_from_json)
                .unwrap_or_default();
            Location {
                path: fallback_path.to_string(),
                range,
            }
        };
        let container_name = item
            .get("containerName")
            .and_then(Value::as_str)
            .and_then(|s| bounded_string(s, limits::MAX_SCALAR_STRING_BYTES, health))
            .or_else(|| container.clone());
        let item_bytes = name
            .len()
            .saturating_add(container_name.as_ref().map_or(0, String::len))
            .saturating_add(location.path.len())
            .saturating_add(std::mem::size_of::<SymbolInfo>());
        if projected_bytes.saturating_add(item_bytes) > limits::MAX_PROJECTED_RESPONSE_BYTES {
            health.truncated = true;
            break;
        }
        projected_bytes = projected_bytes.saturating_add(item_bytes);
        out.push(SymbolInfo {
            name: name.clone(),
            kind: SymbolKind::from_lsp(item.get("kind").and_then(Value::as_i64).unwrap_or(0)),
            location,
            container: container_name,
        });
        if let Some(children) = item.get("children").and_then(Value::as_array) {
            if children.len() > limits::MAX_CONTAINER_ITEMS {
                health.truncated = true;
            }
            for child in children.iter().take(limits::MAX_CONTAINER_ITEMS).rev() {
                stack.push((child, Some(name.clone())));
            }
        }
    }
    out
}

/// Parse a `textDocument/hover` result into flattened markdown.
pub fn parse_hover(result: &Value) -> Option<HoverInfo> {
    let mut health = ProjectionHealth::default();
    parse_hover_limited(result, &mut health)
}

fn parse_hover_limited(result: &Value, health: &mut ProjectionHealth) -> Option<HoverInfo> {
    if result.is_null() {
        return None;
    }
    let contents = result.get("contents")?;
    let markdown = flatten_hover_contents_limited(contents, health);
    if markdown.trim().is_empty() {
        return None;
    }
    Some(HoverInfo {
        markdown,
        range: result.get("range").map(range_from_json),
    })
}

fn flatten_hover_contents_limited(v: &Value, health: &mut ProjectionHealth) -> String {
    let mut stack = vec![v];
    let mut parts = Vec::new();
    let mut bytes = 0usize;
    while let Some(value) = stack.pop() {
        let part = match value {
            Value::Object(o) if o.contains_key("language") => {
                let lang = o.get("language").and_then(Value::as_str).unwrap_or("");
                let code = o.get("value").and_then(Value::as_str).unwrap_or("");
                let Some(lang) = bounded_string(lang, limits::MAX_CODE_SOURCE_BYTES, health) else {
                    continue;
                };
                let Some(code) = bounded_string(code, limits::MAX_HOVER_MARKDOWN_BYTES, health)
                else {
                    continue;
                };
                Some(format!("```{lang}\n{code}\n```"))
            }
            Value::Object(o) if o.contains_key("value") => o
                .get("value")
                .and_then(Value::as_str)
                .and_then(|s| bounded_string(s, limits::MAX_HOVER_MARKDOWN_BYTES, health)),
            Value::String(s) => bounded_string(s, limits::MAX_HOVER_MARKDOWN_BYTES, health),
            Value::Array(items) => {
                if items.len() > limits::MAX_CONTAINER_ITEMS {
                    health.truncated = true;
                }
                for item in items.iter().take(limits::MAX_CONTAINER_ITEMS).rev() {
                    stack.push(item);
                }
                None
            }
            _ => None,
        };
        if let Some(part) = part {
            let separator = usize::from(!parts.is_empty()) * 2;
            if bytes.saturating_add(separator).saturating_add(part.len())
                > limits::MAX_HOVER_MARKDOWN_BYTES
                || bytes.saturating_add(separator).saturating_add(part.len())
                    > limits::MAX_PROJECTED_RESPONSE_BYTES
            {
                health.truncated = true;
                break;
            }
            bytes = bytes.saturating_add(separator).saturating_add(part.len());
            parts.push(part);
        }
    }
    parts.join("\n\n")
}

/// Parse a `textDocument/signatureHelp` result.
pub fn parse_signatures(result: &Value) -> Vec<SignatureInfo> {
    let mut health = ProjectionHealth::default();
    parse_signatures_limited(result, &mut health)
}

fn parse_signatures_limited(result: &Value, health: &mut ProjectionHealth) -> Vec<SignatureInfo> {
    let Some(sigs) = result.get("signatures").and_then(Value::as_array) else {
        return Vec::new();
    };
    if sigs.len() > limits::MAX_CONTAINER_ITEMS {
        health.truncated = true;
    }
    let mut projected_bytes = 0usize;
    sigs.iter()
        .take(limits::MAX_RESULT_ITEMS)
        .filter_map(|s| {
            let label = bounded_string(
                s.get("label").and_then(Value::as_str)?,
                limits::MAX_SCALAR_STRING_BYTES,
                health,
            )?;
            let doc = s
                .get("documentation")
                .map(|value| flatten_hover_contents_limited(value, health))
                .filter(|d| !d.trim().is_empty());
            let item_bytes = label
                .len()
                .saturating_add(doc.as_ref().map_or(0, String::len));
            if projected_bytes.saturating_add(item_bytes) > limits::MAX_PROJECTED_RESPONSE_BYTES {
                health.truncated = true;
                return None;
            }
            projected_bytes = projected_bytes.saturating_add(item_bytes);
            Some(SignatureInfo { label, doc })
        })
        .collect()
}

/// Parse a `textDocument/codeAction` result (`(Command | CodeAction)[]`).
pub fn parse_code_actions(result: &Value) -> Vec<CodeActionInfo> {
    let mut health = ProjectionHealth::default();
    parse_code_actions_limited(result, &mut health)
}

fn parse_code_actions_limited(
    result: &Value,
    health: &mut ProjectionHealth,
) -> Vec<CodeActionInfo> {
    let Some(items) = result.as_array() else {
        return Vec::new();
    };
    if items.len() > limits::MAX_CONTAINER_ITEMS {
        health.truncated = true;
    }
    let mut projected_bytes = 0usize;
    items
        .iter()
        .take(limits::MAX_RESULT_ITEMS)
        .filter_map(|v| {
            let title = bounded_string(
                v.get("title").and_then(Value::as_str)?,
                limits::MAX_SCALAR_STRING_BYTES,
                health,
            )?;
            let kind = v
                .get("kind")
                .and_then(Value::as_str)
                .and_then(|s| bounded_string(s, limits::MAX_CODE_SOURCE_BYTES, health));
            let item_bytes = title
                .len()
                .saturating_add(kind.as_ref().map_or(0, String::len));
            if projected_bytes.saturating_add(item_bytes) > limits::MAX_PROJECTED_RESPONSE_BYTES {
                health.truncated = true;
                return None;
            }
            projected_bytes = projected_bytes.saturating_add(item_bytes);
            Some(CodeActionInfo { title, kind })
        })
        .collect()
}

/// Parse a `publishDiagnostics` notification's params, stamping the owning
/// client's worktree `root` so downstream stores can partition by it.
pub fn parse_published_diagnostics(params: &Value, root: &Path) -> Option<PublishedDiagnostics> {
    parse_published_diagnostics_with_context(params, root, String::new(), 0, 0).0
}

fn parse_published_diagnostics_with_context(
    params: &Value,
    root: &Path,
    server_identity: String,
    generation: u64,
    sequence: u64,
) -> (Option<PublishedDiagnostics>, LspHealth) {
    let mut health = ProjectionHealth::default();
    let Some(path) = params
        .get("uri")
        .and_then(Value::as_str)
        .and_then(bounded_uri_to_path)
    else {
        health.invalid = true;
        return (
            None,
            LspHealth {
                invalid: 1,
                ..LspHealth::default()
            },
        );
    };
    let Some(raw_diagnostics) = params.get("diagnostics").and_then(Value::as_array) else {
        health.invalid = true;
        return (
            Some(PublishedDiagnostics {
                root: root.to_path_buf(),
                path,
                diagnostics: Vec::new(),
                server_identity,
                generation,
                sequence,
                complete: false,
            }),
            LspHealth {
                invalid: 1,
                ..LspHealth::default()
            },
        );
    };
    if raw_diagnostics.len() > limits::MAX_DIAGNOSTICS {
        health.truncated = true;
    }
    let mut bytes = path
        .len()
        .saturating_add(std::mem::size_of::<PublishedDiagnostics>());
    let mut diagnostics = Vec::new();
    for value in raw_diagnostics.iter().take(limits::MAX_DIAGNOSTICS) {
        let estimate = diagnostic_value_bytes(value);
        if bytes.saturating_add(estimate) > limits::MAX_DIAGNOSTIC_BYTES {
            health.truncated = true;
            break;
        }
        let Some(diagnostic) = parse_one_diagnostic(value, &mut health) else {
            health.invalid = true;
            continue;
        };
        let actual = diagnostic
            .message
            .len()
            .saturating_add(diagnostic.code.as_ref().map_or(0, String::len))
            .saturating_add(diagnostic.source.as_ref().map_or(0, String::len))
            .saturating_add(std::mem::size_of::<LspDiagnostic>());
        if bytes.saturating_add(actual) > limits::MAX_DIAGNOSTIC_BYTES {
            health.truncated = true;
            break;
        }
        bytes = bytes.saturating_add(actual);
        diagnostics.push(diagnostic);
    }
    (
        Some(PublishedDiagnostics {
            root: root.to_path_buf(),
            path,
            diagnostics,
            server_identity,
            generation,
            sequence,
            complete: !health.truncated && !health.invalid,
        }),
        LspHealth {
            truncated: u64::from(health.truncated),
            invalid: u64::from(health.invalid),
            ..LspHealth::default()
        },
    )
}

fn diagnostic_value_bytes(value: &Value) -> usize {
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .map_or(0, str::len);
    let source = value
        .get("source")
        .and_then(Value::as_str)
        .map_or(0, str::len);
    let code = value.get("code").map_or(0, |code| match code {
        Value::String(s) => s.len(),
        Value::Number(n) => n.to_string().len(),
        _ => 0,
    });
    message
        .saturating_add(source)
        .saturating_add(code)
        .saturating_add(std::mem::size_of::<LspDiagnostic>())
}

fn parse_one_diagnostic(v: &Value, health: &mut ProjectionHealth) -> Option<LspDiagnostic> {
    let start = v.get("range")?.get("start").map(position_from_json)?;
    let severity = LspSeverity::from_lsp(v.get("severity").and_then(Value::as_i64).unwrap_or(1));
    let code = v.get("code").and_then(|c| match c {
        Value::String(s) => bounded_string(s, limits::MAX_CODE_SOURCE_BYTES, health),
        Value::Number(n) => bounded_string(&n.to_string(), limits::MAX_CODE_SOURCE_BYTES, health),
        _ => None,
    });
    Some(LspDiagnostic {
        line: start.line,
        character: start.character,
        severity,
        message: bounded_string(
            v.get("message").and_then(Value::as_str).unwrap_or(""),
            limits::MAX_SCALAR_STRING_BYTES,
            health,
        )?,
        code,
        source: v
            .get("source")
            .and_then(Value::as_str)
            .and_then(|s| bounded_string(s, limits::MAX_CODE_SOURCE_BYTES, health)),
    })
}

fn bounded_error_text(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("remote LSP error");
    let mut health = ProjectionHealth::default();
    bounded_string(message, limits::MAX_CODE_SOURCE_BYTES, &mut health)
        .unwrap_or_else(|| "remote LSP error (truncated)".to_string())
}

// ─── the client ──────────────────────────────────────────────────────────────

type Pending = Arc<Mutex<HashMap<i64, Sender<Result<Value, LspError>>>>>;

/// Shared write half of the LSP transport — a local child's stdin, or a
/// remote/bridge stream's writer.
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// A live connection to one language server. The transport is abstracted over
/// `Read`/`Write` so the server may run locally (a child process) OR inside a
/// sandbox/remote, driven over the resident bridge's stdio (see `from_io`).
pub struct LspClient {
    stdin: SharedWriter,
    /// The local child, if any. `None` for a bridge/remote server (its lifecycle
    /// is owned by the bridge/exec channel, which closes when this drops).
    child: Mutex<Option<Child>>,
    /// Accounts this server's CPU/RAM to thegn while the client lives (see
    /// `thegn_core::proc_registry`). Language servers are the largest slice of
    /// thegn's real footprint — three `gopls` alone measured 700 MB on one
    /// session — and none of it appeared in any metric before this.
    ///
    /// `None` for a bridged/remote server: that process runs on another machine
    /// (or inside a sandbox with its own accounting), so charging its memory to
    /// this host would be wrong. Dropped with the client, which is what keeps a
    /// stopped server from lingering in the totals.
    _proc: Option<thegn_core::proc_registry::ProcHandle>,
    next_id: AtomicI64,
    pending: Pending,
    closed: Arc<AtomicBool>,
    root: PathBuf,
    diagnostics: Arc<dyn DiagnosticSink>,
    /// The `languageId` sent in `didOpen` — carried from the resolved spec, so
    /// this connection speaks the wire protocol without any tree-sitter `Lang`.
    language_id: String,
    /// The server's negotiated capabilities, captured on `initialize`. Empty
    /// (all-`false`) until then, so any request issued before the handshake is
    /// gated off rather than sent blind.
    caps: OnceLock<ServerCapabilities>,
    timeout: Duration,
    _reader: JoinHandle<()>,
}

impl LspClient {
    /// Spawn and connect to the server described by `spec`, rooted at `root`.
    /// `diag_tx` receives every `publishDiagnostics` notification. The argv is
    /// used as-is (no resource wrap) — the host wraps it via
    /// [`LspClient::start_argv`] before it reaches a live worktree.
    pub fn start<S: DiagnosticSink + 'static>(
        spec: &ServerSpec,
        root: &Path,
        diag_tx: S,
    ) -> Result<LspClient, LspError> {
        Self::start_argv_with_context(
            &spec.argv(),
            &spec.language_id,
            root,
            Arc::new(diag_tx),
            spec.key.clone(),
            0,
        )
    }

    /// Spawn and connect to a local server from a fully-formed `argv`
    /// (`[command, args…]`), tagging the connection with `language_id`. The host
    /// passes an argv already wrapped by the shared background resource wrap
    /// (`sandbox_cpucap::wrap_background_argv`) so the server joins `thegn.slice`.
    pub fn start_argv<S: DiagnosticSink + 'static>(
        argv: &[String],
        language_id: &str,
        root: &Path,
        diag_tx: S,
    ) -> Result<LspClient, LspError> {
        Self::start_argv_with_context(argv, language_id, root, Arc::new(diag_tx), String::new(), 0)
    }

    /// Start a client under supervisor-owned authority.  The generation is
    /// minted by the supervisor and is never trusted from a publication.
    pub fn start_argv_with_identity(
        argv: &[String],
        language_id: &str,
        root: &Path,
        diagnostics: DiagnosticsSender,
        server_identity: String,
        generation: u64,
    ) -> Result<LspClient, LspError> {
        Self::start_argv_with_context(
            argv,
            language_id,
            root,
            Arc::new(diagnostics),
            server_identity,
            generation,
        )
    }

    fn start_argv_with_context(
        argv: &[String],
        language_id: &str,
        root: &Path,
        diag_tx: Arc<dyn DiagnosticSink>,
        server_identity: String,
        generation: u64,
    ) -> Result<LspClient, LspError> {
        let (cmd, rest) = argv
            .split_first()
            .ok_or_else(|| LspError::Spawn("empty server argv".into()))?;
        let mut child = Command::new(cmd)
            .args(rest)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| LspError::Spawn(e.to_string()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Spawn("no stdout".into()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Spawn("no stdin".into()))?;
        // Read the PID before the child moves into the client.
        let pid = child.id();
        let mut client = Self::connect(
            Box::new(stdout),
            Box::new(stdin),
            Some(child),
            root,
            language_id.to_string(),
            diag_tx,
            server_identity,
            generation,
        );
        // The command's file name, not the full path: `/nix/store/…/bin/gopls`
        // is a store hash in a status list, and the args can carry a project
        // path the user may not want on screen.
        let name = std::path::Path::new(cmd)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| cmd.clone());
        client._proc = Some(thegn_core::proc_registry::register(
            thegn_core::proc_registry::GROUP_LSP,
            name,
            pid,
        ));
        Ok(client)
    }

    /// Connect to a language server over ARBITRARY stdio rather than spawning a
    /// local child — the seam for an **in-sandbox/remote** server whose stdin/
    /// stdout are bridged to the host (e.g. via the resident bridge or a provider
    /// exec). Same JSON-RPC behavior; no child to reap (the stream owns
    /// lifecycle), and no host resource wrap (the sandbox bounds it).
    pub fn from_io<S: DiagnosticSink + 'static>(
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        language_id: &str,
        root: &Path,
        diag_tx: S,
    ) -> LspClient {
        Self::connect(
            reader,
            writer,
            None,
            root,
            language_id.to_string(),
            Arc::new(diag_tx),
            String::new(),
            0,
        )
    }

    fn connect(
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        child: Option<Child>,
        root: &Path,
        language_id: String,
        diag_tx: Arc<dyn DiagnosticSink>,
        server_identity: String,
        generation: u64,
    ) -> LspClient {
        let stdin: SharedWriter = Arc::new(Mutex::new(writer));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let reader_thread = {
            let pending = pending.clone();
            let stdin = stdin.clone();
            let root = root.to_path_buf();
            let closed = closed.clone();
            let sequence = Arc::new(AtomicU64::new(0));
            let reader_sequence = sequence.clone();
            let reader_identity = server_identity.clone();
            let reader_diag = diag_tx.clone();
            thread::spawn(move || {
                reader_loop(
                    reader,
                    pending,
                    reader_diag,
                    stdin,
                    root,
                    closed,
                    reader_identity,
                    generation,
                    reader_sequence,
                )
            })
        };

        LspClient {
            stdin,
            child: Mutex::new(child),
            next_id: AtomicI64::new(1),
            pending,
            closed,
            root: root.to_path_buf(),
            language_id,
            diagnostics: diag_tx,
            caps: OnceLock::new(),
            timeout: Duration::from_secs(10),
            _reader: reader_thread,
            _proc: None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The server's negotiated capabilities (all-`false` until `initialize`).
    pub fn capabilities(&self) -> ServerCapabilities {
        self.caps.get().copied().unwrap_or_default()
    }

    /// Whether the server declared support for `method` — the pure gate every
    /// request method checks before touching the wire.
    fn supports(&self, method: LspMethod) -> bool {
        self.capabilities().supports(method)
    }

    /// Run the `initialize`/`initialized` handshake, capturing the server's
    /// declared `capabilities` for the request gate.
    pub fn initialize(&self, root: &Path) -> Result<(), LspError> {
        let uri = path_to_uri(&root.to_string_lossy());
        if uri.is_empty() {
            return Err(LspError::Bounded("workspace identity path limit".into()));
        }
        let params = json!({
            "processId": std::process::id(),
            "rootUri": uri,
            "capabilities": {
                "textDocument": {
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "publishDiagnostics": {},
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                },
                "workspace": {},
            },
            "workspaceFolders": [{ "uri": uri, "name": "root" }],
        });
        let result = self.request("initialize", params)?;
        // Set-once: the handshake happens exactly once per client, off the loop.
        let _ = self // best-effort: set-once (see above); second set is a no-op
            .caps
            .set(ServerCapabilities::from_initialize_result(&result));
        self.notify("initialized", json!({}))
    }

    /// Tell the server a document is open (text is the on-disk content). Uses
    /// the connection's `languageId` (from the resolved registry entry).
    pub fn did_open(&self, uri: &str, text: &str) -> Result<(), LspError> {
        ensure_identity_uri(uri)?;
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri,
                "languageId": self.language_id,
                "version": 1,
                "text": text,
            }}),
        )
    }

    /// Document symbols (the outline) for an opened document.
    pub fn document_symbols(&self, uri: &str) -> Result<Vec<SymbolInfo>, LspError> {
        if !self.supports(LspMethod::DocumentSymbol) {
            return Err(LspError::NotAvailable);
        }
        ensure_identity_uri(uri)?;
        let fallback = bounded_uri_to_path(uri)
            .ok_or_else(|| LspError::Bounded("document identity path limit".into()))?;
        let res = self.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        )?;
        let mut health = ProjectionHealth::default();
        let symbols = project_symbols(&res, &fallback, &mut health);
        health.record(self.diagnostics.as_ref());
        Ok(symbols)
    }

    /// Workspace-wide symbol search.
    pub fn workspace_symbols(&self, query: &str) -> Result<Vec<SymbolInfo>, LspError> {
        if !self.supports(LspMethod::WorkspaceSymbol) {
            return Err(LspError::NotAvailable);
        }
        let res = self.request("workspace/symbol", json!({ "query": query }))?;
        let mut health = ProjectionHealth::default();
        let symbols = project_symbols(&res, "", &mut health);
        health.record(self.diagnostics.as_ref());
        Ok(symbols)
    }

    /// Definition location(s) for a position.
    pub fn definition(&self, uri: &str, pos: Position) -> Result<Vec<Location>, LspError> {
        if !self.supports(LspMethod::Definition) {
            return Err(LspError::NotAvailable);
        }
        ensure_identity_uri(uri)?;
        let res = self.request("textDocument/definition", self.pos_params(uri, pos))?;
        let mut health = ProjectionHealth::default();
        let locations = parse_locations_limited(&res, &mut health);
        health.record(self.diagnostics.as_ref());
        Ok(locations)
    }

    /// Reference location(s) for a position.
    pub fn references(&self, uri: &str, pos: Position) -> Result<Vec<Location>, LspError> {
        if !self.supports(LspMethod::References) {
            return Err(LspError::NotAvailable);
        }
        ensure_identity_uri(uri)?;
        let res = self.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": pos.line, "character": pos.character },
                "context": { "includeDeclaration": false },
            }),
        )?;
        let mut health = ProjectionHealth::default();
        let locations = parse_locations_limited(&res, &mut health);
        health.record(self.diagnostics.as_ref());
        Ok(locations)
    }

    /// Hover content for a position.
    pub fn hover(&self, uri: &str, pos: Position) -> Result<Option<HoverInfo>, LspError> {
        if !self.supports(LspMethod::Hover) {
            return Err(LspError::NotAvailable);
        }
        ensure_identity_uri(uri)?;
        let res = self.request("textDocument/hover", self.pos_params(uri, pos))?;
        let mut health = ProjectionHealth::default();
        let hover = parse_hover_limited(&res, &mut health);
        health.record(self.diagnostics.as_ref());
        Ok(hover)
    }

    /// Signature help for a position.
    pub fn signature_help(&self, uri: &str, pos: Position) -> Result<Vec<SignatureInfo>, LspError> {
        if !self.supports(LspMethod::SignatureHelp) {
            return Err(LspError::NotAvailable);
        }
        ensure_identity_uri(uri)?;
        let res = self.request("textDocument/signatureHelp", self.pos_params(uri, pos))?;
        let mut health = ProjectionHealth::default();
        let signatures = parse_signatures_limited(&res, &mut health);
        health.record(self.diagnostics.as_ref());
        Ok(signatures)
    }

    /// Code actions offered for a range.
    pub fn code_actions(&self, uri: &str, range: Range) -> Result<Vec<CodeActionInfo>, LspError> {
        if !self.supports(LspMethod::CodeAction) {
            return Err(LspError::NotAvailable);
        }
        ensure_identity_uri(uri)?;
        let res = self.request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": range.start.line, "character": range.start.character },
                    "end": { "line": range.end.line, "character": range.end.character },
                },
                "context": { "diagnostics": [] },
            }),
        )?;
        let mut health = ProjectionHealth::default();
        let actions = parse_code_actions_limited(&res, &mut health);
        health.record(self.diagnostics.as_ref());
        Ok(actions)
    }

    fn pos_params(&self, uri: &str, pos: Position) -> Value {
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": pos.line, "character": pos.character },
        })
    }

    /// Send a request and block (up to `timeout`) for its correlated response.
    fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        self.ensure_open()?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            // Reader closure uses this same lock to latch closed and drain.
            // A late requester cannot insert after the terminal drain.
            self.ensure_open()?;
            pending.insert(id, tx);
        }

        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.write(&body.to_string()) {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }

        match rx.recv_timeout(self.timeout) {
            Ok(res) => res,
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(LspError::Timeout)
            }
        }
    }

    fn ensure_open(&self) -> Result<(), LspError> {
        if self.closed.load(Ordering::SeqCst) {
            Err(LspError::Protocol("server stream closed".into()))
        } else {
            Ok(())
        }
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.ensure_open()?;
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write(&body.to_string())
    }

    fn write(&self, body: &str) -> Result<(), LspError> {
        self.ensure_open()?;
        if body.len() > limits::MAX_BODY_BYTES {
            return Err(LspError::Bounded("outbound LSP body limit".into()));
        }
        preflight_json(body.as_bytes())
            .map_err(|reason| LspError::Bounded(format!("outbound LSP JSON: {reason}")))?;
        let framed = framing::encode(body);
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|_| LspError::Protocol("stdin poisoned".into()))?;
        self.ensure_open()?;
        stdin
            .write_all(&framed)
            .and_then(|_| stdin.flush())
            .map_err(|e| LspError::Protocol(e.to_string()))
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Best-effort graceful exit, then make sure a local child is gone (a
        // bridge/remote server has no local child to reap).
        let _ = self.notify("exit", Value::Null); // best-effort: graceful exit; server may already be gone
        if let Ok(mut child) = self.child.lock()
            && let Some(c) = child.as_mut()
        {
            let _ = c.kill(); // best-effort: child may already have exited
            let _ = c.wait(); // best-effort: reap-or-not is terminal here
        }
    }
}

/// Read framed messages off the server's stdout until EOF, dispatching each.
/// Transport-agnostic: `reader` is a local child's stdout or a bridged stream.
fn reader_loop(
    reader: Box<dyn Read + Send>,
    pending: Pending,
    diag_tx: Arc<dyn DiagnosticSink>,
    stdin: SharedWriter,
    root: PathBuf,
    closed: Arc<AtomicBool>,
    server_identity: String,
    generation: u64,
    sequence: Arc<AtomicU64>,
) {
    let mut reader = framing::FramedReader::with_limit(reader, limits::MAX_BODY_BYTES);
    loop {
        match reader.read_message() {
            Ok(Some(body)) => {
                if let Err(reason) = preflight_json(body.as_bytes()) {
                    diag_tx.record(LspHealth {
                        invalid: 1,
                        ..LspHealth::default()
                    });
                    tracing::warn!(target: "thegn::lsp", reason, "dropping over-budget LSP JSON message");
                    continue;
                }
                if let Ok(msg) = serde_json::from_str::<Value>(&body) {
                    dispatch(
                        &msg,
                        &pending,
                        &diag_tx,
                        &stdin,
                        &root,
                        &server_identity,
                        generation,
                        &sequence,
                    );
                } else {
                    diag_tx.record(LspHealth {
                        invalid: 1,
                        ..LspHealth::default()
                    });
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(target: "thegn::lsp", %error, "closing invalid server stream");
                break;
            }
        }
    }
    // Stream closed — unblock any waiters so they don't hang to the deadline.
    let mut map = pending.lock().unwrap();
    closed.store(true, Ordering::SeqCst);
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(LspError::Protocol("server stream closed".into()))); // best-effort: pending requesters may be gone
    }
}

fn dispatch(
    msg: &Value,
    pending: &Pending,
    diag_tx: &Arc<dyn DiagnosticSink>,
    stdin: &SharedWriter,
    root: &Path,
    server_identity: &str,
    generation: u64,
    sequence: &AtomicU64,
) {
    let id = msg.get("id").and_then(Value::as_i64);
    let method = msg.get("method").and_then(Value::as_str);

    match (id, method) {
        // Response to one of our requests.
        (Some(id), None) => {
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let payload = if let Some(err) = msg.get("error") {
                    if err
                        .get("message")
                        .and_then(Value::as_str)
                        .is_some_and(|message| message.len() > limits::MAX_CODE_SOURCE_BYTES)
                    {
                        diag_tx.record(LspHealth {
                            truncated: 1,
                            ..LspHealth::default()
                        });
                    }
                    Err(LspError::Protocol(bounded_error_text(err)))
                } else {
                    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = tx.send(payload); // best-effort: requester may have timed out
            }
        }
        // Server→client request: reply so the server doesn't stall.
        (Some(id), Some(method)) => {
            let result = match method {
                // `workspace/configuration` expects one entry per requested item.
                "workspace/configuration" => {
                    let n = msg
                        .get("params")
                        .and_then(|p| p.get("items"))
                        .and_then(Value::as_array)
                        .map(|a| a.len())
                        .unwrap_or(0);
                    Value::Array(vec![Value::Null; n])
                }
                _ => Value::Null,
            };
            let body = json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
            if let Ok(mut s) = stdin.lock() {
                let _ = s.write_all(&framing::encode(&body)); // best-effort: test fake server stdin; server may be gone
                let _ = s.flush(); // best-effort: same
            }
        }
        // Notification.
        (None, Some("textDocument/publishDiagnostics")) => {
            if let Some(params) = msg.get("params") {
                let seq = sequence.fetch_add(1, Ordering::Relaxed).saturating_add(1);
                let (pd, health) = parse_published_diagnostics_with_context(
                    params,
                    root,
                    server_identity.to_string(),
                    generation,
                    seq,
                );
                diag_tx.record(health);
                if let Some(pd) = pd {
                    diag_tx.publish(pd);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_framing_rejects_late_concurrent_requests_without_writing() {
        struct RejectWrites;
        impl Write for RejectWrites {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                panic!("closed transport wrote bytes")
            }
            fn flush(&mut self) -> std::io::Result<()> {
                panic!("closed transport flushed")
            }
        }
        let (diag_tx, diag_rx) = mpsc::channel();
        let client = Arc::new(LspClient::from_io(
            Box::new(std::io::Cursor::new(b"bad\r\n\r\n")),
            Box::new(RejectWrites),
            "fixture",
            Path::new("/fixture"),
            diag_tx,
        ));
        assert!(matches!(
            diag_rx.recv_timeout(Duration::from_secs(2)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let client = client.clone();
                scope.spawn(move || {
                    assert!(matches!(client.request("late", Value::Null), Err(LspError::Protocol(ref e)) if e == "server stream closed"));
                    assert!(client.notify("late", Value::Null).is_err());
                });
            }
        });
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn invalid_framing_closes_lsp_and_fails_pending_without_dispatch() {
        let (request_tx, request_rx) = mpsc::channel();
        let pending: Pending = Arc::new(Mutex::new(HashMap::from([(7, request_tx)])));
        let (diag_tx, diag_rx) = mpsc::channel();
        let stdin: SharedWriter = Arc::new(Mutex::new(Box::new(std::io::sink())));
        let mut wire = b"Content-Length: 1\r\nContent-Length: 1\r\n\r\nX".to_vec();
        wire.extend(framing::encode(r#"{"id":7,"result":"must not dispatch"}"#));
        reader_loop(
            Box::new(std::io::Cursor::new(wire)),
            pending.clone(),
            Arc::new(diag_tx),
            stdin,
            PathBuf::from("/fixture"),
            Arc::new(AtomicBool::new(false)),
            "fixture".into(),
            1,
            Arc::new(AtomicU64::new(0)),
        );
        assert!(matches!(
            request_rx.try_recv(),
            Ok(Err(LspError::Protocol(_)))
        ));
        assert!(pending.lock().unwrap().is_empty());
        assert!(matches!(
            diag_rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn uri_round_trips_plain_path() {
        let p = "/home/u/proj/src/lib.rs";
        assert_eq!(path_to_uri(p), "file:///home/u/proj/src/lib.rs");
        assert_eq!(uri_to_path(&path_to_uri(p)), p);
    }

    #[test]
    fn uri_round_trips_path_with_spaces() {
        let p = "/home/u/my proj/a b.rs";
        let uri = path_to_uri(p);
        assert!(uri.contains("%20"), "space should be encoded: {uri}");
        assert_eq!(uri_to_path(&uri), p);
    }

    #[test]
    fn symbol_kind_maps_known_and_unknown() {
        assert_eq!(SymbolKind::from_lsp(12), SymbolKind::Function);
        assert_eq!(SymbolKind::from_lsp(23), SymbolKind::Struct);
        assert_eq!(SymbolKind::from_lsp(999), SymbolKind::Other);
        assert_eq!(SymbolKind::Function.label(), "fn");
    }

    #[test]
    fn severity_maps_lsp_scale() {
        assert_eq!(LspSeverity::from_lsp(1), LspSeverity::Error);
        assert_eq!(LspSeverity::from_lsp(2), LspSeverity::Warning);
        assert_eq!(LspSeverity::from_lsp(4), LspSeverity::Hint);
        assert_eq!(LspSeverity::from_lsp(99), LspSeverity::Hint);
    }

    #[test]
    fn parse_plain_location() {
        let v = json!({ "uri": "file:///x/y.rs", "range": {
            "start": { "line": 4, "character": 2 }, "end": { "line": 4, "character": 9 } } });
        let locs = parse_locations(&v);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].path, "/x/y.rs");
        assert_eq!(locs[0].line_1based(), 5);
    }

    #[test]
    fn parse_location_array_and_link() {
        let v = json!([
            { "uri": "file:///a.rs", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } },
            { "targetUri": "file:///b.rs", "targetSelectionRange": { "start": { "line": 9, "character": 0 }, "end": { "line": 9, "character": 4 } } }
        ]);
        let locs = parse_locations(&v);
        assert_eq!(locs.len(), 2);
        assert_eq!(locs[1].path, "/b.rs");
        assert_eq!(locs[1].line_1based(), 10);
    }

    #[test]
    fn parse_document_symbols_hierarchical() {
        let v = json!([
            { "name": "Foo", "kind": 23,
              "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 10, "character": 0 } },
              "selectionRange": { "start": { "line": 0, "character": 7 }, "end": { "line": 0, "character": 10 } },
              "children": [
                { "name": "bar", "kind": 6,
                  "selectionRange": { "start": { "line": 2, "character": 4 }, "end": { "line": 2, "character": 7 } } }
              ] }
        ]);
        let syms = parse_symbols(&v, "/src/foo.rs");
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "Foo");
        assert_eq!(syms[0].kind, SymbolKind::Struct);
        assert_eq!(syms[0].location.line_1based(), 1);
        assert_eq!(syms[1].name, "bar");
        assert_eq!(syms[1].kind, SymbolKind::Method);
        assert_eq!(syms[1].container.as_deref(), Some("Foo"));
        assert_eq!(syms[1].location.path, "/src/foo.rs");
    }

    #[test]
    fn parse_symbol_information_flat() {
        let v = json!([
            { "name": "do_it", "kind": 12, "location": {
                "uri": "file:///pkg/x.go",
                "range": { "start": { "line": 3, "character": 5 }, "end": { "line": 3, "character": 10 } } },
              "containerName": "pkg" }
        ]);
        let syms = parse_symbols(&v, "");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].location.path, "/pkg/x.go");
        assert_eq!(syms[0].container.as_deref(), Some("pkg"));
    }

    #[test]
    fn parse_hover_markup_and_marked_string() {
        let markup = json!({ "contents": { "kind": "markdown", "value": "**bold** doc" } });
        assert_eq!(parse_hover(&markup).unwrap().markdown, "**bold** doc");

        let marked = json!({ "contents": { "language": "rust", "value": "fn x()" } });
        assert_eq!(
            parse_hover(&marked).unwrap().markdown,
            "```rust\nfn x()\n```"
        );

        let arr = json!({ "contents": ["one", { "kind": "plaintext", "value": "two" }] });
        assert_eq!(parse_hover(&arr).unwrap().markdown, "one\n\ntwo");

        assert!(parse_hover(&Value::Null).is_none());
        assert!(parse_hover(&json!({ "contents": "   " })).is_none());
    }

    #[test]
    fn parse_signatures_and_code_actions() {
        let sig = json!({ "signatures": [
            { "label": "fn add(a: u8, b: u8) -> u8", "documentation": "adds" },
            { "label": "noop" }
        ]});
        let sigs = parse_signatures(&sig);
        assert_eq!(sigs.len(), 2);
        assert_eq!(sigs[0].doc.as_deref(), Some("adds"));
        assert_eq!(sigs[1].doc, None);

        let actions = json!([
            { "title": "Import Foo", "kind": "quickfix" },
            { "title": "Inline" }
        ]);
        let acts = parse_code_actions(&actions);
        assert_eq!(acts.len(), 2);
        assert_eq!(acts[0].kind.as_deref(), Some("quickfix"));
    }

    #[test]
    fn parse_diagnostics_notification() {
        let params = json!({
            "uri": "file:///src/lib.rs",
            "diagnostics": [
                { "range": { "start": { "line": 6, "character": 4 }, "end": { "line": 6, "character": 9 } },
                  "severity": 1, "message": "mismatched types", "source": "rustc", "code": "E0308" },
                { "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 1 } },
                  "severity": 2, "message": "unused", "code": 42 }
            ]
        });
        let pd = parse_published_diagnostics(&params, Path::new("/wt")).unwrap();
        assert_eq!(pd.root, Path::new("/wt"));
        assert_eq!(pd.path, "/src/lib.rs");
        assert_eq!(pd.diagnostics.len(), 2);
        assert_eq!(pd.diagnostics[0].severity, LspSeverity::Error);
        assert_eq!(pd.diagnostics[0].line, 6);
        assert_eq!(pd.diagnostics[0].code.as_deref(), Some("E0308"));
        assert_eq!(pd.diagnostics[1].severity, LspSeverity::Warning);
        assert_eq!(pd.diagnostics[1].code.as_deref(), Some("42"));
    }

    #[test]
    fn server_spec_argv_is_command_then_args() {
        let spec = ServerSpec {
            key: "rust".into(),
            language_id: "rust".into(),
            command: "rust-analyzer".into(),
            args: vec!["--x".into()],
        };
        assert_eq!(spec.argv(), vec!["rust-analyzer", "--x"]);
    }

    #[test]
    fn capabilities_parse_the_bool_or_object_union() {
        // rust-analyzer-style: mostly options objects.
        let ra = json!({ "capabilities": {
            "documentSymbolProvider": true,
            "hoverProvider": true,
            "referencesProvider": true,
            "definitionProvider": true,
            "signatureHelpProvider": { "triggerCharacters": ["(", ","] },
            "codeActionProvider": { "codeActionKinds": ["quickfix"] },
            "workspaceSymbolProvider": true,
        }});
        let caps = ServerCapabilities::from_initialize_result(&ra);
        assert!(caps.supports(LspMethod::DocumentSymbol));
        assert!(caps.supports(LspMethod::SignatureHelp)); // options object counts
        assert!(caps.supports(LspMethod::CodeAction));
        assert!(caps.supports(LspMethod::WorkspaceSymbol));
    }

    #[test]
    fn absent_or_false_provider_is_not_supported() {
        // A minimal server: only hover, and an explicit `false` elsewhere.
        let minimal = json!({ "capabilities": {
            "hoverProvider": true,
            "documentSymbolProvider": false,
        }});
        let caps = ServerCapabilities::from_initialize_result(&minimal);
        assert!(caps.supports(LspMethod::Hover));
        assert!(!caps.supports(LspMethod::DocumentSymbol)); // explicit false
        assert!(!caps.supports(LspMethod::References)); // absent
        assert!(!caps.supports(LspMethod::CodeAction)); // absent

        // No `capabilities` object at all ⇒ everything gated off.
        let empty = ServerCapabilities::from_initialize_result(&json!({}));
        assert_eq!(empty, ServerCapabilities::default());
        assert!(!empty.supports(LspMethod::Hover));

        // `null` provider ⇒ not supported.
        let nulled = json!({ "capabilities": { "hoverProvider": null } });
        assert!(!ServerCapabilities::from_initialize_result(&nulled).supports(LspMethod::Hover));
    }

    #[test]
    fn json_preflight_rejects_depth_width_and_decoded_strings_before_value() {
        let mut deep = String::new();
        for _ in 0..=limits::MAX_JSON_DEPTH {
            deep.push('[');
        }
        deep.push_str("0");
        for _ in 0..=limits::MAX_JSON_DEPTH {
            deep.push(']');
        }
        assert_eq!(preflight_json(deep.as_bytes()), Err("json depth limit"));

        let wide = format!(
            "[{}]",
            (0..=limits::MAX_CONTAINER_ITEMS)
                .map(|_| "0")
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(preflight_json(wide.as_bytes()), Err("json container limit"));

        let huge = format!(
            r#"{{"value":"{}"}}"#,
            "x".repeat(limits::MAX_SCALAR_STRING_BYTES + 1)
        );
        assert_eq!(preflight_json(huge.as_bytes()), Err("json string limit"));
    }

    #[test]
    fn iterative_symbol_projection_has_stable_total_cap() {
        let mut root = json!({
            "name": "root",
            "kind": 12,
            "range": { "start": { "line": 0, "character": 0 } }
        });
        for index in 0..=limits::MAX_RESULT_ITEMS {
            root = json!({
                "name": format!("s{index}"),
                "kind": 12,
                "range": { "start": { "line": index, "character": 0 } },
                "children": [root]
            });
        }
        let symbols = parse_symbols(&Value::Array(vec![root]), "/safe.rs");
        assert_eq!(symbols.len(), limits::MAX_RESULT_ITEMS);
        assert_eq!(symbols.first().map(|s| s.name.as_str()), Some("s4096"));
    }

    #[test]
    fn terminal_projection_removes_osc_and_preserves_unicode() {
        assert_eq!(
            sanitize_for_terminal("ok\u{1b}]0;evil\u{7} café\n"),
            "ok café\n"
        );
        assert_eq!(bounded_uri_to_path("file:///a%01b"), None);
        assert_eq!(bounded_uri_to_path("file:///a%20b"), Some("/a b".into()));
    }

    #[test]
    fn diagnostics_queue_coalesces_and_rejects_retired_generation() {
        let (tx, rx) = diagnostics_channel();
        tx.set_active(PathBuf::from("/r"), "rust".into(), 7);
        let publication = |generation, sequence, message| PublishedDiagnostics {
            root: PathBuf::from("/r"),
            path: "/r/a.rs".into(),
            diagnostics: vec![LspDiagnostic {
                line: 0,
                character: 0,
                severity: LspSeverity::Error,
                message: message.into(),
                code: None,
                source: None,
            }],
            server_identity: "rust".into(),
            generation,
            sequence,
            complete: true,
        };
        tx.publish(publication(7, 1, "old"));
        tx.publish(publication(7, 2, "new"));
        assert_eq!(rx.try_recv().unwrap().diagnostics[0].message, "new");
        tx.retire(Path::new("/r"), "rust", 7);
        tx.publish(publication(7, 3, "stale"));
        assert_eq!(rx.health().stale, 1);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn diagnostics_queue_rotates_ready_roots_under_flood() {
        let (tx, rx) = diagnostics_channel();
        let publication = |root: &str, path: &str| PublishedDiagnostics {
            root: PathBuf::from(root),
            path: path.into(),
            diagnostics: vec![],
            server_identity: "rust".into(),
            generation: 1,
            sequence: 1,
            complete: true,
        };
        for index in 0..10 {
            tx.publish(publication("/flood", &format!("/flood/{index}.rs")));
        }
        tx.publish(publication("/quiet", "/quiet/only.rs"));

        assert_eq!(rx.try_recv().unwrap().root, PathBuf::from("/flood"));
        assert_eq!(rx.try_recv().unwrap().root, PathBuf::from("/quiet"));
    }
}
