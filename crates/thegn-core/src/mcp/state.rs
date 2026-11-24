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
        cap: "worktrees.sandbox_profile",
        description: "Read a worktree's pinned sandbox profile (`standard` or `sealed`), or \
                      null when nothing is pinned and the configured policy applies. \
                      Read-only: pinning and clearing are host-owned and not available \
                      over MCP. Requires a running daemon.",
        args: &[ArgSpec {
            name: "worktree",
            kind: ArgKind::String,
            required: true,
            description: "Absolute path of a registered worktree, exactly as registered",
        }],
    },
    StateToolSpec {
        cap: "folders.assign",
        description: "Assign a repo-local sidebar folder to a Git worktree, or clear its folder. \
                      Folder names are trimmed and matched case-insensitively within that \
                      worktree's repository. Pass exactly one of `folder` (assign) or \
                      `clear: true` (unfile); clearing never deletes the folder. \
                      Git-scoped; requires a running daemon.",
        args: &[
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: true,
                description: "Path inside the Git worktree",
            },
            ArgSpec {
                name: "folder",
                kind: ArgKind::String,
                required: false,
                description: "Folder name to assign; exclusive with `clear`",
            },
            ArgSpec {
                name: "clear",
                kind: ArgKind::Boolean,
                required: false,
                description: "true to unfile the worktree; exclusive with `folder`",
            },
        ],
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
        cap: "git.status",
        description: "List changed files in a worktree. Defaults to the server's resolved \
                      worktree when `worktree` is omitted. Read-scoped; requires a running \
                      daemon.",
        args: &[ArgSpec {
            name: "worktree",
            kind: ArgKind::String,
            required: false,
            description: "Absolute path of the worktree; defaults to the resolved worktree",
        }],
    },
    StateToolSpec {
        cap: "merge.list",
        description: "List the merge queue for a worktree's repository. Defaults to the \
                      server's resolved worktree when `worktree` is omitted. Read-scoped; \
                      requires a running daemon.",
        args: &[ArgSpec {
            name: "worktree",
            kind: ArgKind::String,
            required: false,
            description: "Absolute path of the worktree; defaults to the resolved worktree",
        }],
    },
    StateToolSpec {
        cap: "merge.add",
        description: "Enqueue a worktree's branch on its repository's merge queue. Defaults \
                      to the server's resolved worktree when `worktree` is omitted. \
                      Additive: it never removes queue entries. Git-scoped \
                      (`--scopes git`; the narrow `merge_add` grant is for worktree-bound \
                      route tokens and does not enable this tool); requires a running daemon. \
                      Refused for a Lead: it lands only through a rule's land step or an owner \
                      approval.",
        args: &[ArgSpec {
            name: "worktree",
            kind: ArgKind::String,
            required: false,
            description: "Absolute path of the worktree; defaults to the resolved worktree",
        }],
    },
    StateToolSpec {
        cap: "pr.status",
        description: "Read cached pull-request status, one row per worktree. Read-scoped; \
                      requires a running daemon.",
        args: &[],
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
        cap: "ci.runs",
        description: "List cached CI runs and their bounded job metadata for a worktree. \
                      Read-only; provider refreshes happen in the daemon's blocking lane.",
        args: &[
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: false,
                description: "Worktree path (default: the server's working directory)",
            },
            ArgSpec {
                name: "limit",
                kind: ArgKind::Integer,
                required: false,
                description: "Maximum number of cached runs to return",
            },
        ],
    },
    StateToolSpec {
        cap: "ci.logs",
        description: "Read a bounded, redacted cached CI job-log tail. Read-only; a cache \
                      miss may be filled by the daemon's blocking provider lane.",
        args: &[
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: false,
                description: "Worktree path (default: the server's working directory)",
            },
            ArgSpec {
                name: "run",
                kind: ArgKind::String,
                required: true,
                description: "CI run id",
            },
            ArgSpec {
                name: "job",
                kind: ArgKind::String,
                required: true,
                description: "CI job id",
            },
            ArgSpec {
                name: "tail_lines",
                kind: ArgKind::Integer,
                required: false,
                description: "Optional smaller tail selection",
            },
        ],
    },
    StateToolSpec {
        cap: "calendar.events",
        description: "Read the merged, recurrence-expanded calendar events in an inclusive \
                      ISO-date window. Read-only; requires a running daemon.",
        args: &[
            ArgSpec {
                name: "from",
                kind: ArgKind::String,
                required: true,
                description: "Inclusive start date (YYYY-MM-DD)",
            },
            ArgSpec {
                name: "to",
                kind: ArgKind::String,
                required: true,
                description: "Inclusive end date (YYYY-MM-DD)",
            },
        ],
    },
    StateToolSpec {
        cap: "calendar.clocks",
        description: "Read the resolved home and configured world clocks. Read-only; \
                      requires a running daemon.",
        args: &[],
    },
    StateToolSpec {
        cap: "issues.list",
        description: "List tracker issues matching optional status, project, query, and \
                      limit filters. Read-only; requires a running daemon.",
        args: &[
            ArgSpec {
                name: "status",
                kind: ArgKind::String,
                required: false,
                description: "Comma-separated status names",
            },
            ArgSpec {
                name: "limit",
                kind: ArgKind::Integer,
                required: false,
                description: "Maximum number of issues",
            },
            ArgSpec {
                name: "project",
                kind: ArgKind::String,
                required: false,
                description: "Project id filter",
            },
            ArgSpec {
                name: "query",
                kind: ArgKind::String,
                required: false,
                description: "Text query filter",
            },
            ArgSpec {
                name: "repo",
                kind: ArgKind::String,
                required: false,
                description: "Repository path for its issues configuration overlay",
            },
        ],
    },
    StateToolSpec {
        cap: "issues.get",
        description: "Read one tracker issue with its detail and comments. Read-only; \
                      requires a running daemon.",
        args: &[
            ArgSpec {
                name: "id",
                kind: ArgKind::String,
                required: true,
                description: "Tracker issue id",
            },
            ArgSpec {
                name: "repo",
                kind: ArgKind::String,
                required: false,
                description: "Repository path for its issues configuration overlay",
            },
            ArgSpec {
                name: "account",
                kind: ArgKind::String,
                required: false,
                description: "Configured tracker account that owns the issue",
            },
        ],
    },
    StateToolSpec {
        cap: "issues.update",
        description: "Patch one tracker issue (the wire mirror of `POST /v1/issues/{id}`); \
                      send at least one of `status`, `assignee`, `priority`, `title`. \
                      Returns the updated issue. Changes shared tracker state. \
                      Write-scoped (`--scopes write`); requires a running daemon.",
        args: &[
            ArgSpec {
                name: "id",
                kind: ArgKind::String,
                required: true,
                description: "Tracker issue id (see issues_list)",
            },
            ArgSpec {
                name: "status",
                kind: ArgKind::String,
                required: false,
                description: "New status: backlog, todo, in_progress, done or cancelled",
            },
            ArgSpec {
                name: "assignee",
                kind: ArgKind::String,
                required: false,
                description: "`me` assigns the caller, `none` unassigns the caller",
            },
            ArgSpec {
                name: "priority",
                kind: ArgKind::Integer,
                required: false,
                description: "Priority 0-4 (0 none, 1 urgent, 2 high, 3 medium, 4 low)",
            },
            ArgSpec {
                name: "title",
                kind: ArgKind::String,
                required: false,
                description: "New title (non-empty)",
            },
            ArgSpec {
                name: "repo",
                kind: ArgKind::String,
                required: false,
                description: "Repository path for its issues configuration overlay",
            },
            ArgSpec {
                name: "account",
                kind: ArgKind::String,
                required: false,
                description: "Configured tracker account that owns the issue",
            },
        ],
    },
    StateToolSpec {
        cap: "issues.comment",
        description: "Post a comment on a tracker issue (the wire mirror of \
                      `POST /v1/issues/{id}/comment`). `body` is non-empty and at most \
                      64 KiB. Appends only. Write-scoped (`--scopes write`); requires a \
                      running daemon.",
        args: &[
            ArgSpec {
                name: "id",
                kind: ArgKind::String,
                required: true,
                description: "Tracker issue id (see issues_list)",
            },
            ArgSpec {
                name: "body",
                kind: ArgKind::String,
                required: true,
                description: "Comment text (non-empty, at most 65536 bytes)",
            },
            ArgSpec {
                name: "repo",
                kind: ArgKind::String,
                required: false,
                description: "Repository path for its issues configuration overlay",
            },
            ArgSpec {
                name: "account",
                kind: ArgKind::String,
                required: false,
                description: "Configured tracker account that owns the issue",
            },
        ],
    },
    StateToolSpec {
        cap: "dispatches.list",
        description: "List the agent-dispatch roster newest-first, with optional keyset \
                      pagination. Read-only; requires a running daemon.",
        args: &[
            ArgSpec {
                name: "limit",
                kind: ArgKind::Integer,
                required: false,
                description: "Rows per page",
            },
            ArgSpec {
                name: "before",
                kind: ArgKind::Integer,
                required: false,
                description: "Last row id from the previous page",
            },
        ],
    },
    StateToolSpec {
        cap: "dispatches.put",
        description: "Append one row to the agent-dispatch roster (the wire mirror of \
                      `POST /v1/dispatches`); returns the stored row. A lease-bound row \
                      needs a `lead` proof. Write-scoped (`--scopes write`); requires a \
                      running daemon.",
        args: &[
            ArgSpec {
                name: "issue_id",
                kind: ArgKind::String,
                required: true,
                description: "Tracker issue id the dispatch works on",
            },
            ArgSpec {
                name: "worktree_path",
                kind: ArgKind::String,
                required: true,
                description: "Absolute path of the worktree the agent runs in",
            },
            ArgSpec {
                name: "agent_name",
                kind: ArgKind::String,
                required: true,
                description: "Roster agent name (e.g. a `[[pipeline.stages]]` agent)",
            },
            ArgSpec {
                name: "stage",
                kind: ArgKind::String,
                required: false,
                description: "Pipeline stage this row is (stored and grouped by only)",
            },
            ArgSpec {
                name: "parent_id",
                kind: ArgKind::Integer,
                required: false,
                description: "Roster row this one was chunked out of",
            },
            ArgSpec {
                name: "session_id",
                kind: ArgKind::String,
                required: false,
                description: "Daemon session running this dispatch",
            },
            ArgSpec {
                name: "artifact_path",
                kind: ArgKind::String,
                required: false,
                description: "Path to the handoff artifact committed in the worktree",
            },
            ArgSpec {
                name: "lead",
                kind: ArgKind::Object,
                required: false,
                description: "Lead-lease proof `{lease, owner, lease_gen?}`",
            },
        ],
    },
    StateToolSpec {
        cap: "dispatches.set_status",
        description: "Set one dispatch row's status (the wire mirror of \
                      `POST /v1/dispatches/{id}/status`). `status` is a member of the \
                      closed snake_case dispatch-status set, checked by the daemon. A \
                      lease-bound row needs a `lead` proof. Write-scoped \
                      (`--scopes write`); requires a running daemon.",
        args: &[
            ArgSpec {
                name: "id",
                kind: ArgKind::Integer,
                required: true,
                description: "Dispatch row id (see dispatches_list)",
            },
            ArgSpec {
                name: "status",
                kind: ArgKind::String,
                required: true,
                description: "New status (snake_case dispatch-status name)",
            },
            ArgSpec {
                name: "lead",
                kind: ArgKind::Object,
                required: false,
                description: "Lead-lease proof `{lease, owner, lease_gen?}`",
            },
        ],
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
        description: "Block until a session reaches a state: exited | idle | blocked | done | \
                      match:<regex> | screen:<regex> | stable:<ms>. Read-scoped (observes only). \
                      Requires a running daemon.",
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
                description: "exited | idle | blocked | done | match:<regex> | screen:<regex> | stable:<ms>",
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
                      Exactly one of `text`, `bytes_b64` or `keys`. \
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
                description: "UTF-8 text to send (exactly one of `text`, `bytes_b64` or `keys`)",
            },
            ArgSpec {
                name: "bytes_b64",
                kind: ArgKind::String,
                required: false,
                description: "Base64-encoded raw bytes to send — the only way to send \
                              control characters, e.g. Ctrl-C is base64 of byte 0x03",
            },
            ArgSpec {
                name: "keys",
                kind: ArgKind::StringArray,
                required: false,
                description: "Named keys to send in order (up, esc, enter, ctrl+c, f5…; at most 64), encoded for the pane's cursor-key mode",
            },
            ArgSpec {
                name: "paste",
                kind: ArgKind::Boolean,
                required: false,
                description: "With text: paste it (bracketed when the pane enabled bracketed paste)",
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
        cap: "sessions.resize",
        description: "Resize a live session's PTY to `rows` x `cols` (the wire mirror of \
                      `POST /v1/sessions/{s}/resize`). Reversible: it never sends input or \
                      ends the session. Write-scoped (`--scopes write`); requires a \
                      running daemon.",
        args: &[
            ArgSpec {
                name: "session",
                kind: ArgKind::String,
                required: true,
                description: "Target session id (see sessions_list)",
            },
            ArgSpec {
                name: "rows",
                kind: ArgKind::Integer,
                required: true,
                description: "New terminal height in rows (at most 1000)",
            },
            ArgSpec {
                name: "cols",
                kind: ArgKind::Integer,
                required: true,
                description: "New terminal width in columns (at most 1000)",
            },
        ],
    },
    StateToolSpec {
        cap: "sessions.snapshot",
        description: "The current emulator screen of a live session (the wire mirror of \
                      `GET /v1/sessions/{s}/snapshot`): `{session, seq, cols, rows, \
                      ansi_b64, text, cursor, alt_screen, app_cursor, bracketed_paste}` in \
                      the answer. Read-scoped; requires a running daemon.",
        args: &[ArgSpec {
            name: "session",
            kind: ArgKind::String,
            required: true,
            description: "Target session id (see sessions_list)",
        }],
    },
    StateToolSpec {
        cap: "sessions.split",
        description: "Create a sibling pane/session next to an existing one (the wire \
                      mirror of `POST /v1/sessions/{s}/split`). It launches `argv` (a login \
                      shell when empty) but never sends input to the target. Write-scoped \
                      (`--scopes write`); requires a running daemon.",
        args: &[
            ArgSpec {
                name: "session",
                kind: ArgKind::String,
                required: true,
                description: "Existing session to split beside (see sessions_list)",
            },
            ArgSpec {
                name: "dir",
                kind: ArgKind::String,
                required: false,
                description: "Split direction: `right` (default) or `down`",
            },
            ArgSpec {
                name: "argv",
                kind: ArgKind::StringArray,
                required: false,
                description: "Program + args for the new pane (a login shell when empty)",
            },
            ArgSpec {
                name: "cwd",
                kind: ArgKind::String,
                required: false,
                description: "Working directory (inherits the target's when omitted)",
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
                description: "PTY rows (0 or omitted inherits the target's; at most 1000)",
            },
            ArgSpec {
                name: "cols",
                kind: ArgKind::Integer,
                required: false,
                description: "PTY columns (0 or omitted inherits the target's; at most 1000)",
            },
        ],
    },
    StateToolSpec {
        cap: "worktrees.open",
        description: "Open/focus a workspace in the running thegn instance (the wire mirror \
                      of `thegn open`): queues a focus request the compositor claims within \
                      about a second. It never creates a worktree or repository on disk. \
                      Write-scoped (`--scopes write`); requires a running daemon.",
        args: &[
            ArgSpec {
                name: "repo",
                kind: ArgKind::String,
                required: true,
                description: "Repository root path of the workspace to focus",
            },
            ArgSpec {
                name: "branch",
                kind: ArgKind::String,
                required: false,
                description: "Branch hint, accepted for parity with the HTTP body; \
                              branch selection stays a compositor concern",
            },
        ],
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
    StateToolSpec {
        cap: "notify.push",
        description: "Push a notification into thegn's tray (the wire mirror of `thegn notify \
                      push`). `urgency` `alert`/`critical` raise the red-flag priority; \
                      anything else lands at the normal notice tier. Additive. Write-scoped \
                      (`--scopes write`); requires a running daemon.",
        args: &[
            ArgSpec {
                name: "title",
                kind: ArgKind::String,
                required: true,
                description: "Short summary line (the inbox message when `body` is empty)",
            },
            ArgSpec {
                name: "body",
                kind: ArgKind::String,
                required: false,
                description: "Longer detail, appended to the inbox message when non-empty",
            },
            ArgSpec {
                name: "urgency",
                kind: ArgKind::String,
                required: false,
                description: "`alert` or `critical` for the red-flag tier; otherwise normal",
            },
            ArgSpec {
                name: "source",
                kind: ArgKind::String,
                required: false,
                description: "Opaque source reference stored on the row (defaults to `api`)",
            },
        ],
    },
    StateToolSpec {
        cap: "automations.list",
        description: "List trusted global/profile automation rules, inert reasons, and recent audited outcomes. Read-only.",
        args: &[],
    },
    StateToolSpec {
        cap: "automations.test",
        description: "Purely evaluate one named automation rule against a normalized event fixture. Never executes or writes state.",
        args: &[
            ArgSpec {
                name: "rule",
                kind: ArgKind::String,
                required: true,
                description: "Stable automation rule name",
            },
            ArgSpec {
                name: "event",
                kind: ArgKind::Object,
                required: true,
                description: "Normalized AutomationEvent JSON object",
            },
            ArgSpec {
                name: "at",
                kind: ArgKind::Integer,
                required: false,
                description: "Evaluation clock in Unix seconds (defaults to event.occurred_at)",
            },
        ],
    },
    StateToolSpec {
        cap: "tools.run",
        description: "Run one trusted configured [[tools]] entry by name in an optional worktree. Exec-scoped; command and sandbox resolve on the daemon.",
        args: &[
            ArgSpec {
                name: "name",
                kind: ArgKind::String,
                required: true,
                description: "Configured [[tools]] name",
            },
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: false,
                description: "Worktree in which to run the tool",
            },
        ],
    },
    // --- Lead tool surface (THE-2172) ---
    StateToolSpec {
        cap: "overview.get",
        description: "One-call cross-workspace overview for the Lead: worktrees, agents, queues and what needs attention. Read-scoped; requires a running daemon.",
        args: &[
            ArgSpec {
                name: "workspace",
                kind: ArgKind::String,
                required: false,
                description: "Restrict the overview to one workspace (repo root)",
            },
            ArgSpec {
                name: "limit",
                kind: ArgKind::Integer,
                required: false,
                description: "Maximum rows per section",
            },
        ],
    },
    // b163 (THE-2214): no arguments, like a bare `overview.get`.
    StateToolSpec {
        cap: "producers.status",
        description: "The daemon's headless producers: which kinds are armed, who holds each lease, and each inbox account's last poll, error (scrubbed) and item count. Read-scoped; requires a running daemon.",
        args: &[],
    },
    StateToolSpec {
        cap: "notifications.list",
        description: "List notifications, newest first, filtered by kind, unread state, age and worktree. Read-scoped; requires a running daemon.",
        args: &[
            ArgSpec {
                name: "kind",
                kind: ArgKind::String,
                required: false,
                description: "Notification kind to match",
            },
            ArgSpec {
                name: "unread",
                kind: ArgKind::Boolean,
                required: false,
                description: "true for unread notifications only",
            },
            ArgSpec {
                name: "since",
                kind: ArgKind::Integer,
                required: false,
                description: "Only notifications at or after this Unix time (milliseconds)",
            },
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: false,
                description: "Only notifications for this worktree path",
            },
            ArgSpec {
                name: "limit",
                kind: ArgKind::Integer,
                required: false,
                description: "Maximum number of notifications to return",
            },
        ],
    },
    StateToolSpec {
        cap: "notifications.ack",
        description: "Acknowledge (mark read) notifications: pass exactly one of `id`, `all: true` or `worktree`. Write-scoped (`--scopes write`); requires a running daemon.",
        args: &[
            ArgSpec {
                name: "id",
                kind: ArgKind::Integer,
                required: false,
                description: "One notification id",
            },
            ArgSpec {
                name: "all",
                kind: ArgKind::Boolean,
                required: false,
                description: "true to acknowledge every notification",
            },
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: false,
                description: "Acknowledge every notification for this worktree path",
            },
        ],
    },
    StateToolSpec {
        cap: "automations.put",
        description: "Create or replace a Lead-authored automation rule. Lead rules may only notify, open a session, wake the Lead or propose a pipeline (pipeline.start at autonomy = \"propose\" only); they never merge, and config rules are read-only. Write-scoped (`--scopes write`); requires a running daemon.",
        args: &[ArgSpec {
            name: "rule",
            kind: ArgKind::Object,
            required: true,
            description: "The automation rule (the [[automations]] table shape)",
        }],
    },
    StateToolSpec {
        cap: "automations.rm",
        description: "Remove a Lead-authored automation rule by name; config rules answer a conflict. Write-scoped (`--scopes write`); requires a running daemon.",
        args: &[ArgSpec {
            name: "name",
            kind: ArgKind::String,
            required: true,
            description: "Rule name",
        }],
    },
    StateToolSpec {
        cap: "automations.enable",
        description: "Enable or disable a Lead-authored automation rule by name; config rules answer a conflict. Write-scoped (`--scopes write`); requires a running daemon.",
        args: &[
            ArgSpec {
                name: "name",
                kind: ArgKind::String,
                required: true,
                description: "Rule name",
            },
            ArgSpec {
                name: "enabled",
                kind: ArgKind::Boolean,
                required: true,
                description: "true to enable, false to disable",
            },
        ],
    },
    StateToolSpec {
        cap: "automations.runs",
        description: "Recent audited runs of an automation rule (or of every rule). Read-scoped; requires a running daemon.",
        args: &[
            ArgSpec {
                name: "rule",
                kind: ArgKind::String,
                required: false,
                description: "Rule name; omit for every rule",
            },
            ArgSpec {
                name: "limit",
                kind: ArgKind::Integer,
                required: false,
                description: "Maximum number of runs to return",
            },
        ],
    },
    StateToolSpec {
        cap: "approvals.list",
        description: "Pipeline approval history and the pending approval requests. Read-scoped; requires a running daemon.",
        args: &[
            ArgSpec {
                name: "issue_id",
                kind: ArgKind::String,
                required: false,
                description: "Only approvals for this issue id",
            },
            ArgSpec {
                name: "limit",
                kind: ArgKind::Integer,
                required: false,
                description: "Maximum number of rows to return",
            },
        ],
    },
    StateToolSpec {
        cap: "approvals.request",
        description: "Ask a human to approve a pipeline stage. It only asks: granting is an admin action that is never available over MCP. Write-scoped (`--scopes write`); requires a running daemon.",
        args: &[
            ArgSpec {
                name: "issue_id",
                kind: ArgKind::String,
                required: true,
                description: "Issue id the stage belongs to",
            },
            ArgSpec {
                name: "stage",
                kind: ArgKind::String,
                required: true,
                description: "Pipeline stage awaiting approval",
            },
            ArgSpec {
                name: "commit_sha",
                kind: ArgKind::String,
                required: true,
                description: "Commit the approval would cover",
            },
            ArgSpec {
                name: "worktree",
                kind: ArgKind::String,
                required: false,
                description: "Worktree path the stage runs in",
            },
            ArgSpec {
                name: "reason",
                kind: ArgKind::String,
                required: false,
                description: "Why approval is needed, shown to the human",
            },
        ],
    },
    StateToolSpec {
        cap: "merge.drain",
        description: "Start the merge-queue drain for a worktree's repository. Git-scoped (`--scopes git`; the narrow `merge_add` grant is for worktree-bound route tokens and does not enable this tool); requires a running daemon. Refused for a Lead: it lands only through a rule's land step or an owner approval.",
        args: &[ArgSpec {
            name: "worktree",
            kind: ArgKind::String,
            required: true,
            description: "Absolute path of a worktree in the repository to drain",
        }],
    },
    StateToolSpec {
        cap: "automations.pause",
        description: "Pause every automation rule: events are still audited (as paused) but nothing executes. Only the operator can resume; resuming is an admin action that is never available over MCP. Write-scoped (`--scopes write`); requires a running daemon.",
        args: &[],
    },
];

/// Host capabilities the MCP server exposes as state tools, by catalog id.
/// The docs tools (`search_docs`, `read_doc`, …) are not catalog items — they
/// read the embedded help corpus, not a running instance. Unimplemented
/// `Surface::Mcp` catalog rows remain excused in `capability::SURFACE_GAPS`;
/// this table is the thing that grows to retire those excuses
/// (`mcp_tools_cover_catalog` arbitrates both directions).
pub const MCP_STATE_CAPS: &[&str] = &[
    "sessions.list",
    "worktrees.list",
    "worktrees.sandbox_profile",
    "folders.assign",
    "editor.open",
    "preview.fetch",
    "git.status",
    "merge.list",
    "merge.add",
    "pr.status",
    "leases.list",
    "me",
    "ci.runs",
    "ci.logs",
    "calendar.events",
    "calendar.clocks",
    "issues.list",
    "issues.get",
    "issues.update",
    "issues.comment",
    "dispatches.list",
    "dispatches.put",
    "dispatches.set_status",
    "agent.sessions",
    "sessions.wait",
    "sessions.open",
    "sessions.fork",
    "sessions.input",
    "sessions.kill",
    "sessions.resize",
    "sessions.snapshot",
    "sessions.split",
    "worktrees.open",
    "semantic.map",
    "semantic.blast_radius",
    "notify.push",
    "automations.list",
    "automations.test",
    "tools.run",
    // Lead tool surface (THE-2172). `approvals.resolve` is Admin and never an
    // MCP tool.
    "overview.get",
    "producers.status", // b163: producer and inbox state (read).
    "notifications.list",
    "notifications.ack",
    "automations.put",
    "automations.rm",
    "automations.enable",
    "automations.runs",
    "approvals.list",
    "approvals.request",
    "merge.drain",
    // THE-2197: pausing is the Lead's kill switch; `automations.resume` is
    // Admin and never an MCP tool.
    "automations.pause",
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
    /// Self-asserted caller label (`THEGN_CALLER`), audit-only: it is logged
    /// on every mutating call and never used for authorization.
    caller: Option<String>,
}

impl<'a> StateRouter<'a> {
    pub fn new(
        allowed: Vec<&'static str>,
        fetch: impl Fn(&str, &Value) -> Result<Value, String> + 'a,
    ) -> Self {
        Self {
            allowed,
            fetch: Box::new(fetch),
            caller: None,
        }
    }

    /// Attach the audit-only caller label (see [`super::caller`]). Never
    /// consulted for authorization.
    #[must_use]
    pub fn with_caller(mut self, caller: Option<String>) -> Self {
        self.caller = caller;
        self
    }

    /// The audit caller label, `-` when none was set.
    pub fn caller(&self) -> &str {
        self.caller.as_deref().unwrap_or("-")
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
                    "annotations": tool_annotations(t.cap),
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
            let scope = if matches!(spec.cap, "merge.add" | "merge.drain") {
                "git"
            } else {
                lookup(spec.cap)
                    .map(|c| scope_of(c).as_str())
                    .unwrap_or("read")
            };
            return Some(Err((
                -32001,
                format!(
                    "tool `{name}` requires scope `{scope}` — grant it with \
                     `thegn mcp serve --scopes {scope}`"
                ),
            )));
        }
        if let Err(msg) =
            validate_args(spec.args, args).and_then(|()| validate_semantics(spec.cap, args))
        {
            return Some(Err((-32602, msg)));
        }
        let mutating = lookup(spec.cap).is_some_and(|c| scope_of(c) != Scope::Read);
        if mutating {
            tracing::info!(
                target: "thegn::mcp",
                cap = spec.cap,
                caller = self.caller(),
                args = %redact_for_audit(spec.cap, args),
                "mcp tool call",
            );
        }
        let result = (self.fetch)(spec.cap, args);
        if mutating {
            match &result {
                Ok(_) => tracing::info!(
                    target: "thegn::mcp",
                    cap = spec.cap,
                    caller = self.caller(),
                    "mcp tool call ok"
                ),
                Err(e) => tracing::warn!(
                    target: "thegn::mcp",
                    cap = spec.cap,
                    caller = self.caller(),
                    error = %e,
                    "mcp tool call failed"
                ),
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

/// Write-class MCP tools whose effect is additive or reversible (open/fork a
/// session, open an editor, assign a folder, resize a PTY, append a dispatch
/// row, focus a workspace): advertised with
/// `destructiveHint: false`. Every other non-read tool is advertised
/// destructive (fail closed), so a new write tool is destructive until it is
/// deliberately listed here.
const NON_DESTRUCTIVE_WRITE_CAPS: &[&str] = &[
    "issues.comment",
    "dispatches.put",
    "folders.assign",
    "editor.open",
    "merge.add",
    "notify.push",
    "sessions.open",
    "sessions.fork",
    "sessions.resize",
    "sessions.split",
    "worktrees.open",
    // Lead tool surface (THE-2172): acknowledging only marks rows read, a
    // request only asks, and enabling is reversible.
    "notifications.ack",
    "approvals.request",
    "automations.enable",
    // THE-2197: pausing only stops automations (the operator resumes).
    "automations.pause",
];

/// MCP `annotations` for a state tool, derived from the catalog scope. Hints
/// only: `--scopes` is the enforcement.
fn tool_annotations(cap: &str) -> Value {
    let read_only = lookup(cap).is_some_and(|c| scope_of(c) == Scope::Read);
    let destructive = !read_only && !NON_DESTRUCTIVE_WRITE_CAPS.contains(&cap);
    json!({ "readOnlyHint": read_only, "destructiveHint": destructive })
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

/// Longest `issues_comment` body, in bytes. The control route has no body cap
/// of its own beyond the transport limit, so the MCP door fixes one.
pub const MAX_ISSUE_COMMENT_BYTES: usize = 64 * 1024;

/// Cap-specific argument checks that a type check cannot express, run after
/// [`validate_args`] and before any fetch. Pure.
fn validate_semantics(cap: &str, args: &Value) -> Result<(), String> {
    let non_empty = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .is_some_and(|v| !v.trim().is_empty())
    };
    match cap {
        "issues.update" => {
            if !non_empty("id") {
                return Err("`id` must be a non-empty string".into());
            }
            if !["status", "assignee", "priority", "title"]
                .iter()
                .any(|k| args.get(*k).is_some())
            {
                return Err(
                    "send at least one of `status`, `assignee`, `priority`, `title`".into(),
                );
            }
            if let Some(p) = args.get("priority")
                && !p.as_i64().is_some_and(|n| (0..=4).contains(&n))
            {
                return Err("`priority` must be an integer from 0 to 4".into());
            }
            if let Some(a) = args.get("assignee")
                && !matches!(a.as_str(), Some("me" | "none"))
            {
                return Err("`assignee` must be `me` or `none`".into());
            }
            if args.get("title").is_some() && !non_empty("title") {
                return Err("`title` must not be empty".into());
            }
            if args.get("status").is_some() && !non_empty("status") {
                return Err("`status` must not be empty".into());
            }
        }
        "issues.comment" => {
            if !non_empty("id") {
                return Err("`id` must be a non-empty string".into());
            }
            if !non_empty("body") {
                return Err("`body` must not be empty".into());
            }
            let n = args["body"].as_str().map_or(0, str::len);
            if n > MAX_ISSUE_COMMENT_BYTES {
                return Err(format!(
                    "`body` is {n} bytes; the limit is {MAX_ISSUE_COMMENT_BYTES}"
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// A free-text value's audit stand-in: its length and a short SHA-256 prefix,
/// never the text.
fn text_digest(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(text.as_bytes());
    let hex: String = hash.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("<{} bytes sha256:{hex}>", text.len())
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
            if let Some(v) = out.get("keys").and_then(Value::as_array) {
                out.insert("keys".to_string(), json!(format!("<{} keys>", v.len())));
            }
        }
        "issues.comment" => {
            if let Some(v) = out.get("body").and_then(Value::as_str) {
                let digest = text_digest(v);
                out.insert("body".to_string(), json!(digest));
            }
        }
        "sessions.open" | "sessions.split" => {
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
    fn sandbox_profile_is_a_read_only_mcp_tool() {
        let r = StateRouter::new(vec!["worktrees.sandbox_profile"], |cap, args| {
            Ok(json!({ "cap": cap, "args": args }))
        });
        let entries = r.tool_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "worktrees_sandbox_profile");
        assert_eq!(entries[0]["inputSchema"]["required"], json!(["worktree"]));
        assert_eq!(entries[0]["annotations"]["readOnlyHint"], json!(true));
        assert_eq!(entries[0]["annotations"]["destructiveHint"], json!(false));
    }

    #[test]
    fn mirrored_read_tools_expose_optional_worktree_and_read_annotations() {
        let r = StateRouter::new(vec!["git.status", "merge.list", "pr.status"], |_, _| {
            Ok(json!(null))
        });
        let entries = r.tool_entries();
        assert_eq!(entries.len(), 3);
        for name in ["git_status", "merge_list", "pr_status"] {
            let entry = entries
                .iter()
                .find(|entry| entry["name"] == name)
                .expect(name);
            assert_eq!(entry["annotations"]["readOnlyHint"], json!(true));
            assert_eq!(entry["annotations"]["destructiveHint"], json!(false));
        }
        for name in ["git_status", "merge_list"] {
            let entry = entries
                .iter()
                .find(|entry| entry["name"] == name)
                .expect(name);
            assert_eq!(entry["inputSchema"]["required"], json!([]));
            assert_eq!(
                entry["inputSchema"]["properties"]["worktree"]["type"],
                "string"
            );
        }
        assert_eq!(
            entries[2]["inputSchema"],
            json!({
                "type": "object",
                "properties": {},
            })
        );
    }

    #[test]
    fn client_api_read_tools_expose_read_only_annotations() {
        let caps = [
            "calendar.events",
            "calendar.clocks",
            "issues.list",
            "issues.get",
            "dispatches.list",
        ];
        let r = StateRouter::new(caps.to_vec(), |_, _| Ok(json!(null)));
        let entries = r.tool_entries();
        assert_eq!(entries.len(), caps.len());
        for cap in caps {
            let entry = entries
                .iter()
                .find(|entry| entry["name"] == tool_name(cap))
                .expect(cap);
            assert_eq!(entry["annotations"]["readOnlyHint"], json!(true));
            assert_eq!(entry["annotations"]["destructiveHint"], json!(false));
        }
    }

    #[test]
    fn folder_assignment_exposes_optional_folder_and_routes_the_mutation() {
        let r = StateRouter::new(vec!["folders.assign"], |cap, args| {
            Ok(json!({ "cap": cap, "args": args }))
        });
        let entries = r.tool_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "folders_assign");
        assert_eq!(entries[0]["inputSchema"]["required"], json!(["worktree"]));
        assert_eq!(
            entries[0]["inputSchema"]["properties"]["folder"]["type"],
            "string"
        );
        assert_eq!(
            entries[0]["inputSchema"]["properties"]["clear"]["type"],
            "boolean"
        );
        let result = r
            .call(
                "folders_assign",
                &json!({ "worktree": "/repo/wt", "folder": " Agents " }),
            )
            .unwrap()
            .unwrap();
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("folders.assign")
        );
    }

    #[test]
    fn merge_add_and_notify_push_are_advertised_only_when_allowed() {
        let r = StateRouter::new(vec!["merge.list", "pr.status"], |_, _| Ok(json!(null)));
        let names: Vec<String> = r
            .tool_entries()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(!names.iter().any(|n| n == "merge_add" || n == "notify_push"));
        // A disallowed call is refused with the scope that would grant it,
        // and never reaches the fetch.
        let denied = StateRouter::new(vec!["merge.list"], |_, _| -> Result<Value, String> {
            panic!("a denied tool must not reach fetch")
        });
        let (code, msg) = denied.call("merge_add", &json!({})).unwrap().unwrap_err();
        assert_eq!(code, -32001);
        assert!(msg.contains("--scopes git"), "{msg}");
        let (code, msg) = denied
            .call("notify_push", &json!({ "title": "t" }))
            .unwrap()
            .unwrap_err();
        assert_eq!(code, -32001);
        assert!(msg.contains("--scopes write"), "{msg}");

        let r = StateRouter::new(vec!["merge.add", "notify.push"], |_, _| Ok(json!(null)));
        let entries = r.tool_entries();
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["merge_add", "notify_push"]);
        let merge = &entries[0]["inputSchema"];
        assert_eq!(merge["required"], json!([]));
        assert_eq!(merge["properties"]["worktree"]["type"], "string");
        let note = &entries[1]["inputSchema"];
        assert_eq!(note["required"], json!(["title"]));
        for key in ["title", "body", "urgency", "source"] {
            assert_eq!(note["properties"][key]["type"], "string", "{key}");
        }
        // Daemon-internal automation ancestry is never caller-supplied.
        assert!(note["properties"].get("automation_origin").is_none());
    }

    #[test]
    fn merge_add_rejects_a_non_string_worktree_before_fetch() {
        let r = StateRouter::new(vec!["merge.add"], |_, _| -> Result<Value, String> {
            panic!("invalid args must not reach fetch")
        });
        let (code, msg) = r
            .call("merge_add", &json!({ "worktree": 7 }))
            .unwrap()
            .unwrap_err();
        assert_eq!(code, -32602);
        assert!(msg.contains("`worktree`"), "{msg}");
    }

    #[test]
    fn notify_push_requires_a_string_title_and_rejects_ancestry() {
        let r = StateRouter::new(vec!["notify.push"], |_, _| -> Result<Value, String> {
            panic!("invalid args must not reach fetch")
        });
        for bad in [
            json!({}),
            json!({ "title": 1 }),
            json!({ "title": "t", "urgency": true }),
            json!({ "title": "t", "automation_origin": {} }),
        ] {
            let (code, _) = r.call("notify_push", &bad).unwrap().unwrap_err();
            assert_eq!(code, -32602, "{bad}");
        }
    }

    #[test]
    fn merge_add_and_notify_push_route_their_cap_ids_to_fetch() {
        let r = StateRouter::new(vec!["merge.add", "notify.push"], |cap, args| {
            Ok(json!({ "cap": cap, "args": args }))
        });
        let merged = r
            .call("merge_add", &json!({ "worktree": "/repo/wt" }))
            .unwrap()
            .unwrap();
        let text = merged["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("merge.add"), "{text}");
        assert!(text.contains("/repo/wt"), "{text}");
        // `worktree` is optional: the host resolves it when omitted.
        assert!(r.call("merge_add", &json!({})).unwrap().is_ok());
        let pushed = r
            .call(
                "notify_push",
                &json!({ "title": "done", "body": "b", "urgency": "alert", "source": "ci" }),
            )
            .unwrap()
            .unwrap();
        let text = pushed["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("notify.push"), "{text}");
        assert!(text.contains("done"), "{text}");
    }

    #[test]
    fn sessions_resize_and_worktrees_open_are_advertised_only_when_allowed() {
        let r = StateRouter::new(vec!["sessions.list", "worktrees.list"], |_, _| {
            Ok(json!(null))
        });
        let names: Vec<String> = r
            .tool_entries()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(
            !names
                .iter()
                .any(|n| n == "sessions_resize" || n == "worktrees_open")
        );
        // A disallowed call names the catalog scope and never reaches fetch.
        let denied = StateRouter::new(vec!["sessions.list"], |_, _| -> Result<Value, String> {
            panic!("a denied tool must not reach fetch")
        });
        for (tool, args, cap) in [
            (
                "sessions_resize",
                json!({ "session": "s1", "rows": 24, "cols": 80 }),
                "sessions.resize",
            ),
            (
                "worktrees_open",
                json!({ "repo": "/repo" }),
                "worktrees.open",
            ),
        ] {
            let (code, msg) = denied.call(tool, &args).unwrap().unwrap_err();
            assert_eq!(code, -32001);
            let scope = scope_of(lookup(cap).unwrap()).as_str();
            assert!(msg.contains(&format!("--scopes {scope}")), "{msg}");
            // The description states the scope the catalog assigns.
            let spec = STATE_TOOLS.iter().find(|t| t.cap == cap).unwrap();
            assert!(
                spec.description.contains(&format!("`--scopes {scope}`")),
                "{cap}: {}",
                spec.description
            );
        }

        let r = StateRouter::new(vec!["sessions.resize", "worktrees.open"], |_, _| {
            Ok(json!(null))
        });
        let entries = r.tool_entries();
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["sessions_resize", "worktrees_open"]);
        // Field-for-field mirrors of ResizeBody (+ the `{s}` path param) and
        // OpenWorktreeBody.
        let resize = &entries[0]["inputSchema"];
        assert_eq!(resize["required"], json!(["session", "rows", "cols"]));
        assert_eq!(resize["properties"]["session"]["type"], "string");
        assert_eq!(resize["properties"]["rows"]["type"], "integer");
        assert_eq!(resize["properties"]["cols"]["type"], "integer");
        assert_eq!(resize["properties"].as_object().unwrap().len(), 3);
        let open = &entries[1]["inputSchema"];
        assert_eq!(open["required"], json!(["repo"]));
        assert_eq!(open["properties"]["repo"]["type"], "string");
        assert_eq!(open["properties"]["branch"]["type"], "string");
        assert_eq!(open["properties"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn sessions_resize_and_worktrees_open_reject_wrong_typed_args_before_fetch() {
        let r = StateRouter::new(
            vec!["sessions.resize", "worktrees.open"],
            |_, _| -> Result<Value, String> { panic!("invalid args must not reach fetch") },
        );
        for (tool, bad) in [
            (
                "sessions_resize",
                json!({ "session": "s1", "rows": "24", "cols": 80 }),
            ),
            (
                "sessions_resize",
                json!({ "session": "s1", "rows": 24, "cols": 1.5 }),
            ),
            (
                "sessions_resize",
                json!({ "session": 7, "rows": 24, "cols": 80 }),
            ),
            ("sessions_resize", json!({ "session": "s1", "rows": 24 })),
            ("worktrees_open", json!({})),
            ("worktrees_open", json!({ "repo": 1 })),
            ("worktrees_open", json!({ "repo": "/r", "branch": false })),
            ("worktrees_open", json!({ "repo": "/r", "create": true })),
        ] {
            let (code, _) = r.call(tool, &bad).unwrap().unwrap_err();
            assert_eq!(code, -32602, "{tool} {bad}");
        }
    }

    #[test]
    fn sessions_resize_and_worktrees_open_route_their_cap_ids_to_fetch() {
        let r = StateRouter::new(vec!["sessions.resize", "worktrees.open"], |cap, args| {
            Ok(json!({ "cap": cap, "args": args }))
        });
        let resized = r
            .call(
                "sessions_resize",
                &json!({ "session": "s1", "rows": 40, "cols": 120 }),
            )
            .unwrap()
            .unwrap();
        let text = resized["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("sessions.resize"), "{text}");
        assert!(text.contains("120"), "{text}");
        let opened = r
            .call(
                "worktrees_open",
                &json!({ "repo": "/repo", "branch": "main" }),
            )
            .unwrap()
            .unwrap();
        let text = opened["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("worktrees.open"), "{text}");
        assert!(text.contains("/repo"), "{text}");
        // `branch` is optional.
        assert!(
            r.call("worktrees_open", &json!({ "repo": "/repo" }))
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn sessions_snapshot_and_split_are_advertised_only_when_allowed() {
        let r = StateRouter::new(vec!["sessions.list", "worktrees.list"], |_, _| {
            Ok(json!(null))
        });
        let names: Vec<String> = r
            .tool_entries()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(
            !names
                .iter()
                .any(|n| n == "sessions_snapshot" || n == "sessions_split")
        );
        let denied = StateRouter::new(vec!["sessions.list"], |_, _| -> Result<Value, String> {
            panic!("a denied tool must not reach fetch")
        });
        for (tool, args, cap) in [
            (
                "sessions_snapshot",
                json!({ "session": "s1" }),
                "sessions.snapshot",
            ),
            (
                "sessions_split",
                json!({ "session": "s1" }),
                "sessions.split",
            ),
        ] {
            let (code, msg) = denied.call(tool, &args).unwrap().unwrap_err();
            assert_eq!(code, -32001);
            let scope = scope_of(lookup(cap).unwrap()).as_str();
            assert!(msg.contains(&format!("--scopes {scope}")), "{msg}");
            let spec = STATE_TOOLS.iter().find(|t| t.cap == cap).unwrap();
            let stated = if scope == "read" {
                "Read-scoped".to_string()
            } else {
                format!("`--scopes {scope}`")
            };
            assert!(
                spec.description.contains(&stated),
                "{cap}: {}",
                spec.description
            );
        }

        let r = StateRouter::new(vec!["sessions.snapshot", "sessions.split"], |_, _| {
            Ok(json!(null))
        });
        let entries = r.tool_entries();
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["sessions_snapshot", "sessions_split"]);
        let snap = &entries[0]["inputSchema"];
        assert_eq!(snap["required"], json!(["session"]));
        assert_eq!(snap["properties"].as_object().unwrap().len(), 1);
        // Mirrors SplitBody (dir/argv/cwd/env/rows/cols) + the `{s}` path param.
        let split = &entries[1]["inputSchema"];
        assert_eq!(split["required"], json!(["session"]));
        assert_eq!(split["properties"]["session"]["type"], "string");
        assert_eq!(split["properties"]["dir"]["type"], "string");
        assert_eq!(split["properties"]["argv"]["type"], "array");
        assert_eq!(split["properties"]["cwd"]["type"], "string");
        assert_eq!(split["properties"]["env"]["type"], "object");
        assert_eq!(split["properties"]["rows"]["type"], "integer");
        assert_eq!(split["properties"]["cols"]["type"], "integer");
        assert_eq!(split["properties"].as_object().unwrap().len(), 7);
    }

    #[test]
    fn sessions_snapshot_and_split_reject_wrong_typed_args_before_fetch() {
        let r = StateRouter::new(
            vec!["sessions.snapshot", "sessions.split"],
            |_, _| -> Result<Value, String> { panic!("invalid args must not reach fetch") },
        );
        for (tool, bad) in [
            ("sessions_snapshot", json!({})),
            ("sessions_snapshot", json!({ "session": 7 })),
            ("sessions_snapshot", json!({ "session": "s1", "extra": 1 })),
            ("sessions_split", json!({})),
            ("sessions_split", json!({ "session": 7 })),
            ("sessions_split", json!({ "session": "s1", "dir": 1 })),
            ("sessions_split", json!({ "session": "s1", "argv": "sh" })),
            ("sessions_split", json!({ "session": "s1", "argv": [1] })),
            ("sessions_split", json!({ "session": "s1", "cwd": false })),
            ("sessions_split", json!({ "session": "s1", "env": ["A"] })),
            ("sessions_split", json!({ "session": "s1", "rows": "24" })),
            ("sessions_split", json!({ "session": "s1", "cols": 1.5 })),
            ("sessions_split", json!({ "session": "s1", "bogus": true })),
        ] {
            let (code, _) = r.call(tool, &bad).unwrap().unwrap_err();
            assert_eq!(code, -32602, "{tool} {bad}");
        }
    }

    #[test]
    fn sessions_snapshot_and_split_route_their_cap_ids_to_fetch() {
        let r = StateRouter::new(vec!["sessions.snapshot", "sessions.split"], |cap, args| {
            Ok(json!({ "cap": cap, "args": args }))
        });
        let snap = r
            .call("sessions_snapshot", &json!({ "session": "s1" }))
            .unwrap()
            .unwrap();
        let text = snap["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("sessions.snapshot"), "{text}");
        let split = r
            .call(
                "sessions_split",
                &json!({ "session": "s1", "dir": "down", "argv": ["sh"] }),
            )
            .unwrap()
            .unwrap();
        let text = split["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("sessions.split"), "{text}");
        assert!(text.contains("down"), "{text}");
        // Only `session` is required.
        assert!(
            r.call("sessions_split", &json!({ "session": "s1" }))
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn redact_for_audit_hides_session_split_env_values() {
        let args = json!({ "session": "s1", "env": {"API_KEY": "sk-verysecret"} });
        let red = redact_for_audit("sessions.split", &args);
        let env = red["env"].as_str().unwrap();
        assert!(!env.contains("sk-verysecret"), "{env}");
        assert!(env.contains("1 vars"), "{env}");
    }

    #[test]
    fn dispatches_put_and_set_status_are_write_scoped_and_advertised_only_when_allowed() {
        let r = StateRouter::new(vec!["dispatches.list"], |_, _| Ok(json!(null)));
        let names: Vec<String> = r
            .tool_entries()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["dispatches_list"]);
        let denied = StateRouter::new(vec!["dispatches.list"], |_, _| -> Result<Value, String> {
            panic!("a denied tool must not reach fetch")
        });
        for (tool, args, cap) in [
            (
                "dispatches_put",
                json!({ "issue_id": "THE-1", "worktree_path": "/w", "agent_name": "a" }),
                "dispatches.put",
            ),
            (
                "dispatches_set_status",
                json!({ "id": 42, "status": "running" }),
                "dispatches.set_status",
            ),
        ] {
            let (code, msg) = denied.call(tool, &args).unwrap().unwrap_err();
            assert_eq!(code, -32001);
            assert_eq!(scope_of(lookup(cap).unwrap()), Scope::Write, "{cap}");
            assert!(msg.contains("--scopes write"), "{msg}");
            let spec = STATE_TOOLS.iter().find(|t| t.cap == cap).unwrap();
            assert!(
                spec.description.contains("`--scopes write`"),
                "{cap}: {}",
                spec.description
            );
        }

        let r = StateRouter::new(vec!["dispatches.put", "dispatches.set_status"], |_, _| {
            Ok(json!(null))
        });
        let entries = r.tool_entries();
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["dispatches_put", "dispatches_set_status"]);
        // Mirrors DispatchPutReq.
        let put = &entries[0]["inputSchema"];
        assert_eq!(
            put["required"],
            json!(["issue_id", "worktree_path", "agent_name"])
        );
        for (key, ty) in [
            ("issue_id", "string"),
            ("worktree_path", "string"),
            ("agent_name", "string"),
            ("stage", "string"),
            ("parent_id", "integer"),
            ("session_id", "string"),
            ("artifact_path", "string"),
            ("lead", "object"),
        ] {
            assert_eq!(put["properties"][key]["type"], ty, "{key}");
        }
        assert_eq!(put["properties"].as_object().unwrap().len(), 8);
        // Mirrors DispatchStatusBody + the `{id}` path param.
        let set = &entries[1]["inputSchema"];
        assert_eq!(set["required"], json!(["id", "status"]));
        assert_eq!(set["properties"]["id"]["type"], "integer");
        assert_eq!(set["properties"]["status"]["type"], "string");
        assert_eq!(set["properties"]["lead"]["type"], "object");
        assert_eq!(set["properties"].as_object().unwrap().len(), 3);
    }

    #[test]
    fn dispatches_put_and_set_status_reject_wrong_typed_args_before_fetch() {
        let r = StateRouter::new(
            vec!["dispatches.put", "dispatches.set_status"],
            |_, _| -> Result<Value, String> { panic!("invalid args must not reach fetch") },
        );
        let put_ok = json!({ "issue_id": "THE-1", "worktree_path": "/w", "agent_name": "a" });
        let mut put_extra = put_ok.clone();
        put_extra["bogus"] = json!(true);
        let mut put_lead = put_ok.clone();
        put_lead["lead"] = json!("lease");
        let mut put_parent = put_ok.clone();
        put_parent["parent_id"] = json!("7");
        for (tool, bad) in [
            ("dispatches_put", json!({})),
            (
                "dispatches_put",
                json!({ "issue_id": "THE-1", "worktree_path": "/w" }),
            ),
            (
                "dispatches_put",
                json!({ "issue_id": 1, "worktree_path": "/w", "agent_name": "a" }),
            ),
            ("dispatches_put", put_extra),
            ("dispatches_put", put_lead),
            ("dispatches_put", put_parent),
            ("dispatches_set_status", json!({})),
            ("dispatches_set_status", json!({ "id": 42 })),
            ("dispatches_set_status", json!({ "status": "done" })),
            (
                "dispatches_set_status",
                json!({ "id": "x", "status": "done" }),
            ),
            (
                "dispatches_set_status",
                json!({ "id": 1.5, "status": "done" }),
            ),
            ("dispatches_set_status", json!({ "id": 42, "status": 3 })),
            (
                "dispatches_set_status",
                json!({ "id": 42, "status": "done", "lead": [] }),
            ),
            (
                "dispatches_set_status",
                json!({ "id": 42, "status": "done", "note": "x" }),
            ),
        ] {
            let (code, _) = r.call(tool, &bad).unwrap().unwrap_err();
            assert_eq!(code, -32602, "{tool} {bad}");
        }
    }

    #[test]
    fn dispatches_put_and_set_status_route_their_cap_ids_to_fetch() {
        let r = StateRouter::new(
            vec!["dispatches.put", "dispatches.set_status"],
            |cap, args| Ok(json!({ "cap": cap, "args": args })),
        );
        let put = r
            .call(
                "dispatches_put",
                &json!({ "issue_id": "THE-1", "worktree_path": "/w", "agent_name": "a" }),
            )
            .unwrap()
            .unwrap();
        let text = put["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("dispatches.put"), "{text}");
        // An unknown status passes the router; the daemon owns the closed set.
        let set = r
            .call(
                "dispatches_set_status",
                &json!({ "id": 42, "status": "no_such_status" }),
            )
            .unwrap()
            .unwrap();
        let text = set["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("dispatches.set_status"), "{text}");
        assert!(text.contains("no_such_status"), "{text}");
    }

    #[test]
    fn redact_for_audit_keeps_dispatch_fields() {
        // Roster rows carry identifiers and paths, never free text or secrets.
        let args = json!({ "id": 42, "status": "done", "lead": { "lease": "l", "owner": "o" } });
        assert_eq!(redact_for_audit("dispatches.set_status", &args), args);
    }

    #[test]
    fn issues_update_and_comment_are_write_scoped_and_advertised_only_when_allowed() {
        let r = StateRouter::new(vec!["issues.list"], |_, _| Ok(json!(null)));
        let names: Vec<String> = r
            .tool_entries()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["issues_list"]);
        let denied = StateRouter::new(vec!["issues.list"], |_, _| -> Result<Value, String> {
            panic!("a denied tool must not reach fetch")
        });
        for (tool, args, cap) in [
            (
                "issues_update",
                json!({ "id": "linear:T-1", "status": "done" }),
                "issues.update",
            ),
            (
                "issues_comment",
                json!({ "id": "linear:T-1", "body": "hi" }),
                "issues.comment",
            ),
        ] {
            let (code, msg) = denied.call(tool, &args).unwrap().unwrap_err();
            assert_eq!(code, -32001);
            assert_eq!(scope_of(lookup(cap).unwrap()), Scope::Write, "{cap}");
            assert!(msg.contains("--scopes write"), "{msg}");
            let spec = STATE_TOOLS.iter().find(|t| t.cap == cap).unwrap();
            assert!(spec.description.contains("`--scopes write`"), "{cap}");
        }
        let r = StateRouter::new(vec!["issues.update", "issues.comment"], |_, _| {
            Ok(json!(null))
        });
        let entries = r.tool_entries();
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["issues_update", "issues_comment"]);
        let upd = &entries[0]["inputSchema"];
        assert_eq!(upd["required"], json!(["id"]));
        for (key, ty) in [
            ("id", "string"),
            ("status", "string"),
            ("assignee", "string"),
            ("priority", "integer"),
            ("title", "string"),
            ("repo", "string"),
            ("account", "string"),
        ] {
            assert_eq!(upd["properties"][key]["type"], ty, "{key}");
        }
        assert_eq!(upd["properties"].as_object().unwrap().len(), 7);
        let com = &entries[1]["inputSchema"];
        assert_eq!(com["required"], json!(["id", "body"]));
        assert_eq!(com["properties"].as_object().unwrap().len(), 4);
    }

    #[test]
    fn issues_update_and_comment_reject_bad_args_before_fetch() {
        let r = StateRouter::new(
            vec!["issues.update", "issues.comment"],
            |_, _| -> Result<Value, String> { panic!("invalid args must not reach fetch") },
        );
        let oversize = "x".repeat(MAX_ISSUE_COMMENT_BYTES + 1);
        for (tool, bad) in [
            ("issues_update", json!({})),
            ("issues_update", json!({ "status": "done" })),
            ("issues_update", json!({ "id": 7, "status": "done" })),
            ("issues_update", json!({ "id": "" , "status": "done" })),
            ("issues_update", json!({ "id": "linear:T-1" })),
            (
                "issues_update",
                json!({ "id": "linear:T-1", "account": "w" }),
            ),
            (
                "issues_update",
                json!({ "id": "linear:T-1", "priority": 5 }),
            ),
            (
                "issues_update",
                json!({ "id": "linear:T-1", "priority": -1 }),
            ),
            (
                "issues_update",
                json!({ "id": "linear:T-1", "priority": "2" }),
            ),
            (
                "issues_update",
                json!({ "id": "linear:T-1", "assignee": "bob" }),
            ),
            (
                "issues_update",
                json!({ "id": "linear:T-1", "title": "  " }),
            ),
            ("issues_update", json!({ "id": "linear:T-1", "bogus": 1 })),
            ("issues_comment", json!({})),
            ("issues_comment", json!({ "body": "hi" })),
            ("issues_comment", json!({ "id": "linear:T-1" })),
            ("issues_comment", json!({ "id": "linear:T-1", "body": "" })),
            (
                "issues_comment",
                json!({ "id": "linear:T-1", "body": "  \n" }),
            ),
            ("issues_comment", json!({ "id": "linear:T-1", "body": 3 })),
            (
                "issues_comment",
                json!({ "id": "linear:T-1", "body": oversize }),
            ),
        ] {
            let (code, _) = r.call(tool, &bad).unwrap().unwrap_err();
            assert_eq!(code, -32602, "{tool} {bad}");
        }
    }

    #[test]
    fn issues_update_and_comment_route_their_cap_ids_and_accept_the_limit() {
        let r = StateRouter::new(vec!["issues.update", "issues.comment"], |cap, _| {
            Ok(json!({ "cap": cap }))
        });
        for (tool, args, cap) in [
            (
                "issues_update",
                json!({ "id": "linear:T-1", "priority": 0, "assignee": "none" }),
                "issues.update",
            ),
            (
                "issues_comment",
                json!({ "id": "linear:T-1", "body": "x".repeat(MAX_ISSUE_COMMENT_BYTES) }),
                "issues.comment",
            ),
        ] {
            let res = r.call(tool, &args).unwrap().unwrap();
            let text = res["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(cap), "{text}");
        }
    }

    #[test]
    fn redact_for_audit_hides_the_comment_body_but_keeps_its_size() {
        let secret = "token sk-live-12345 in a comment";
        let args = json!({ "id": "linear:T-1", "body": secret, "account": "w" });
        let red = redact_for_audit("issues.comment", &args);
        assert_eq!(red["id"], "linear:T-1");
        assert_eq!(red["account"], "w");
        let body = red["body"].as_str().unwrap();
        assert!(!body.contains("sk-live"), "{body}");
        assert!(body.contains(&format!("{} bytes", secret.len())), "{body}");
        assert!(body.contains("sha256:"), "{body}");
        assert!(!red.to_string().contains(secret));
        // Deterministic, and distinct bodies hash differently.
        assert_eq!(body, text_digest(secret));
        assert_ne!(text_digest("a"), text_digest("b"));
        // An update carries no free-text body: unchanged.
        let upd = json!({ "id": "linear:T-1", "status": "done" });
        assert_eq!(redact_for_audit("issues.update", &upd), upd);
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
    fn sessions_input_advertises_keys_and_paste() {
        let r = StateRouter::new(vec!["sessions.input"], |_, _| Ok(json!(null)));
        let entry = &r.tool_entries()[0];
        let properties = entry["inputSchema"]["properties"].as_object().unwrap();
        assert_eq!(properties["keys"]["type"], "array");
        assert_eq!(properties["keys"]["items"]["type"], "string");
        assert_eq!(properties["paste"]["type"], "boolean");
        assert!(
            properties["keys"]["description"]
                .as_str()
                .unwrap()
                .contains("at most 64")
        );
    }

    #[test]
    fn every_state_cap_maps_to_the_scope_it_documents() {
        // The deliberate split this change introduces: listing/observing
        // tools stay Read (unchanged default `--scopes read` still covers
        // them); mutating tools require Write, while command execution
        // requires Exec. This replaces
        // `every_state_cap_is_read_scope_today`, whose own doc comment
        // predicted exactly this: "pins that a future write-side tool
        // forces a deliberate scope-model decision rather than silently
        // widening `read`."
        let read = [
            "sessions.list",
            "worktrees.list",
            "worktrees.sandbox_profile",
            "preview.fetch",
            "git.status",
            "merge.list",
            "leases.list",
            "me",
            "pr.status",
            "ci.runs",
            "ci.logs",
            "calendar.events",
            "calendar.clocks",
            "issues.list",
            "issues.get",
            "dispatches.list",
            "agent.sessions",
            "sessions.wait",
            "sessions.snapshot",
            "semantic.map",
            "semantic.blast_radius",
            "automations.list",
            "automations.test",
            "overview.get",
            "notifications.list",
            "automations.runs",
            "approvals.list",
            "producers.status",
        ];
        let write = [
            "editor.open",
            "notify.push",
            "sessions.open",
            "sessions.fork",
            "sessions.input",
            "sessions.kill",
            "sessions.resize",
            "sessions.split",
            "worktrees.open",
            "dispatches.put",
            "dispatches.set_status",
            "issues.update",
            "issues.comment",
            "notifications.ack",
            "automations.put",
            "automations.rm",
            "automations.enable",
            "approvals.request",
            "automations.pause",
        ];
        // Folder assignment is a repo-local worktree metadata mutation: Git.
        let git = ["folders.assign"];
        // Merge enqueue has its own single-verb scope (which `git` implies).
        let merge_add = ["merge.add", "merge.drain"];
        let exec = ["tools.run"];
        for cap in read {
            let c = lookup(cap).expect("state cap in catalog");
            assert_eq!(scope_of(c), Scope::Read, "{cap}");
        }
        for cap in write {
            let c = lookup(cap).expect("state cap in catalog");
            assert_eq!(scope_of(c), Scope::Write, "{cap}");
        }
        for cap in git {
            let c = lookup(cap).expect("state cap in catalog");
            assert_eq!(scope_of(c), Scope::Git, "{cap}");
        }
        for cap in exec {
            let c = lookup(cap).expect("state cap in catalog");
            assert_eq!(scope_of(c), Scope::Exec, "{cap}");
        }
        for cap in merge_add {
            let c = lookup(cap).expect("state cap in catalog");
            assert_eq!(scope_of(c), Scope::MergeAdd, "{cap}");
        }
        // Exhaustive: every implemented cap is in one of the two groups.
        let mut grouped: Vec<&str> = read
            .iter()
            .chain(write.iter())
            .chain(git.iter())
            .chain(merge_add.iter())
            .chain(exec.iter())
            .copied()
            .collect();
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
    fn redact_for_audit_counts_keys_never_names_them() {
        let args = json!({"session": "s1", "keys": ["ctrl+c", "a"]});
        let red = redact_for_audit("sessions.input", &args);
        assert_eq!(red["keys"], "<2 keys>");
        assert!(!red.to_string().contains("ctrl+c"));
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

    #[test]
    fn tool_entries_annotate_read_write_and_destructive_classes() {
        let r = StateRouter::new(all_caps(), |_, _| Ok(json!(null)));
        let entries = r.tool_entries();
        assert_eq!(entries.len(), MCP_STATE_CAPS.len());
        for entry in &entries {
            let name = entry["name"].as_str().unwrap();
            let cap = MCP_STATE_CAPS
                .iter()
                .find(|cap| tool_name(cap) == name)
                .unwrap();
            let read_only = scope_of(lookup(cap).unwrap()) == Scope::Read;
            assert_eq!(entry["annotations"]["readOnlyHint"], json!(read_only));
            assert_eq!(
                entry["annotations"]["destructiveHint"],
                json!(!read_only && !NON_DESTRUCTIVE_WRITE_CAPS.contains(cap))
            );
        }

        for (name, read_only, destructive) in [
            ("sessions_list", true, false),
            ("automations_test", true, false),
            ("sessions_kill", false, true),
            ("sessions_input", false, true),
            ("tools_run", false, true),
            ("folders_assign", false, false),
            ("merge_add", false, false),
            ("notify_push", false, false),
            ("editor_open", false, false),
            ("sessions_open", false, false),
            ("sessions_fork", false, false),
            ("sessions_resize", false, false),
            ("sessions_snapshot", true, false),
            ("sessions_split", false, false),
            ("worktrees_open", false, false),
            ("dispatches_put", false, false),
            ("dispatches_set_status", false, true),
            ("issues_update", false, true),
            ("issues_comment", false, false),
        ] {
            let entry = entries
                .iter()
                .find(|entry| entry["name"].as_str() == Some(name))
                .unwrap();
            assert_eq!(entry["annotations"]["readOnlyHint"], json!(read_only));
            assert_eq!(entry["annotations"]["destructiveHint"], json!(destructive));
        }
    }

    #[test]
    fn non_destructive_write_caps_are_real_non_read_mcp_caps() {
        for cap in NON_DESTRUCTIVE_WRITE_CAPS {
            assert!(MCP_STATE_CAPS.contains(cap), "{cap}");
            assert_ne!(scope_of(lookup(cap).unwrap()), Scope::Read, "{cap}");
        }
    }

    #[test]
    fn approvals_resolve_is_never_an_mcp_tool() {
        // Granting an approval is Admin (no supervisor path may grant), so it is
        // neither a state cap nor a declared tool, on any scope.
        assert!(!MCP_STATE_CAPS.contains(&"approvals.resolve"));
        assert!(STATE_TOOLS.iter().all(|t| t.cap != "approvals.resolve"));
        let r = StateRouter::new(all_caps(), |_, _| Ok(json!(null)));
        assert!(r.call("approvals_resolve", &json!({})).is_none());
        assert!(
            r.tool_entries()
                .iter()
                .all(|e| e["name"].as_str() != Some("approvals_resolve"))
        );
    }

    #[test]
    fn b155_spine_caps_are_never_mcp_tools() {
        // The phone's answer, the operator's halt and the profile env are
        // HTTP + CLI only, on any scope.
        let r = StateRouter::new(all_caps(), |_, _| Ok(json!(null)));
        let entries = r.tool_entries();
        for cap in [
            "approvals.redeem",
            "automations.halt",
            "mcp_proxy.profile_env",
        ] {
            assert!(!MCP_STATE_CAPS.contains(&cap), "{cap}");
            assert!(STATE_TOOLS.iter().all(|t| t.cap != cap), "{cap}");
            let tool = cap.replace('.', "_");
            assert!(r.call(&tool, &json!({})).is_none(), "{cap}");
            assert!(
                entries
                    .iter()
                    .all(|e| e["name"].as_str() != Some(tool.as_str())),
                "{cap}"
            );
        }
    }

    #[test]
    fn automations_pause_is_an_mcp_tool_and_resume_never_is() {
        // THE-2197: the Lead may pull the kill switch but never release it.
        assert!(MCP_STATE_CAPS.contains(&"automations.pause"));
        assert!(!MCP_STATE_CAPS.contains(&"automations.resume"));
        assert!(STATE_TOOLS.iter().all(|t| t.cap != "automations.resume"));
        let spec = STATE_TOOLS
            .iter()
            .find(|t| t.cap == "automations.pause")
            .expect("pause spec");
        assert!(spec.args.is_empty(), "pause takes no arguments");
        let r = StateRouter::new(all_caps(), |_, _| Ok(json!({"paused": true})));
        assert!(r.call("automations_resume", &json!({})).is_none());
        let entries = r.tool_entries();
        assert!(
            entries
                .iter()
                .all(|e| e["name"].as_str() != Some("automations_resume"))
        );
        let pause = entries
            .iter()
            .find(|e| e["name"].as_str() == Some("automations_pause"))
            .expect("automations_pause listed");
        assert_eq!(pause["annotations"]["readOnlyHint"], json!(false));
        assert_eq!(pause["annotations"]["destructiveHint"], json!(false));
        // A tool with no arguments lists none as required (the key may be absent).
        assert!(
            pause["inputSchema"]["required"]
                .as_array()
                .is_none_or(Vec::is_empty)
        );
        assert!(r.call("automations_pause", &json!({})).is_some());
    }

    #[test]
    fn lead_tools_declare_their_args_and_annotations() {
        let r = StateRouter::new(all_caps(), |_, _| Ok(json!(null)));
        let entries = r.tool_entries();
        let entry = |name: &str| {
            entries
                .iter()
                .find(|e| e["name"].as_str() == Some(name))
                .unwrap_or_else(|| panic!("{name} missing"))
                .clone()
        };
        for (name, read_only, destructive) in [
            ("overview_get", true, false),
            ("notifications_list", true, false),
            ("notifications_ack", false, false),
            ("automations_put", false, true),
            ("automations_rm", false, true),
            ("automations_enable", false, false),
            ("automations_runs", true, false),
            ("approvals_list", true, false),
            ("approvals_request", false, false),
            ("merge_drain", false, true),
            ("producers_status", true, false),
        ] {
            let e = entry(name);
            assert_eq!(e["annotations"]["readOnlyHint"], json!(read_only), "{name}");
            assert_eq!(
                e["annotations"]["destructiveHint"],
                json!(destructive),
                "{name}"
            );
            let description = e["description"].as_str().unwrap();
            assert!(
                description.contains("requires a running daemon"),
                "{name}: {description}"
            );
        }
        assert_eq!(
            entry("automations_put")["inputSchema"]["required"],
            json!(["rule"])
        );
        assert_eq!(
            entry("automations_enable")["inputSchema"]["required"],
            json!(["name", "enabled"])
        );
        assert_eq!(
            entry("approvals_request")["inputSchema"]["required"],
            json!(["issue_id", "stage", "commit_sha"])
        );
        assert_eq!(
            entry("merge_drain")["inputSchema"]["required"],
            json!(["worktree"])
        );
        assert_eq!(entry("overview_get")["inputSchema"]["required"], json!([]));
        // No declared args: the schema is the bare object form (no `required`).
        assert_eq!(
            entry("producers_status")["inputSchema"],
            json!({ "type": "object", "properties": {} })
        );
        // A missing required arg never reaches the fetch closure.
        let refused = r.call("merge_drain", &json!({})).unwrap().unwrap_err();
        assert_eq!(refused.0, -32602);
    }

    #[test]
    fn merge_drain_scope_hint_names_git_like_merge_add() {
        let r = StateRouter::new(vec![], |_, _| Ok(json!(null)));
        for name in ["merge_add", "merge_drain"] {
            let (code, msg) = r.call(name, &json!({})).unwrap().unwrap_err();
            assert_eq!(code, -32001);
            assert!(msg.contains("--scopes git"), "{name}: {msg}");
        }
    }

    #[test]
    fn with_caller_carries_the_audit_label() {
        let r = StateRouter::new(vec!["me"], |_, _| Ok(json!(null)));
        assert_eq!(r.caller(), "-");
        let r = r.with_caller(Some("lead:main".to_string()));
        assert_eq!(r.caller(), "lead:main");
        // The label never changes what is allowed.
        assert!(r.call("me", &json!({})).unwrap().is_ok());
        assert!(r.call("sessions_kill", &json!({})).unwrap().is_err());
        let r = r.with_caller(None);
        assert_eq!(r.caller(), "-");
    }

    #[test]
    fn unknown_capability_annotations_fail_closed() {
        assert_eq!(
            tool_annotations("nope.nope"),
            json!({ "readOnlyHint": false, "destructiveHint": true })
        );
    }
}
