//! `StateRouter` — the *live-state* half of `thegn mcp serve`: tools that
//! answer from (or act on) a running instance (the pane daemon / DB cache),
//! beside the static docs tools in [`super::docs::DocsRouter`].
//!
//! Mirrors the `DocsRouter` pattern: this module is PURE. It owns the tool
//! *descriptions* (name, description, argument schema) and the routing +
//! scope policy; the actual data fetch (or daemon mutation) is injected by
//! the host as a closure (`FetchFn`), so daemon sockets, tokio and the DB
//! stay out of core.
//!
//! Scope model: the router is constructed with the *allowed* capability ids
//! (the host maps `--scopes` → caps via [`crate::control::required_scope`],
//! and MAY narrow that further with a tool-specific interlock — see
//! `sessions.input` in `thegn-host/src/cmd/mcp.rs`). `tools/list` advertises
//! only allowed tools; a `tools/call` for a known but not-allowed tool
//! answers a JSON-RPC error naming the missing scope, so an agent learns
//! *why* rather than seeing the tool vanish. Neither discovery nor
//! invocation trusts the other: `call()` re-checks `allowed` regardless of
//! what `tool_entries()` advertised.
//!
//! Tool names are the catalog ids' MCP-safe projection
//! ([`crate::capability::CapId::tool_name`]): `sessions.list` → `sessions_list`.
//!
//! Argument schema: each [`StateToolSpec`] declares its arguments as a flat
//! [`ArgSpec`] list — deliberately not a general JSON-Schema builder (see the
//! module doc on [`validate_args`]). A `tools/call` whose arguments don't
//! satisfy the declared schema never reaches the fetch closure — it is
//! rejected at this router with `-32602` (JSON-RPC "Invalid params").
//!
//! Audit: every successful call to a tool whose capability needs more than
//! `Read` scope logs a redacted `tracing` event (`target: "thegn::mcp"`) —
//! see [`redact_for_audit`].

use crate::capability::{lookup, scope_of};
use crate::control::Scope;
use serde_json::{Map, Value, json};

/// One declared argument's JSON type, for both `inputSchema` generation and
/// (cheap, structural) validation at the router boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    String,
    Integer,
    Boolean,
    /// A JSON array whose elements must all be strings (`argv`, …).
    StringArray,
    /// An unstructured JSON object (`env`, …) — validated as "is an object";
    /// any deeper shape (e.g. that its values are themselves strings) is the
    /// host fetch closure's job, not this router's — it is specific to one
    /// tool's semantics, not a generic argument shape.
    Object,
}

impl ArgKind {
    fn schema_type(self) -> &'static str {
        match self {
            ArgKind::String => "string",
            ArgKind::Integer => "integer",
            ArgKind::Boolean => "boolean",
            ArgKind::StringArray => "array",
            ArgKind::Object => "object",
        }
    }

    fn matches(self, v: &Value) -> bool {
        match self {
            ArgKind::String => v.is_string(),
            ArgKind::Integer => v.is_i64() || v.is_u64(),
            ArgKind::Boolean => v.is_boolean(),
            ArgKind::StringArray => v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
            ArgKind::Object => v.is_object(),
        }
    }
}

/// One declared tool argument.
pub struct ArgSpec {
    pub name: &'static str,
    pub kind: ArgKind,
    pub required: bool,
    pub description: &'static str,
}

/// One state tool: the catalog capability it projects, its MCP description,
/// and its declared arguments (`&[]` for a plain no-argument listing).
pub struct StateToolSpec {
    /// Catalog capability id (`sessions.list`); the tool name is its
    /// [`crate::capability::CapId::tool_name`] projection.
    pub cap: &'static str,
    pub description: &'static str,
    pub args: &'static [ArgSpec],
}

/// The implemented state tools. Must stay in step with [`MCP_STATE_CAPS`]
/// (pinned by `state_tools_match_state_caps`).
pub const STATE_TOOLS: &[StateToolSpec] = &[
    StateToolSpec {
        cap: "sessions.list",
        description: "List the thegn pane daemon's live terminal sessions: id, program, \
                      worktree, geometry, attached clients, lease state. Live state — \
                      requires a running daemon (start thegn or `thegn daemon`).",
        args: &[],
    },
    StateToolSpec {
        cap: "worktrees.list",
        description: "List the worktrees registered with thegn: path, branch, repo root, \
                      remote location. Answers from the daemon when one is running, else \
                      from thegn's local DB cache (`source` says which).",
        args: &[],
    },
    StateToolSpec {
        cap: "editor.open",
        description: "Queue a worktree or one of its relative files for handoff to the owning \
                      compositor's locally configured editor. The request never selects an \
                      executable, provider, argv or environment. Write-scoped (`--scopes \
                      write`). Requires a running daemon and compositor.",
        args: &[
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: true,
                description: "Absolute path of the worktree to open",
            },
            ArgSpec {
                name: "path",
                kind: ArgKind::String,
                required: false,
                description: "File path relative to the worktree",
            },
            ArgSpec {
                name: "line",
                kind: ArgKind::Integer,
                required: false,
                description: "1-based line number (requires path)",
            },
            ArgSpec {
                name: "col",
                kind: ArgKind::Integer,
                required: false,
                description: "1-based column number (requires path and line)",
            },
        ],
    },
    StateToolSpec {
        cap: "preview.fetch",
        description: "Fetch a preview URL through the daemon's bounded, credential-free HTTP \
                      client. Loopback targets only unless the operator explicitly enables \
                      external preview URLs. Read-scoped; requires a running daemon.",
        args: &[
            ArgSpec {
                name: "url",
                kind: ArgKind::String,
                required: true,
                description: "Absolute http/https preview URL",
            },
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: false,
                description: "Worktree used to select bounded dev-server pane diagnostics",
            },
            ArgSpec {
                name: "include_console",
                kind: ArgKind::Boolean,
                required: false,
                description: "Include source-labelled dev-server error lines (default false)",
            },
        ],
    },
    StateToolSpec {
        cap: "leases.list",
        description: "Relay lease state per session — which detached sessions are being \
                      kept warm, and until when. Requires a running daemon.",
        args: &[],
    },
    StateToolSpec {
        cap: "me",
        description: "The caller's identity as thegn's control plane sees it: pairing id, \
                      label and granted scopes. Requires a running daemon.",
        args: &[],
    },
    StateToolSpec {
        cap: "agent.sessions",
        description: "List discovered coding-agent sessions from each harness's local \
                      transcript store: harness, session id, worktree/project, last-modified \
                      time, and a one-line summary. Read-only — a bounded on-demand filesystem \
                      scan that never launches the harness, spends tokens, or returns \
                      credential material. Answers locally without a running daemon.",
        args: &[
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: false,
                description: "Only sessions whose recorded working dir is this worktree",
            },
            ArgSpec {
                name: "harness",
                kind: ArgKind::String,
                required: false,
                description: "Only sessions from this harness id (claude, codex, …)",
            },
        ],
    },
    StateToolSpec {
        cap: "sessions.wait",
        description: "Block until a session reaches a state: exited, idle, blocked, done, \
                      or its output matches a regex. Read-scoped (observes only). Requires \
                      a running daemon.",
        args: &[
            ArgSpec {
                name: "session",
                kind: ArgKind::String,
                required: true,
                description: "Target session id (see sessions_list)",
            },
            ArgSpec {
                name: "condition",
                kind: ArgKind::String,
                required: true,
                description: "exited | idle | blocked | done | match:<regex>",
            },
            ArgSpec {
                name: "timeout_ms",
                kind: ArgKind::Integer,
                required: false,
                description: "Milliseconds before giving up (omit to wait forever)",
            },
        ],
    },
    StateToolSpec {
        cap: "sessions.open",
        description: "Open a new terminal session: a raw argv, or a configured agent (an \
                      [[agents]]/[[tools]] name or provider id) launched with the daemon's \
                      full sandbox/env/credential composition. Write-scoped \
                      (`--scopes write`). Requires a running daemon.",
        args: &[
            ArgSpec {
                name: "argv",
                kind: ArgKind::StringArray,
                required: false,
                description: "Program + args (ignored when `agent` is set)",
            },
            ArgSpec {
                name: "cwd",
                kind: ArgKind::String,
                required: false,
                description: "Working directory",
            },
            ArgSpec {
                name: "env",
                kind: ArgKind::Object,
                required: false,
                description: "Extra environment variables (var → value, both strings)",
            },
            ArgSpec {
                name: "rows",
                kind: ArgKind::Integer,
                required: false,
                description: "PTY rows (default 24)",
            },
            ArgSpec {
                name: "cols",
                kind: ArgKind::Integer,
                required: false,
                description: "PTY columns (default 80)",
            },
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: false,
                description: "Worktree this session belongs to (listing/grouping hint)",
            },
            ArgSpec {
                name: "agent",
                kind: ArgKind::String,
                required: false,
                description: "An [[agents]]/[[tools]] name, or a provider id (claude, \
                              codex, …) — launches a configured agent instead of `argv`",
            },
            ArgSpec {
                name: "prompt",
                kind: ArgKind::String,
                required: false,
                description: "Task to seed the agent's first turn (with `agent` only)",
            },
            ArgSpec {
                name: "headless",
                kind: ArgKind::Boolean,
                required: false,
                description: "Run the agent headlessly rather than as an interactive TUI \
                              (with `agent` only; defaults to headless exactly when a \
                              prompt is given)",
            },
            ArgSpec {
                name: "bind_worktree",
                kind: ArgKind::Boolean,
                required: false,
                description: "Record this agent as the worktree's own, so resurrection \
                              relaunches it and the sidebar attributes its activity (with \
                              `agent` only)",
            },
            ArgSpec {
                name: "resume",
                kind: ArgKind::String,
                required: false,
                description: "Resume this harness session id (see agent_sessions) instead of \
                              launching cold — validated and refused if malformed (with \
                              `agent` only)",
            },
        ],
    },
    StateToolSpec {
        cap: "sessions.fork",
        description: "Fork a live daemon or recorded harness session into a new process. \
                      The source is never paused or cloned; a recorded source uses only \
                      the harness's native fork operation. Write-scoped \
                      (`--scopes write`). No argv, environment, prompt, transcript, or \
                      vendor file data is accepted. Requires a running daemon.",
        args: &[
            ArgSpec {
                name: "session",
                kind: ArgKind::String,
                required: true,
                description: "Live daemon session id, or native id from agent_sessions",
            },
            ArgSpec {
                name: "harness",
                kind: ArgKind::String,
                required: false,
                description: "Harness id when session is a recorded native session",
            },
            ArgSpec {
                name: "agent",
                kind: ArgKind::String,
                required: false,
                description: "Configured agent name for the fork launch context",
            },
            ArgSpec {
                name: "cwd",
                kind: ArgKind::String,
                required: false,
                description: "Working directory override",
            },
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: false,
                description: "Worktree override",
            },
            ArgSpec {
                name: "scrollback",
                kind: ArgKind::Boolean,
                required: false,
                description: "Request a bounded plain-text scrollback handoff file",
            },
            ArgSpec {
                name: "tab",
                kind: ArgKind::Boolean,
                required: false,
                description: "Adopt the child in a new tab",
            },
            ArgSpec {
                name: "adopt",
                kind: ArgKind::Boolean,
                required: false,
                description: "Ask a connected compositor to adopt the child",
            },
        ],
    },
    StateToolSpec {
        cap: "sessions.input",
        description: "Send terminal input to a live session — raw bytes to its stdin, \
                      exactly as if typed at its keyboard (control characters included). \
                      Write-scoped AND requires `thegn mcp serve --allow-session-input` \
                      (an interlock beyond `write` — see the mcp-write-tools design doc: \
                      typing into an arbitrary live session is a materially larger blast \
                      radius than opening or killing one). Requires a running daemon.",
        args: &[
            ArgSpec {
                name: "session",
                kind: ArgKind::String,
                required: true,
                description: "Target session id (see sessions_list)",
            },
            ArgSpec {
                name: "text",
                kind: ArgKind::String,
                required: false,
                description: "UTF-8 text to send (exactly one of `text`/`bytes_b64`)",
            },
            ArgSpec {
                name: "bytes_b64",
                kind: ArgKind::String,
                required: false,
                description: "Base64-encoded raw bytes to send — the only way to send \
                              control characters, e.g. Ctrl-C is base64 of byte 0x03",
            },
            ArgSpec {
                name: "enter",
                kind: ArgKind::Boolean,
                required: false,
                description: "Append a carriage return after the input (send-and-run)",
            },
        ],
    },
    StateToolSpec {
        cap: "sessions.kill",
        description: "Kill a session's process. Idempotent — killing an already-dead \
                      session succeeds. Write-scoped (`--scopes write`). Requires a \
                      running daemon.",
        args: &[ArgSpec {
            name: "session",
            kind: ArgKind::String,
            required: true,
            description: "Target session id (see sessions_list)",
        }],
    },
    StateToolSpec {
        cap: "semantic.map",
        description: "A ranked, line-budgeted repo map of a worktree: its \
                      tree-sitter-indexed entities (functions, types, …) grouped by \
                      file, most-referenced first — the outline coding agents inject \
                      for context. Reads the entity index (no language server needed); \
                      builds it inline, capped, on first use. Read-scoped. Does NOT \
                      require a running daemon.",
        args: &[
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: false,
                description: "Worktree path (default: the server's working directory)",
            },
            ArgSpec {
                name: "budget",
                kind: ArgKind::Integer,
                required: false,
                description: "Line budget for the map (default: [semantic] map_budget_lines)",
            },
            ArgSpec {
                name: "file",
                kind: ArgKind::String,
                required: false,
                description: "Narrow to one file's outline (path relative to the worktree)",
            },
        ],
    },
    StateToolSpec {
        cap: "semantic.blast_radius",
        description: "The blast-radius of a worktree's pending changes: the changed \
                      entities with their callers, the untested set, and the overall \
                      risk band, from the persisted semantic graph. Read-scoped. \
                      Returns a clear \"graph unavailable\" result when no graph exists \
                      (LSP off / never built / no dependents). Does NOT require a \
                      running daemon.",
        args: &[ArgSpec {
            name: "worktree",
            kind: ArgKind::String,
            required: false,
            description: "Worktree path (default: the server's working directory)",
        }],
    },
];

/// Host capabilities the MCP server exposes as state tools, by catalog id.
/// The docs tools (`search_docs`, `read_doc`, …) are not catalog items — they
/// read the embedded help corpus, not a running instance. Every other
/// `Surface::Mcp` catalog row is still excused in
/// `capability::SURFACE_GAPS`; this table is the thing that grows to retire
/// those excuses (`mcp_tools_cover_catalog` arbitrates both directions).
pub const MCP_STATE_CAPS: &[&str] = &[
    "sessions.list",
    "worktrees.list",
    "editor.open",
    "preview.fetch",
    "leases.list",
    "me",
    "agent.sessions",
    "sessions.wait",
    "sessions.open",
    "sessions.fork",
    "sessions.input",
    "sessions.kill",
    "semantic.map",
    "semantic.blast_radius",
];

/// The injected data fetch: `(capability id, tool arguments) → payload JSON`.
/// `Err(msg)` becomes a JSON-RPC error (e.g. "daemon not reachable — …").
/// Arguments have already passed [`validate_args`] against the tool's
/// declared [`ArgSpec`]s by the time this is called.
pub type FetchFn<'a> = dyn Fn(&str, &Value) -> Result<Value, String> + 'a;

/// Routes `tools/list` entries and `tools/call`s for the state tools.
/// Composed into [`super::docs::DocsRouter`] via `with_state`, so one MCP
/// endpoint serves docs + state in a single `tools/list` reply.
pub struct StateRouter<'a> {
    /// Allowed capability ids (host-computed from `--scopes` and any
    /// tool-specific interlock, e.g. `--allow-session-input`).
    allowed: Vec<&'static str>,
    fetch: Box<FetchFn<'a>>,
}

impl<'a> StateRouter<'a> {
    pub fn new(
        allowed: Vec<&'static str>,
        fetch: impl Fn(&str, &Value) -> Result<Value, String> + 'a,
    ) -> Self {
        Self {
            allowed,
            fetch: Box::new(fetch),
        }
    }

    /// The `tools/list` entries for the *allowed* tools.
    pub fn tool_entries(&self) -> Vec<Value> {
        STATE_TOOLS
            .iter()
            .filter(|t| self.allowed.contains(&t.cap))
            .map(|t| {
                json!({
                    "name": tool_name(t.cap),
                    "description": t.description,
                    "inputSchema": input_schema(t.args),
                })
            })
            .collect()
    }

    /// Handle a `tools/call` if `name` is a state tool (allowed or not);
    /// `None` means "not mine" so the caller can fall through to its own
    /// unknown-tool error.
    pub fn call(&self, name: &str, args: &Value) -> Option<Result<Value, (i32, String)>> {
        let spec = STATE_TOOLS.iter().find(|t| tool_name(t.cap) == name)?;
        if !self.allowed.contains(&spec.cap) {
            let scope = lookup(spec.cap)
                .map(|c| scope_of(c).as_str())
                .unwrap_or("read");
            return Some(Err((
                -32001,
                format!(
                    "tool `{name}` requires scope `{scope}` — grant it with \
                     `thegn mcp serve --scopes {scope}`"
                ),
            )));
        }
        if let Err(msg) = validate_args(spec.args, args) {
            return Some(Err((-32602, msg)));
        }
        let mutating = lookup(spec.cap).is_some_and(|c| scope_of(c) != Scope::Read);
        if mutating {
            tracing::info!(
                target: "thegn::mcp",
                cap = spec.cap,
                args = %redact_for_audit(spec.cap, args),
                "mcp tool call",
            );
        }
        let result = (self.fetch)(spec.cap, args);
        if mutating {
            match &result {
                Ok(_) => tracing::info!(target: "thegn::mcp", cap = spec.cap, "mcp tool call ok"),
                Err(e) => {
                    tracing::warn!(target: "thegn::mcp", cap = spec.cap, error = %e, "mcp tool call failed")
                }
            }
        }
        Some(match result {
            Ok(v) => Ok(super::docs::text_result(super::docs::pretty(&v))),
            Err(msg) => Err((-32000, msg)),
        })
    }
}

/// Build a tool's `inputSchema` from its declared arguments.
/// `additionalProperties: false`: an unexpected field is far more likely a
/// typo or a stale client than an intentional forward-compatible extra, and
/// rejecting it beats silently ignoring it.
fn input_schema(args: &[ArgSpec]) -> Value {
    if args.is_empty() {
        return json!({ "type": "object", "properties": {} });
    }
    let mut properties = Map::new();
    let mut required = Vec::new();
    for a in args {
        let mut prop = json!({ "type": a.kind.schema_type(), "description": a.description });
        if a.kind == ArgKind::StringArray {
            prop["items"] = json!({ "type": "string" });
        }
        properties.insert(a.name.to_string(), prop);
        if a.required {
            required.push(a.name);
        }
    }
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required,
        "additionalProperties": false,
    })
}

/// Validate `value` against a tool's declared arguments: `Value::Null` is
/// treated as an empty object (so no-arg tools keep accepting the bare `{}`/
/// omitted `arguments` they always have); any other non-object is rejected;
/// each declared arg is checked for presence (if `required`) and JSON type;
/// any key not in the declared set is rejected. Pure; the router calls this
/// before the fetch closure ever sees `value`.
pub fn validate_args(args: &[ArgSpec], value: &Value) -> Result<(), String> {
    let empty = Map::new();
    let obj = match value {
        Value::Null => &empty,
        Value::Object(m) => m,
        other => {
            return Err(format!(
                "arguments must be a JSON object, got {}",
                type_name(other)
            ));
        }
    };
    for a in args {
        match obj.get(a.name) {
            Some(v) if !a.kind.matches(v) => {
                return Err(format!(
                    "argument `{}` must be of type {}",
                    a.name,
                    a.kind.schema_type()
                ));
            }
            None if a.required => {
                return Err(format!("missing required argument `{}`", a.name));
            }
            _ => {}
        }
    }
    let known: std::collections::HashSet<&str> = args.iter().map(|a| a.name).collect();
    for k in obj.keys() {
        if !known.contains(k.as_str()) {
            return Err(format!("unknown argument `{k}`"));
        }
    }
    Ok(())
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Redact a mutating tool call's arguments for the audit log: any value that
/// can carry a secret (terminal input bytes, launch environment variables)
/// becomes a non-reversible size descriptor rather than being omitted — the
/// log still shows *that* a value was present and roughly how large it was,
/// never its content. Every other field survives unredacted: they name what
/// ran and where, not a secret.
pub fn redact_for_audit(cap: &str, args: &Value) -> Value {
    let Value::Object(obj) = args else {
        return args.clone();
    };
    let mut out = obj.clone();
    match cap {
        "sessions.input" => {
            for key in ["text", "bytes_b64"] {
                if let Some(v) = out.get(key).and_then(Value::as_str) {
                    let n = v.len();
                    out.insert(key.to_string(), json!(format!("<{n} bytes>")));
                }
            }
        }
        "sessions.open" => {
            if let Some(Value::Object(env)) = out.get("env") {
                let n = env.len();
                out.insert("env".to_string(), json!(format!("<{n} vars>")));
            }
        }
        _ => {}
    }
    Value::Object(out)
}

/// The catalog id's MCP tool-name projection (`sessions.list` → `sessions_list`).
fn tool_name(cap: &str) -> String {
    cap.replace('.', "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Surface, coverage_problems};

    fn all_caps() -> Vec<&'static str> {
        MCP_STATE_CAPS.to_vec()
    }

    #[test]
    fn mcp_tools_cover_catalog() {
        let problems = coverage_problems(Surface::Mcp, MCP_STATE_CAPS);
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn state_tools_match_state_caps() {
        let spec_caps: Vec<&str> = STATE_TOOLS.iter().map(|t| t.cap).collect();
        assert_eq!(spec_caps, MCP_STATE_CAPS);
    }

    #[test]
    fn tool_entries_advertise_only_allowed_tools() {
        let r = StateRouter::new(vec!["me", "worktrees.list"], |_, _| Ok(json!(null)));
        let names: Vec<String> = r
            .tool_entries()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["worktrees_list", "me"]);
        // No-arg tools keep the exact pre-existing schema shape (no
        // additionalProperties/required leaking onto tools that never
        // declared any args).
        for t in r.tool_entries() {
            assert!(!t["description"].as_str().unwrap().is_empty());
            assert_eq!(t["inputSchema"]["type"], "object");
            assert_eq!(
                t["inputSchema"],
                json!({ "type": "object", "properties": {} })
            );
        }
    }

    #[test]
    fn tool_entries_with_args_declare_schema() {
        let r = StateRouter::new(vec!["sessions.kill"], |_, _| Ok(json!(null)));
        let entries = r.tool_entries();
        let schema = &entries[0]["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["session"]["type"], "string");
        assert_eq!(schema["required"], json!(["session"]));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn editor_open_schema_has_only_the_safe_target_arguments() {
        let r = StateRouter::new(vec!["editor.open"], |_, _| Ok(json!(null)));
        let entries = r.tool_entries();
        let schema = &entries[0]["inputSchema"];
        let properties = schema["properties"].as_object().unwrap();
        let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["col", "line", "path", "worktree"]);
        assert_eq!(schema["required"], json!(["worktree"]));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn tool_entries_string_array_declares_items() {
        let r = StateRouter::new(vec!["sessions.open"], |_, _| Ok(json!(null)));
        let entries = r.tool_entries();
        let schema = &entries[0]["inputSchema"];
        assert_eq!(schema["properties"]["argv"]["type"], "array");
        assert_eq!(schema["properties"]["argv"]["items"]["type"], "string");
    }

    #[test]
    fn call_routes_cap_id_to_fetch() {
        let r = StateRouter::new(all_caps(), |cap, _args| Ok(json!({ "cap": cap })));
        let res = r.call("sessions_list", &json!({})).unwrap().unwrap();
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("sessions.list"), "{text}");
    }

    #[test]
    fn call_surfaces_fetch_error() {
        let r = StateRouter::new(all_caps(), |_, _| {
            Err("daemon not reachable — start thegn or `thegn daemon`".to_string())
        });
        let (code, msg) = r.call("me", &json!({})).unwrap().unwrap_err();
        assert_eq!(code, -32000);
        assert!(msg.contains("daemon not reachable"), "{msg}");
    }

    #[test]
    fn call_denied_names_the_missing_scope() {
        let r = StateRouter::new(vec![], |_, _| Ok(json!(null)));
        let (code, msg) = r.call("worktrees_list", &json!({})).unwrap().unwrap_err();
        assert_eq!(code, -32001);
        assert!(msg.contains("scope `read`"), "{msg}");
        assert!(msg.contains("--scopes read"), "{msg}");
    }

    #[test]
    fn call_denied_for_write_scoped_tool_names_write() {
        let r = StateRouter::new(vec![], |_, _| Ok(json!(null)));
        let (code, msg) = r
            .call("sessions_kill", &json!({"session":"s1"}))
            .unwrap()
            .unwrap_err();
        assert_eq!(code, -32001);
        assert!(msg.contains("scope `write`"), "{msg}");

        let (code, msg) = r
            .call("editor_open", &json!({"worktree":"/w"}))
            .unwrap()
            .unwrap_err();
        assert_eq!(code, -32001);
        assert!(msg.contains("scope `write`"), "{msg}");
    }

    #[test]
    fn call_unknown_tool_is_not_mine() {
        let r = StateRouter::new(all_caps(), |_, _| Ok(json!(null)));
        assert!(r.call("read_doc", &json!({})).is_none());
        assert!(r.call("nope", &json!({})).is_none());
    }

    #[test]
    fn call_bad_args_never_reaches_fetch() {
        // A fetch that panics if invoked proves invalid args short-circuit
        // before any daemon call is made.
        let r = StateRouter::new(all_caps(), |cap, _| {
            panic!("fetch should not be called for bad args (cap={cap})")
        });
        let (code, msg) = r.call("sessions_kill", &json!({})).unwrap().unwrap_err();
        assert_eq!(code, -32602);
        assert!(msg.contains("session"), "{msg}");
    }

    #[test]
    fn call_bad_arg_type_is_invalid_params() {
        let r = StateRouter::new(all_caps(), |_, _| panic!("should not be called"));
        let (code, msg) = r
            .call("sessions_kill", &json!({"session": 5}))
            .unwrap()
            .unwrap_err();
        assert_eq!(code, -32602);
        assert!(msg.contains("session"), "{msg}");
    }

    #[test]
    fn call_unknown_arg_is_invalid_params() {
        let r = StateRouter::new(all_caps(), |_, _| panic!("should not be called"));
        let (code, _) = r
            .call("sessions_kill", &json!({"session": "s1", "extra": true}))
            .unwrap()
            .unwrap_err();
        assert_eq!(code, -32602);
    }

    #[test]
    fn call_valid_args_reaches_fetch() {
        let r = StateRouter::new(all_caps(), |cap, args| {
            Ok(json!({"cap": cap, "args": args}))
        });
        let res = r
            .call("sessions_kill", &json!({"session": "s1"}))
            .unwrap()
            .unwrap();
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("s1"), "{text}");
    }

    #[test]
    fn every_state_cap_maps_to_the_scope_it_documents() {
        // The deliberate split this change introduces: listing/observing
        // tools stay Read (unchanged default `--scopes read` still covers
        // them); mutating tools require Write. This replaces
        // `every_state_cap_is_read_scope_today`, whose own doc comment
        // predicted exactly this: "pins that a future write-side tool
        // forces a deliberate scope-model decision rather than silently
        // widening `read`."
        let read = [
            "sessions.list",
            "worktrees.list",
            "preview.fetch",
            "leases.list",
            "me",
            "agent.sessions",
            "sessions.wait",
            "semantic.map",
            "semantic.blast_radius",
        ];
        let write = [
            "editor.open",
            "sessions.open",
            "sessions.fork",
            "sessions.input",
            "sessions.kill",
        ];
        for cap in read {
            let c = lookup(cap).expect("state cap in catalog");
            assert_eq!(scope_of(c), Scope::Read, "{cap}");
        }
        for cap in write {
            let c = lookup(cap).expect("state cap in catalog");
            assert_eq!(scope_of(c), Scope::Write, "{cap}");
        }
        // Exhaustive: every implemented cap is in one of the two groups.
        let mut grouped: Vec<&str> = read.iter().chain(write.iter()).copied().collect();
        let mut all: Vec<&str> = MCP_STATE_CAPS.to_vec();
        grouped.sort_unstable();
        all.sort_unstable();
        assert_eq!(all, grouped);
    }

    // --- validate_args -------------------------------------------------

    #[test]
    fn validate_args_no_args_accepts_null_and_empty_object() {
        assert!(validate_args(&[], &Value::Null).is_ok());
        assert!(validate_args(&[], &json!({})).is_ok());
    }

    #[test]
    fn validate_args_rejects_non_object() {
        let err = validate_args(&[], &json!("nope")).unwrap_err();
        assert!(err.contains("string"), "{err}");
        let err = validate_args(&[], &json!([1, 2])).unwrap_err();
        assert!(err.contains("array"), "{err}");
    }

    #[test]
    fn validate_args_required_missing() {
        let spec = [ArgSpec {
            name: "session",
            kind: ArgKind::String,
            required: true,
            description: "",
        }];
        let err = validate_args(&spec, &json!({})).unwrap_err();
        assert!(err.contains("session"), "{err}");
        // Null is treated as an empty object, so a required field is still missing.
        let err = validate_args(&spec, &Value::Null).unwrap_err();
        assert!(err.contains("session"), "{err}");
    }

    #[test]
    fn validate_args_optional_missing_is_ok() {
        let spec = [ArgSpec {
            name: "timeout_ms",
            kind: ArgKind::Integer,
            required: false,
            description: "",
        }];
        assert!(validate_args(&spec, &json!({})).is_ok());
    }

    #[test]
    fn validate_args_type_mismatch_per_kind() {
        let cases: &[(ArgKind, Value, Value)] = &[
            (ArgKind::String, json!("ok"), json!(5)),
            (ArgKind::Integer, json!(5), json!("nope")),
            (ArgKind::Boolean, json!(true), json!("nope")),
            (ArgKind::StringArray, json!(["a", "b"]), json!(["a", 1])),
            (ArgKind::Object, json!({"a": "b"}), json!("nope")),
        ];
        for (kind, good, bad) in cases {
            let spec = [ArgSpec {
                name: "x",
                kind: *kind,
                required: true,
                description: "",
            }];
            assert!(
                validate_args(&spec, &json!({"x": good})).is_ok(),
                "{kind:?} should accept {good:?}"
            );
            assert!(
                validate_args(&spec, &json!({"x": bad})).is_err(),
                "{kind:?} should reject {bad:?}"
            );
        }
    }

    #[test]
    fn validate_args_unknown_key_rejected() {
        let spec = [ArgSpec {
            name: "session",
            kind: ArgKind::String,
            required: true,
            description: "",
        }];
        let err = validate_args(&spec, &json!({"session": "s1", "bogus": 1})).unwrap_err();
        assert!(err.contains("bogus"), "{err}");
    }

    // --- redact_for_audit ------------------------------------------------

    #[test]
    fn redact_for_audit_hides_session_input_bytes() {
        let secret = "super secret token";
        let args = json!({"session": "s1", "text": secret, "enter": true});
        let red = redact_for_audit("sessions.input", &args);
        assert_eq!(red["session"], "s1");
        assert_eq!(red["enter"], true);
        let text = red["text"].as_str().unwrap();
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains(&format!("{} bytes", secret.len())), "{text}");
    }

    #[test]
    fn redact_for_audit_hides_session_open_env_values() {
        let args = json!({
            "argv": ["node"],
            "env": {"API_KEY": "sk-verysecret", "OTHER": "x"},
        });
        let red = redact_for_audit("sessions.open", &args);
        assert_eq!(red["argv"], json!(["node"]));
        let env = red["env"].as_str().unwrap();
        assert!(!env.contains("sk-verysecret"), "{env}");
        assert!(env.contains("2 vars"), "{env}");
    }

    #[test]
    fn redact_for_audit_identity_for_other_caps() {
        let args = json!({"session": "s1"});
        assert_eq!(redact_for_audit("sessions.kill", &args), args);
        assert_eq!(redact_for_audit("sessions.list", &args), args);
    }
}
