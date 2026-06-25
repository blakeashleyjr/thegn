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

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

use superzej_core::semantic::Lang;

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
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LspError::NotAvailable => write!(f, "no language server available"),
            LspError::Spawn(e) => write!(f, "language server spawn failed: {e}"),
            LspError::Timeout => write!(f, "language server request timed out"),
            LspError::Protocol(e) => write!(f, "language server protocol error: {e}"),
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

/// A server-pushed diagnostics set for one document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedDiagnostics {
    pub path: String,
    pub diagnostics: Vec<LspDiagnostic>,
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

// ─── server registry ────────────────────────────────────────────────────────

/// A configured/known language server command for one language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub lang: Lang,
    pub command: String,
    pub args: Vec<String>,
}

/// A user config override for one language's server command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerOverride {
    /// Language key: "rust", "typescript", "tsx", "javascript", "python", "go".
    pub lang: String,
    pub command: String,
    pub args: Vec<String>,
}

/// The canonical config key for a language.
pub fn lang_key(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "rust",
        Lang::TypeScript => "typescript",
        Lang::Tsx => "tsx",
        Lang::JavaScript => "javascript",
        Lang::Python => "python",
        Lang::Go => "go",
    }
}

/// The `languageId` the server expects in `didOpen`.
pub fn language_id(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "rust",
        Lang::TypeScript => "typescript",
        Lang::Tsx => "typescriptreact",
        Lang::JavaScript => "javascript",
        Lang::Python => "python",
        Lang::Go => "go",
    }
}

/// The built-in default server command for a language.
fn default_command(lang: Lang) -> (&'static str, &'static [&'static str]) {
    match lang {
        Lang::Rust => ("rust-analyzer", &[]),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            ("typescript-language-server", &["--stdio"])
        }
        Lang::Python => ("pyright-langserver", &["--stdio"]),
        Lang::Go => ("gopls", &[]),
    }
}

/// The built-in default server specs for every supported language.
pub fn default_servers() -> Vec<ServerSpec> {
    [
        Lang::Rust,
        Lang::TypeScript,
        Lang::Tsx,
        Lang::JavaScript,
        Lang::Python,
        Lang::Go,
    ]
    .into_iter()
    .map(|lang| {
        let (cmd, args) = default_command(lang);
        ServerSpec {
            lang,
            command: cmd.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    })
    .collect()
}

/// Resolve the server to launch for `lang`: a config override wins outright;
/// otherwise the built-in default, but only if its binary is found on `PATH`.
pub fn resolve_server(lang: Lang, overrides: &[ServerOverride]) -> Option<ServerSpec> {
    resolve_server_with(lang, overrides, binary_on_path)
}

/// Testable core of [`resolve_server`] with an injectable existence check.
fn resolve_server_with(
    lang: Lang,
    overrides: &[ServerOverride],
    exists: impl Fn(&str) -> bool,
) -> Option<ServerSpec> {
    let key = lang_key(lang);
    if let Some(ov) = overrides.iter().find(|o| o.lang.eq_ignore_ascii_case(key)) {
        if ov.command.is_empty() {
            return None; // explicit "disable this language"
        }
        return Some(ServerSpec {
            lang,
            command: ov.command.clone(),
            args: ov.args.clone(),
        });
    }
    let (cmd, args) = default_command(lang);
    if !exists(cmd) {
        return None;
    }
    Some(ServerSpec {
        lang,
        command: cmd.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
    })
}

/// Whether `cmd` resolves to a file (absolute/relative path or a `PATH` lookup).
fn binary_on_path(cmd: &str) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).is_file();
    }
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(cmd).is_file())
}

// ─── uri ⇄ path ──────────────────────────────────────────────────────────────

/// Encode an absolute filesystem path as a `file://` URI.
pub fn path_to_uri(path: &str) -> String {
    format!("file://{}", percent_encode_path(path))
}

/// Decode a `file://` URI back to a filesystem path (best-effort).
pub fn uri_to_path(uri: &str) -> String {
    let body = uri.strip_prefix("file://").unwrap_or(uri);
    percent_decode(body)
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
            path: uri_to_path(uri.as_str()?),
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
            path: uri_to_path(uri),
            range,
        });
    }
    None
}

/// Parse a definition/references result: `Location | Location[] | LocationLink[]`.
pub fn parse_locations(result: &Value) -> Vec<Location> {
    match result {
        Value::Array(items) => items.iter().filter_map(location_from_json).collect(),
        Value::Object(_) => location_from_json(result).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Parse a `documentSymbol`/`workspace/symbol` result, handling both the
/// hierarchical `DocumentSymbol[]` and the flat `SymbolInformation[]` shapes.
pub fn parse_symbols(result: &Value, fallback_path: &str) -> Vec<SymbolInfo> {
    let Some(items) = result.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        collect_symbol(item, fallback_path, None, &mut out);
    }
    out
}

fn collect_symbol(
    v: &Value,
    fallback_path: &str,
    container: Option<&str>,
    out: &mut Vec<SymbolInfo>,
) {
    let Some(name) = v.get("name").and_then(Value::as_str) else {
        return;
    };
    let kind = SymbolKind::from_lsp(v.get("kind").and_then(Value::as_i64).unwrap_or(0));

    // `SymbolInformation` carries a `location`; `DocumentSymbol` carries
    // `range`/`selectionRange` and is scoped to the queried document.
    let location = if let Some(loc) = v.get("location").and_then(location_from_json) {
        loc
    } else {
        let range = v
            .get("selectionRange")
            .or_else(|| v.get("range"))
            .map(range_from_json)
            .unwrap_or_default();
        Location {
            path: fallback_path.to_string(),
            range,
        }
    };

    out.push(SymbolInfo {
        name: name.to_string(),
        kind,
        location,
        container: v
            .get("containerName")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| container.map(str::to_string)),
    });

    // DocumentSymbol children are nested under this symbol's name.
    if let Some(children) = v.get("children").and_then(Value::as_array) {
        for child in children {
            collect_symbol(child, fallback_path, Some(name), out);
        }
    }
}

/// Parse a `textDocument/hover` result into flattened markdown.
pub fn parse_hover(result: &Value) -> Option<HoverInfo> {
    if result.is_null() {
        return None;
    }
    let contents = result.get("contents")?;
    let markdown = flatten_hover_contents(contents);
    if markdown.trim().is_empty() {
        return None;
    }
    Some(HoverInfo {
        markdown,
        range: result.get("range").map(range_from_json),
    })
}

fn flatten_hover_contents(v: &Value) -> String {
    match v {
        // `MarkedString` as `{ language, value }` (a code block) — check before
        // the bare-`value` arm, since this shape also carries a `value`.
        Value::Object(o) if o.contains_key("language") => {
            let lang = o.get("language").and_then(Value::as_str).unwrap_or("");
            let value = o.get("value").and_then(Value::as_str).unwrap_or("");
            format!("```{lang}\n{value}\n```")
        }
        // `MarkupContent { kind, value }`.
        Value::Object(o) if o.contains_key("value") => o
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(flatten_hover_contents)
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Parse a `textDocument/signatureHelp` result.
pub fn parse_signatures(result: &Value) -> Vec<SignatureInfo> {
    let Some(sigs) = result.get("signatures").and_then(Value::as_array) else {
        return Vec::new();
    };
    sigs.iter()
        .filter_map(|s| {
            let label = s.get("label").and_then(Value::as_str)?.to_string();
            let doc = s
                .get("documentation")
                .map(flatten_hover_contents)
                .filter(|d| !d.trim().is_empty());
            Some(SignatureInfo { label, doc })
        })
        .collect()
}

/// Parse a `textDocument/codeAction` result (`(Command | CodeAction)[]`).
pub fn parse_code_actions(result: &Value) -> Vec<CodeActionInfo> {
    let Some(items) = result.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| {
            let title = v.get("title").and_then(Value::as_str)?.to_string();
            Some(CodeActionInfo {
                title,
                kind: v.get("kind").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect()
}

/// Parse a `publishDiagnostics` notification's params.
pub fn parse_published_diagnostics(params: &Value) -> Option<PublishedDiagnostics> {
    let path = uri_to_path(params.get("uri").and_then(Value::as_str)?);
    let diagnostics = params
        .get("diagnostics")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(parse_one_diagnostic)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(PublishedDiagnostics { path, diagnostics })
}

fn parse_one_diagnostic(v: &Value) -> Option<LspDiagnostic> {
    let start = v.get("range")?.get("start").map(position_from_json)?;
    let severity = LspSeverity::from_lsp(v.get("severity").and_then(Value::as_i64).unwrap_or(1));
    let code = v.get("code").and_then(|c| match c {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    Some(LspDiagnostic {
        line: start.line,
        character: start.character,
        severity,
        message: v
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        code,
        source: v.get("source").and_then(Value::as_str).map(str::to_string),
    })
}

// ─── the client ──────────────────────────────────────────────────────────────

type Pending = Arc<Mutex<HashMap<i64, Sender<Result<Value, LspError>>>>>;

/// A live connection to one language-server subprocess.
pub struct LspClient {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Mutex<Child>,
    next_id: AtomicI64,
    pending: Pending,
    root: PathBuf,
    timeout: Duration,
    _reader: JoinHandle<()>,
}

impl LspClient {
    /// Spawn and connect to the server described by `spec`, rooted at `root`.
    /// `diag_tx` receives every `publishDiagnostics` notification.
    pub fn start(
        spec: &ServerSpec,
        root: &Path,
        diag_tx: Sender<PublishedDiagnostics>,
    ) -> Result<LspClient, LspError> {
        let mut child = Command::new(&spec.command)
            .args(&spec.args)
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
        let stdin = Arc::new(Mutex::new(stdin));

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader = {
            let pending = pending.clone();
            let stdin = stdin.clone();
            thread::spawn(move || reader_loop(stdout, pending, diag_tx, stdin))
        };

        Ok(LspClient {
            stdin,
            child: Mutex::new(child),
            next_id: AtomicI64::new(1),
            pending,
            root: root.to_path_buf(),
            timeout: Duration::from_secs(10),
            _reader: reader,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Run the `initialize`/`initialized` handshake.
    pub fn initialize(&self, root: &Path) -> Result<(), LspError> {
        let uri = path_to_uri(&root.to_string_lossy());
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
        self.request("initialize", params)?;
        self.notify("initialized", json!({}))
    }

    /// Tell the server a document is open (text is the on-disk content).
    pub fn did_open(&self, uri: &str, lang: Lang, text: &str) -> Result<(), LspError> {
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri,
                "languageId": language_id(lang),
                "version": 1,
                "text": text,
            }}),
        )
    }

    /// Document symbols (the outline) for an opened document.
    pub fn document_symbols(&self, uri: &str) -> Result<Vec<SymbolInfo>, LspError> {
        let res = self.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        )?;
        Ok(parse_symbols(&res, &uri_to_path(uri)))
    }

    /// Workspace-wide symbol search.
    pub fn workspace_symbols(&self, query: &str) -> Result<Vec<SymbolInfo>, LspError> {
        let res = self.request("workspace/symbol", json!({ "query": query }))?;
        Ok(parse_symbols(&res, ""))
    }

    /// Definition location(s) for a position.
    pub fn definition(&self, uri: &str, pos: Position) -> Result<Vec<Location>, LspError> {
        let res = self.request("textDocument/definition", self.pos_params(uri, pos))?;
        Ok(parse_locations(&res))
    }

    /// Reference location(s) for a position.
    pub fn references(&self, uri: &str, pos: Position) -> Result<Vec<Location>, LspError> {
        let res = self.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": pos.line, "character": pos.character },
                "context": { "includeDeclaration": false },
            }),
        )?;
        Ok(parse_locations(&res))
    }

    /// Hover content for a position.
    pub fn hover(&self, uri: &str, pos: Position) -> Result<Option<HoverInfo>, LspError> {
        let res = self.request("textDocument/hover", self.pos_params(uri, pos))?;
        Ok(parse_hover(&res))
    }

    /// Signature help for a position.
    pub fn signature_help(&self, uri: &str, pos: Position) -> Result<Vec<SignatureInfo>, LspError> {
        let res = self.request("textDocument/signatureHelp", self.pos_params(uri, pos))?;
        Ok(parse_signatures(&res))
    }

    /// Code actions offered for a range.
    pub fn code_actions(&self, uri: &str, range: Range) -> Result<Vec<CodeActionInfo>, LspError> {
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
        Ok(parse_code_actions(&res))
    }

    fn pos_params(&self, uri: &str, pos: Position) -> Value {
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": pos.line, "character": pos.character },
        })
    }

    /// Send a request and block (up to `timeout`) for its correlated response.
    fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);

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

    fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write(&body.to_string())
    }

    fn write(&self, body: &str) -> Result<(), LspError> {
        let framed = framing::encode(body);
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|_| LspError::Protocol("stdin poisoned".into()))?;
        stdin
            .write_all(&framed)
            .and_then(|_| stdin.flush())
            .map_err(|e| LspError::Protocol(e.to_string()))
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Best-effort graceful exit, then make sure the child is gone.
        let _ = self.notify("exit", Value::Null);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Read framed messages off the server's stdout until EOF, dispatching each.
fn reader_loop(
    stdout: std::process::ChildStdout,
    pending: Pending,
    diag_tx: Sender<PublishedDiagnostics>,
    stdin: Arc<Mutex<ChildStdin>>,
) {
    let mut decoder = framing::FrameDecoder::new();
    let mut reader = BufReader::new(stdout);
    let mut chunk = [0u8; 8192];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        decoder.push(&chunk[..n]);
        while let Some(body) = decoder.next_message() {
            if let Ok(msg) = serde_json::from_str::<Value>(&body) {
                dispatch(&msg, &pending, &diag_tx, &stdin);
            }
        }
    }
    // Stream closed — unblock any waiters so they don't hang to the deadline.
    let mut map = pending.lock().unwrap();
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(LspError::Protocol("server stream closed".into())));
    }
}

fn dispatch(
    msg: &Value,
    pending: &Pending,
    diag_tx: &Sender<PublishedDiagnostics>,
    stdin: &Arc<Mutex<ChildStdin>>,
) {
    let id = msg.get("id").and_then(Value::as_i64);
    let method = msg.get("method").and_then(Value::as_str);

    match (id, method) {
        // Response to one of our requests.
        (Some(id), None) => {
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let payload = if let Some(err) = msg.get("error") {
                    Err(LspError::Protocol(err.to_string()))
                } else {
                    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = tx.send(payload);
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
                let _ = s.write_all(&framing::encode(&body));
                let _ = s.flush();
            }
        }
        // Notification.
        (None, Some("textDocument/publishDiagnostics")) => {
            if let Some(params) = msg.get("params")
                && let Some(pd) = parse_published_diagnostics(params)
            {
                let _ = diag_tx.send(pd);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let pd = parse_published_diagnostics(&params).unwrap();
        assert_eq!(pd.path, "/src/lib.rs");
        assert_eq!(pd.diagnostics.len(), 2);
        assert_eq!(pd.diagnostics[0].severity, LspSeverity::Error);
        assert_eq!(pd.diagnostics[0].line, 6);
        assert_eq!(pd.diagnostics[0].code.as_deref(), Some("E0308"));
        assert_eq!(pd.diagnostics[1].severity, LspSeverity::Warning);
        assert_eq!(pd.diagnostics[1].code.as_deref(), Some("42"));
    }

    #[test]
    fn resolve_prefers_override_then_default_on_path() {
        let overrides = vec![ServerOverride {
            lang: "rust".into(),
            command: "my-ra".into(),
            args: vec!["--x".into()],
        }];
        // Override wins regardless of PATH.
        let spec = resolve_server_with(Lang::Rust, &overrides, |_| false).unwrap();
        assert_eq!(spec.command, "my-ra");
        assert_eq!(spec.args, vec!["--x".to_string()]);

        // Default used when present on PATH…
        let spec = resolve_server_with(Lang::Go, &[], |c| c == "gopls").unwrap();
        assert_eq!(spec.command, "gopls");
        // …and None when the binary is missing.
        assert_eq!(resolve_server_with(Lang::Go, &[], |_| false), None);
    }

    #[test]
    fn empty_override_command_disables_language() {
        let overrides = vec![ServerOverride {
            lang: "python".into(),
            command: String::new(),
            args: vec![],
        }];
        assert_eq!(
            resolve_server_with(Lang::Python, &overrides, |_| true),
            None
        );
    }

    #[test]
    fn default_servers_cover_all_languages() {
        let servers = default_servers();
        assert_eq!(servers.len(), 6);
        assert!(servers.iter().any(|s| s.command == "rust-analyzer"));
        assert!(servers.iter().any(|s| s.command == "gopls"));
    }

    #[test]
    fn language_ids_are_server_expected() {
        assert_eq!(language_id(Lang::Tsx), "typescriptreact");
        assert_eq!(language_id(Lang::Rust), "rust");
        assert_eq!(lang_key(Lang::JavaScript), "javascript");
    }
}
