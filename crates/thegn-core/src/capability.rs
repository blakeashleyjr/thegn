//! The host capability catalog: the ONE list every external surface projects.
//!
//! thegn is driven from outside through several doors — the control API
//! (HTTP/WS, gRPC), the CLI's control verbs, the MCP server, and plugin
//! `host.call`s. Each door used to keep its own verb list, and they drifted
//! (gRPC lagged HTTP; `Verb::ListWorktrees` existed with no route). The
//! catalog is the fix: one row per [`Verb`], naming the stable external id,
//! the surfaces the capability is (or must be) exposed on, and a one-line
//! summary. Per-surface coverage tests in `thegn-svc`/`thegn-host` assert
//! every surface either implements each row it is listed on or excuses it
//! in [`SURFACE_GAPS`] — which only shrinks.
//!
//! The catalog never restates *policy*: the scope a capability needs is
//! [`required_scope`]`(row.verb)`, so control tokens, MCP tool scopes and plugin
//! scope sets all answer to the same table.

use crate::control::{Scope, Verb, required_scope};

/// An external door through which a capability can be invoked.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    /// The control HTTP/WS/SSE API (`thegn-svc::control::http`).
    Http,
    /// The gRPC mirror (`thegn-svc::control::grpc`, feature `control-grpc`).
    Grpc,
    /// `thegn` CLI control verbs (`thegn session …`, `thegn open`, …).
    Cli,
    /// `thegn mcp serve` tools.
    Mcp,
    /// Plugin `host.call` verbs.
    Plugin,
}

impl Surface {
    pub const ALL: &'static [Surface] = &[
        Surface::Http,
        Surface::Grpc,
        Surface::Cli,
        Surface::Mcp,
        Surface::Plugin,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Http => "http",
            Surface::Grpc => "grpc",
            Surface::Cli => "cli",
            Surface::Mcp => "mcp",
            Surface::Plugin => "plugin",
        }
    }

    const fn bit(self) -> u8 {
        match self {
            Surface::Http => 1,
            Surface::Grpc => 2,
            Surface::Cli => 4,
            Surface::Mcp => 8,
            Surface::Plugin => 16,
        }
    }
}

/// A set of surfaces (const-constructible so the catalog is a `const`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SurfaceSet(u8);

impl SurfaceSet {
    pub const NONE: SurfaceSet = SurfaceSet(0);
    /// Every surface — the default for a plain read/write capability.
    pub const ALL: SurfaceSet = SurfaceSet(1 | 2 | 4 | 8 | 16);
    /// The operator surfaces: control API + CLI, never MCP or plugins.
    pub const OPERATOR: SurfaceSet = SurfaceSet(1 | 2 | 4);
    /// Streaming capabilities: request/response doors (MCP, plugin) can't carry them.
    pub const STREAMING: SurfaceSet = SurfaceSet(1 | 2 | 4);

    pub const fn of(surfaces: &[Surface]) -> SurfaceSet {
        let mut bits = 0u8;
        let mut i = 0;
        while i < surfaces.len() {
            bits |= surfaces[i].bit();
            i += 1;
        }
        SurfaceSet(bits)
    }

    pub const fn contains(self, s: Surface) -> bool {
        self.0 & s.bit() != 0
    }

    pub fn iter(self) -> impl Iterator<Item = Surface> {
        Surface::ALL
            .iter()
            .copied()
            .filter(move |s| self.contains(*s))
    }

    pub fn names(self) -> Vec<&'static str> {
        self.iter().map(Surface::as_str).collect()
    }
}

/// Stable external id: `<domain>.<action>`, snake_case, never renamed (a
/// rename is a deprecation + a new row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct CapId(pub &'static str);

impl CapId {
    pub fn as_str(self) -> &'static str {
        self.0
    }
    /// The MCP tool name projection (`sessions.list` → `sessions_list`).
    pub fn tool_name(self) -> String {
        self.0.replace('.', "_")
    }
}

impl std::fmt::Display for CapId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// One row of the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCapability {
    pub id: CapId,
    /// The scope-policy key: `required_scope(verb)` is what every door checks.
    pub verb: Verb,
    /// One line; becomes the MCP tool description and the docs table row.
    pub summary: &'static str,
    /// Where this capability is (must be) exposed.
    pub surfaces: SurfaceSet,
    /// Control-API version that introduced it (`"1"` = the v1 routes).
    pub since: &'static str,
    /// Set when the row is kept only for compatibility; names the replacement.
    pub deprecated: Option<&'static str>,
    /// Set when the row is *routed on every surface but answers
    /// `Unimplemented` unconditionally* — a reserved contract slot with no
    /// behavior yet (`browser.drive` today). Names what the row waits on. A
    /// stub is still projected by the surface tables (so it is "covered"), but
    /// the coverage report counts it apart from working capabilities so
    /// routed-but-inert never reads as done; removing the last `Unimplemented`
    /// answer MUST clear the marker.
    pub stub: Option<&'static str>,
}

const fn cap(
    id: &'static str,
    verb: Verb,
    surfaces: SurfaceSet,
    summary: &'static str,
) -> HostCapability {
    HostCapability {
        id: CapId(id),
        verb,
        summary,
        surfaces,
        since: "1",
        deprecated: None,
        stub: None,
    }
}

/// A [`cap`] that is a routed-but-inert stub (see [`HostCapability::stub`]).
const fn stub_cap(
    id: &'static str,
    verb: Verb,
    surfaces: SurfaceSet,
    summary: &'static str,
    waits_on: &'static str,
) -> HostCapability {
    HostCapability {
        stub: Some(waits_on),
        ..cap(id, verb, surfaces, summary)
    }
}

/// The catalog. One row per [`Verb`]; `every_verb_has_exactly_one_row` pins it.
pub const CATALOG: &[HostCapability] = &[
    // --- sessions -----------------------------------------------------------
    cap(
        "sessions.list",
        Verb::ListSessions,
        SurfaceSet::ALL,
        "List daemon sessions (worktree hints, geometry, lease state)",
    ),
    cap(
        "sessions.open",
        Verb::OpenSession,
        SurfaceSet::ALL,
        "Open a session: argv, cwd, env, rows/cols, optional worktree",
    ),
    cap(
        "sessions.attach",
        Verb::Attach,
        SurfaceSet::STREAMING,
        "Attach to a session's live output stream (snapshot + deltas)",
    ),
    cap(
        "sessions.detach",
        Verb::Detach,
        SurfaceSet::ALL,
        "Detach this client from a session",
    ),
    cap(
        "sessions.input",
        Verb::SendInput,
        SurfaceSet::ALL,
        "Send terminal input bytes to a session",
    ),
    cap(
        "sessions.resize",
        Verb::Resize,
        SurfaceSet::ALL,
        "Resize a session's PTY",
    ),
    cap(
        "sessions.snapshot",
        Verb::Snapshot,
        SurfaceSet::ALL,
        "Current emulator screen of a session",
    ),
    cap(
        "sessions.kill",
        Verb::KillSession,
        SurfaceSet::ALL,
        "Kill a session's process",
    ),
    cap(
        "sessions.wait",
        Verb::Wait,
        SurfaceSet::ALL,
        "Block until a session reaches a state (idle, exit, pattern)",
    ),
    cap(
        "sessions.split",
        Verb::Split,
        SurfaceSet::ALL,
        "Create a sibling pane/session next to an existing one",
    ),
    cap(
        "sessions.record",
        Verb::RecordSession,
        // Operator surfaces only (HTTP/gRPC/CLI): recording another session's
        // output is surveillance-adjacent, so it is deliberately kept off MCP
        // and plugin `host.call` in v1.
        SurfaceSet::OPERATOR,
        "Start/stop/query an asciicast recording of a session's output",
    ),
    // --- worktrees / browser --------------------------------------------------
    cap(
        "worktrees.list",
        Verb::ListWorktrees,
        SurfaceSet::ALL,
        "Worktrees registered with thegn (path, branch, repo root, location)",
    ),
    cap(
        "worktrees.open",
        Verb::OpenWorktree,
        SurfaceSet::ALL,
        "Open/focus a worktree in the owning instance",
    ),
    cap(
        "launch.preset",
        Verb::LaunchPreset,
        SurfaceSet::ALL,
        "Launch a configured preset into a workspace (name only; argv/env resolve locally)",
    ),
    stub_cap(
        "browser.drive",
        Verb::DriveBrowser,
        SurfaceSet::ALL,
        "Drive the preview browser (navigate, reload)",
        "no preview browser to drive yet (answers 501 on every surface)",
    ),
    // --- git / merge queue ----------------------------------------------------
    cap(
        "git.status",
        Verb::GitStatus,
        SurfaceSet::ALL,
        "Changed files in a worktree",
    ),
    cap(
        "git.stage",
        Verb::GitStage,
        SurfaceSet::ALL,
        "Stage paths in a worktree",
    ),
    cap(
        "git.commit",
        Verb::GitCommit,
        SurfaceSet::ALL,
        "Commit staged changes in a worktree",
    ),
    cap(
        "merge.list",
        Verb::MergeList,
        SurfaceSet::ALL,
        "The merge queue for a worktree's repo",
    ),
    cap(
        "merge.add",
        Verb::MergeAdd,
        SurfaceSet::ALL,
        "Enqueue a worktree's branch on the merge queue",
    ),
    cap(
        "merge.clear",
        Verb::MergeClear,
        SurfaceSet::ALL,
        "Clear the merge queue for a worktree's repo",
    ),
    cap(
        "pr.status",
        Verb::PrStatus,
        SurfaceSet::ALL,
        "Cached PR status for a worktree",
    ),
    // --- calendar / notifications -------------------------------------------
    cap(
        "calendar.events",
        Verb::CalendarEvents,
        SurfaceSet::ALL,
        "Merged calendar events over a date window",
    ),
    cap(
        "calendar.clocks",
        Verb::CalendarClocks,
        SurfaceSet::ALL,
        "Resolved world clocks",
    ),
    cap(
        "calendar.ingest",
        Verb::CalendarIngest,
        SurfaceSet::ALL,
        "Push events into a calendar source's own cache",
    ),
    cap(
        "notify.push",
        Verb::NotifyPush,
        SurfaceSet::ALL,
        "Push a notification into the tray",
    ),
    // --- agents --------------------------------------------------------------
    cap(
        "agent.sessions",
        Verb::AgentSessions,
        SurfaceSet::ALL,
        "List discovered coding-agent sessions (harness, id, worktree, mtime, summary)",
    ),
    cap(
        "agent.list",
        Verb::AgentList,
        SurfaceSet::of(&[Surface::Cli]),
        "Effective harness/model/env/permissions of every agent entry and pipeline stage",
    ),
    // --- feed / leases / identity -------------------------------------------
    cap(
        "events.subscribe",
        Verb::Events,
        SurfaceSet::of(&[Surface::Http, Surface::Grpc, Surface::Plugin]),
        "The live event feed",
    ),
    cap(
        "leases.list",
        Verb::LeaseStatus,
        SurfaceSet::ALL,
        "Relay lease state per session",
    ),
    cap(
        "me",
        Verb::Me,
        SurfaceSet::ALL,
        "The caller's pairing id, label and scopes",
    ),
    // --- mcp proxy hub -------------------------------------------------------
    cap(
        "mcp_proxy.status",
        Verb::McpProxyStatus,
        SurfaceSet::of(&[Surface::Http, Surface::Grpc, Surface::Cli]),
        "mcp-proxy hub state: per-upstream instances, breaker + health, exposed tool counts, withheld reasons",
    ),
    cap(
        "mcp_proxy.reload",
        Verb::McpProxyReload,
        SurfaceSet::of(&[Surface::Http, Surface::Grpc, Surface::Cli]),
        "Re-read config and reconcile the mcp-proxy hub's upstreams (start/stop/restart/refilter)",
    ),
    // --- hosts ---------------------------------------------------------------
    cap(
        "host.discover",
        Verb::HostDiscover,
        SurfaceSet::of(&[Surface::Cli]),
        "Discover remote-host candidates from the tailnet (`tailscale status`)",
    ),
    // --- semantic map / graph -----------------------------------------------
    // Read-only structural summaries of source the caller can already open.
    // Claim exactly the surfaces implemented (no SURFACE_GAPS excuses): the map
    // is CLI (`thegn map`) + MCP; the blast-radius is MCP-only (its only client
    // is a review agent — there is no `thegn blast-radius` verb).
    cap(
        "semantic.map",
        Verb::SemanticMap,
        SurfaceSet::of(&[Surface::Cli, Surface::Mcp]),
        "Ranked, budgeted repo map of a worktree's indexed entities",
    ),
    cap(
        "semantic.blast_radius",
        Verb::SemanticBlastRadius,
        SurfaceSet::of(&[Surface::Mcp]),
        "Blast-radius of a worktree's changes: callers, untested set, risk band",
    ),
    // --- admin ---------------------------------------------------------------
    // Pairing management and shutdown are deliberately HTTP + CLI only: gRPC is
    // for external tooling, and minting/revoking credentials or stopping the
    // daemon are operator actions that never need a third door. This is policy,
    // expressed by the narrowed surface set — NOT a `SURFACE_GAPS` excuse (the
    // gap table holds only temporary debt). MCP/plugin are excluded because
    // these are admin-scoped (the `admin_caps_never_reach_mcp_or_plugin` test
    // pins that).
    cap(
        "pairings.issue",
        Verb::IssuePairing,
        SurfaceSet::of(&[Surface::Http, Surface::Cli]),
        "Mint a single-use pairing code",
    ),
    cap(
        "pairings.list",
        Verb::ListPairings,
        SurfaceSet::of(&[Surface::Http, Surface::Cli]),
        "List pairings",
    ),
    cap(
        "pairings.revoke",
        Verb::RevokePairing,
        SurfaceSet::of(&[Surface::Http, Surface::Cli]),
        "Revoke a pairing",
    ),
    cap(
        "pairings.approve",
        Verb::ApprovePairing,
        SurfaceSet::of(&[Surface::Http, Surface::Cli]),
        "Approve a parked pairing",
    ),
    cap(
        "daemon.shutdown",
        Verb::Shutdown,
        SurfaceSet::of(&[Surface::Http, Surface::Cli]),
        "Shut the daemon down",
    ),
    // --- diagnostics ---------------------------------------------------------
    cap(
        "doctor.bundle",
        Verb::DoctorBundle,
        SurfaceSet::OPERATOR,
        "Write a redacted debug support bundle (doctor JSON, config, log tails, crash reports)",
    ),
    // --- secrets (credential broker, THE-66) ---------------------------------
    // OPERATOR surfaces only (CLI + control API — never MCP/plugins): a
    // tool-calling agent must not enumerate or rewrite secret custody. There is
    // deliberately NO secret.get row — the broker resolves for components, not
    // callers. Admin-scoped via `required_scope`.
    cap(
        "secret.set",
        Verb::SecretSet,
        SurfaceSet::OPERATOR,
        "Store a secret in the broker (keyring/file), returning a ref for config",
    ),
    cap(
        "secret.rm",
        Verb::SecretRm,
        SurfaceSet::OPERATOR,
        "Remove a stored secret",
    ),
    cap(
        "secret.list",
        Verb::SecretList,
        SurfaceSet::OPERATOR,
        "List configured secret refs and backends (names only, never values)",
    ),
    cap(
        "secret.migrate",
        Verb::SecretMigrate,
        SurfaceSet::OPERATOR,
        "Move plaintext literal secrets from config into the store",
    ),
    cap(
        "secret.audit",
        Verb::SecretAudit,
        SurfaceSet::OPERATOR,
        "Summarize configured refs with backend and last resolution outcome",
    ),
    cap(
        "secret.ssh.rotate",
        Verb::SecretSshRotate,
        SurfaceSet::OPERATOR,
        "Rotate a managed SSH key across its scope's live instances",
    ),
    // --- projects (multi-repo workspace groups, THE-33) ----------------------
    // OPERATOR surfaces (control API + CLI). Implemented locally as `thegn
    // project …` / `thegn wt new --project …` subcommands (they touch the local
    // per-profile DB + git, not the daemon), so the CLI surface covers them
    // directly; the HTTP/gRPC routes are deferred (excused in SURFACE_GAPS).
    // MCP/plugin exposure waits on the in-flight write-tool scope-gating work —
    // the CATALOG rows below do not depend on it. Grouping only: no policy, so
    // no secret/egress custody rides on these.
    cap(
        "project.list",
        Verb::ProjectList,
        SurfaceSet::OPERATOR,
        "List projects (multi-repo workspace groups) with member counts",
    ),
    cap(
        "project.create",
        Verb::ProjectCreate,
        SurfaceSet::OPERATOR,
        "Create a project",
    ),
    cap(
        "project.rename",
        Verb::ProjectRename,
        SurfaceSet::OPERATOR,
        "Rename a project",
    ),
    cap(
        "project.rm",
        Verb::ProjectRemove,
        SurfaceSet::OPERATOR,
        "Delete a project (refused while it has members unless forced)",
    ),
    cap(
        "project.assign",
        Verb::ProjectAssign,
        SurfaceSet::OPERATOR,
        "Assign or unassign a workspace's project membership",
    ),
    cap(
        "project.new_feature",
        Verb::ProjectNewFeature,
        SurfaceSet::OPERATOR,
        "Create a feature across a project's repos: one linked branch + a worktree in each member",
    ),
    // --- agent orchestration (THE-57) ---------------------------------------
    // The hands a supervisor agent drives: read the board and the durable
    // roster, write issue transitions/comments, record and re-status
    // dispatches, and spin up a worktree (optionally from an issue). Every row
    // works with no agent configured — they are plain tracker/git/roster ops.
    cap(
        "issues.list",
        Verb::IssuesList,
        SurfaceSet::ALL,
        "List tracker issues (filter by status/limit) from the configured provider",
    ),
    cap(
        "issues.get",
        Verb::IssuesGet,
        SurfaceSet::ALL,
        "Read one tracker issue with its detail and comments",
    ),
    cap(
        "issues.update",
        Verb::IssuesUpdate,
        SurfaceSet::ALL,
        "Patch a tracker issue (status/assignee/priority/title)",
    ),
    cap(
        "issues.comment",
        Verb::IssuesComment,
        SurfaceSet::ALL,
        "Post a comment on a tracker issue",
    ),
    cap(
        "dispatches.list",
        Verb::DispatchesList,
        SurfaceSet::ALL,
        "List the agent-dispatch roster (issue, worktree, agent, status)",
    ),
    cap(
        "dispatches.put",
        Verb::DispatchesPut,
        SurfaceSet::ALL,
        "Record a new dispatch on the roster",
    ),
    cap(
        "dispatches.set_status",
        Verb::DispatchesSetStatus,
        SurfaceSet::ALL,
        "Advance a dispatch's status on the roster",
    ),
    cap(
        "worktrees.create",
        Verb::WorktreeCreate,
        SurfaceSet::ALL,
        "Create a worktree, optionally from an issue (branch from its hint, link it)",
    ),
    // --- workspace search & replace (THE-5) ---------------------------------
    // Driven by the local `thegn search` CLI verb (in-process against the
    // worktree filesystem, like `thegn open`/`wt list`), not the control API —
    // hence CLI-only and excused in SURFACE_GAPS for the control-client
    // coverage test. Read/write scope is enforced via `required_scope`.
    cap(
        "search.query",
        Verb::SearchQuery,
        SurfaceSet::of(&[Surface::Cli]),
        "Run a workspace text/structural search (JSON output)",
    ),
    cap(
        "search.replace",
        Verb::SearchReplace,
        SurfaceSet::of(&[Surface::Cli]),
        "Apply a workspace search-and-replace through the guarded write path",
    ),
    // --- containers ----------------------------------------------------------
    // First-party container management: read = list, write = lifecycle on OWNED
    // containers, admin = estate cleanup. Ownership is enforced structurally in
    // `sandbox_manage` regardless of surface; the scope is the only per-door
    // policy, via `required_scope`. `containers.prune` is `OPERATOR`, not `ALL`:
    // it is admin-scoped and `admin_caps_never_reach_mcp_or_plugin` forbids an
    // admin capability on the untrusted MCP/plugin doors — the same shape as
    // `daemon.shutdown`.
    cap(
        "containers.list",
        Verb::ContainersList,
        SurfaceSet::ALL,
        "List thegn's containers across backends (owned first; foreign read-only)",
    ),
    cap(
        "containers.control",
        Verb::ContainersControl,
        SurfaceSet::ALL,
        "Lifecycle on an owned container: stop/start/restart/logs",
    ),
    cap(
        "containers.prune",
        Verb::ContainersPrune,
        SurfaceSet::OPERATOR,
        "Clean up thegn-owned containers/images/volumes (gc + prune)",
    ),
    // Model proxy (THE-58) — OPERATOR-surface only (control HTTP/gRPC + CLI),
    // never MCP or plugins. status/stats are read; start/stop are admin.
    cap(
        "model_proxy.status",
        Verb::ModelProxyStatus,
        SurfaceSet::OPERATOR,
        "Report the model proxy: enabled, listen, reachability, providers",
    ),
    cap(
        "model_proxy.stats",
        Verb::ModelProxyStats,
        SurfaceSet::OPERATOR,
        "Read the model proxy's spend/token/latency stats rollup",
    ),
    cap(
        "model_proxy.start",
        Verb::ModelProxyStart,
        SurfaceSet::OPERATOR,
        "Launch the model proxy daemon",
    ),
    cap(
        "model_proxy.stop",
        Verb::ModelProxyStop,
        SurfaceSet::OPERATOR,
        "Stop the model proxy daemon",
    ),
];

/// Documented, shrink-only gaps: `(capability id, surface, why)`. A surface's
/// coverage test fails when a catalog row for that surface is neither
/// implemented nor listed here — and when an entry here names something the
/// surface now implements (stale excuses are removed, never accumulated).
///
/// This table holds ONLY temporary debt. A surface a capability is deliberately
/// never exposed on (pairing management + shutdown are HTTP + CLI only) is
/// expressed by narrowing the row's `surfaces` set, not by an excuse here.
///
/// The table is pinned shrink-only by `test/surface-gaps-ratchet.txt`
/// (`ratchet_pins_surface_gaps`): adding an excuse fails the build until the
/// file grows a line with a written reason; removing one requires deleting its
/// line. Each ratchet line carries the reason as a third TAB-separated column,
/// regenerated from this table by `just ratchet-update` so the two never drift.
/// When the table reaches empty, the pinning test asserts it stays empty.
///
/// The remaining debt, by surface: the **plugin** surface has none (`host.call`
/// dispatches every non-streaming row generically); **MCP** owes its state
/// tools; **gRPC** owes the mirror for the rows HTTP already serves; and
/// **HTTP/CLI** owe control routes for the CLI-first operator families
/// (`secret.*`, `project.*`, `containers.*`, `doctor.bundle`, `launch.preset`)
/// — those run against local custody today, so the CLI implements them directly.
pub const SURFACE_GAPS: &[(&str, Surface, &str)] = &[
    // -- gRPC: the mirror lags HTTP; each is a proto + handler addition -------
    (
        "launch.preset",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "mcp_proxy.status",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "mcp_proxy.reload",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "launch.preset",
        Surface::Http,
        "CLI-first (`open --preset` via the intents mailbox); an HTTP route is a follow-up",
    ),
    // The debug bundle is a local operator operation (`thegn doctor bundle`): it
    // reads local log files + crash reports and writes an archive. The CLI verb
    // exists; the control-plane routes are not wired (a remote client would want
    // its own local bundle, not the daemon's), so both are excused here.
    (
        "doctor.bundle",
        Surface::Http,
        "local CLI operator verb; no control route (bundle reads local files)",
    ),
    (
        "doctor.bundle",
        Surface::Grpc,
        "local CLI operator verb; no control route (bundle reads local files)",
    ),
    // -- secrets: CLI-implemented locally; control-API routes deferred ---------
    // The `secret.*` verbs run against local custody (keyring / config file), so
    // the CLI implements them directly; an HTTP/gRPC route for a remote operator
    // is future work. They are NEVER on MCP/plugins (OPERATOR surface set).
    (
        "secret.set",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "secret.set",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "secret.rm",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "secret.rm",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "secret.list",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "secret.list",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "secret.migrate",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "secret.migrate",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "secret.audit",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "secret.audit",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "secret.ssh.rotate",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "secret.ssh.rotate",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    // -- projects (THE-33): CLI-implemented locally; control-API routes deferred.
    // The verbs run against the local per-profile DB + git, so the CLI covers
    // them directly; HTTP/gRPC routes for a remote operator are future work.
    (
        "project.list",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "project.list",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "project.create",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "project.create",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "project.rename",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "project.rename",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "project.rm",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "project.rm",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "project.assign",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "project.assign",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "project.new_feature",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "project.new_feature",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    // -- CLI: local worktree-fs verbs, not driven through the control API ------
    // (`thegn search` runs in-process against the worktree, like `thegn open`;
    // `cli_control_caps` only measures control-client-driven caps).
    (
        "search.query",
        Surface::Cli,
        "local worktree-fs verb; runs in-process, not through the control API",
    ),
    (
        "search.replace",
        Surface::Cli,
        "local worktree-fs verb; runs in-process, not through the control API",
    ),
    // -- MCP: state tools not yet landed (the MCP write-tools branch retires
    //    these). The plugin surface has NO excuses left: `host.call` dispatches
    //    every non-streaming row generically off the `API_CALLS` spine.
    (
        "sessions.detach",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "sessions.resize",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "sessions.snapshot",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "sessions.split",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "worktrees.open",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "launch.preset",
        Surface::Mcp,
        "MCP exec-scoped tools land in the MCP write-tools phase",
    ),
    (
        "browser.drive",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "git.status",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "git.stage",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "git.commit",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "merge.list",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "merge.add",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "merge.clear",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "pr.status",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "calendar.events",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "calendar.clocks",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "calendar.ingest",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "notify.push",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    // --- agent orchestration (THE-57): HTTP + CLI today ----------------------
    // The eight orchestration rows are served over the control HTTP surface
    // (and therefore the CLI's generic client) plus dedicated `thegn` verbs.
    // gRPC mirroring and MCP state tools follow the same phased path as every
    // other state cap above — recorded, not built, here. Plugin dispatch is
    // generic and needs no excuse.
    (
        "issues.list",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "issues.get",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "issues.update",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "issues.comment",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "dispatches.list",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "dispatches.put",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "dispatches.set_status",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "worktrees.create",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "issues.list",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "issues.get",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "issues.update",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "issues.comment",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "dispatches.list",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "dispatches.put",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "dispatches.set_status",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    (
        "worktrees.create",
        Surface::Mcp,
        "MCP state tools land in the client-API phase",
    ),
    // -- containers: the TUI Containers tab + `thegn sandbox gc/prune` are the
    //    surfaces this change ships; the external control/MCP doors land with
    //    the client-API / MCP scope-gating phase. `containers.prune` (admin,
    //    OPERATOR-only) IS wired on the CLI (`thegn sandbox gc/prune`), and the
    //    plugin door is generic (no excuse).
    (
        "containers.list",
        Surface::Http,
        "container doors land in the client-API phase; TUI Containers tab ships now",
    ),
    (
        "containers.list",
        Surface::Grpc,
        "container doors land in the client-API phase; TUI Containers tab ships now",
    ),
    (
        "containers.list",
        Surface::Cli,
        "container doors land in the client-API phase; TUI Containers tab ships now",
    ),
    (
        "containers.list",
        Surface::Mcp,
        "container doors land in the MCP scope-gating phase",
    ),
    (
        "containers.control",
        Surface::Http,
        "container doors land in the client-API phase; TUI row actions ship now",
    ),
    (
        "containers.control",
        Surface::Grpc,
        "container doors land in the client-API phase; TUI row actions ship now",
    ),
    (
        "containers.control",
        Surface::Cli,
        "container doors land in the client-API phase; TUI row actions ship now",
    ),
    (
        "containers.control",
        Surface::Mcp,
        "container doors land in the MCP scope-gating phase",
    ),
    (
        "containers.prune",
        Surface::Http,
        "prune is `thegn sandbox gc/prune` (CLI) this change; a control route lands in the client-API phase",
    ),
    (
        "containers.prune",
        Surface::Grpc,
        "prune is `thegn sandbox gc/prune` (CLI) this change; a control route lands in the client-API phase",
    ),
    // -- gRPC verbs landed after the proto was last regenerated ---------------
    (
        "sessions.record",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "agent.sessions",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    // -- model proxy (THE-58): CLI-implemented; control-API routes deferred ----
    // `thegn proxy status|stats|start|stop` run against the local daemon; an
    // HTTP/gRPC route for a remote operator is future work. NEVER MCP/plugins.
    (
        "model_proxy.status",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "model_proxy.status",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "model_proxy.stats",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "model_proxy.stats",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "model_proxy.start",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "model_proxy.start",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "model_proxy.stop",
        Surface::Http,
        "control-API route deferred; CLI-only for now",
    ),
    (
        "model_proxy.stop",
        Surface::Grpc,
        "control-API route deferred; CLI-only for now",
    ),
];

/// Look a capability up by id.
pub fn lookup(id: &str) -> Option<&'static HostCapability> {
    CATALOG.iter().find(|c| c.id.0 == id)
}

/// The row for a verb (every verb has exactly one).
pub fn for_verb(verb: Verb) -> Option<&'static HostCapability> {
    CATALOG.iter().find(|c| c.verb == verb)
}

/// The scope a capability requires — always via the verb policy table.
pub fn scope_of(c: &HostCapability) -> Scope {
    required_scope(c.verb)
}

/// Rows a surface is expected to expose.
pub fn for_surface(s: Surface) -> impl Iterator<Item = &'static HostCapability> {
    CATALOG.iter().filter(move |c| c.surfaces.contains(s))
}

/// Whether `(id, surface)` is an excused gap.
pub fn is_gap(id: &str, s: Surface) -> bool {
    SURFACE_GAPS.iter().any(|(g, gs, _)| *g == id && *gs == s)
}

/// Rows a surface must implement today (expected minus excused).
pub fn required_for(s: Surface) -> impl Iterator<Item = &'static HostCapability> {
    for_surface(s).filter(move |c| !is_gap(c.id.0, s))
}

/// The coverage check every surface test runs. `implemented` is the surface's
/// own table of capability ids. Returns the list of problems (empty = pass):
/// expected-but-missing rows, excused-but-implemented (stale gap) rows, and
/// implemented ids that are not in the catalog at all.
pub fn coverage_problems(s: Surface, implemented: &[&str]) -> Vec<String> {
    let mut problems = Vec::new();
    for c in for_surface(s) {
        let done = implemented.contains(&c.id.0);
        let excused = is_gap(c.id.0, s);
        match (done, excused) {
            (false, false) => problems.push(format!(
                "{}: `{}` is in the catalog for this surface but not implemented — implement it or add a SURFACE_GAPS entry",
                s.as_str(),
                c.id
            )),
            (true, true) => problems.push(format!(
                "{}: `{}` is implemented but still excused in SURFACE_GAPS — delete the stale entry",
                s.as_str(),
                c.id
            )),
            _ => {}
        }
    }
    for id in implemented {
        match lookup(id) {
            None => problems.push(format!(
                "{}: `{id}` is implemented but not in the catalog — add a row",
                s.as_str()
            )),
            Some(c) if !c.surfaces.contains(s) => problems.push(format!(
                "{}: `{id}` is implemented but the catalog does not list this surface for it",
                s.as_str()
            )),
            _ => {}
        }
    }
    problems
}

/// One surface's coverage ledger, computed by pure logic from the catalog and
/// the surface's own implementation table — what `thegn api coverage` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceLedger {
    pub surface: Surface,
    /// Working rows: implemented and not a routed stub.
    pub implemented: usize,
    /// Routed-but-inert rows (see [`HostCapability::stub`]).
    pub stub: usize,
    /// Rows excused in [`SURFACE_GAPS`] for this surface.
    pub excused: usize,
    /// Every catalog row listing this surface.
    pub declared: usize,
    /// The excused `(capability id, reason)` pairs, sorted.
    pub gaps: Vec<(&'static str, &'static str)>,
}

/// Compute a surface's [`SurfaceLedger`]. `implemented_ids` is the surface's own
/// table of implemented capability ids (routed stubs included — they are still
/// wired, just inert). Stubs are counted apart from working capabilities so a
/// routed stub never reads as done.
pub fn ledger(surface: Surface, implemented_ids: &[&str]) -> SurfaceLedger {
    let mut implemented = 0;
    let mut stub = 0;
    for c in for_surface(surface) {
        if implemented_ids.contains(&c.id.0) {
            if c.stub.is_some() {
                stub += 1;
            } else {
                implemented += 1;
            }
        }
    }
    let mut gaps: Vec<(&'static str, &'static str)> = SURFACE_GAPS
        .iter()
        .filter(|(_, s, _)| *s == surface)
        .map(|(id, _, why)| (*id, *why))
        .collect();
    gaps.sort_unstable();
    SurfaceLedger {
        surface,
        implemented,
        stub,
        excused: gaps.len(),
        declared: for_surface(surface).count(),
        gaps,
    }
}

/// Every stub row in the catalog (routed-but-inert; see
/// [`HostCapability::stub`]).
pub fn stubs() -> impl Iterator<Item = &'static HostCapability> {
    CATALOG.iter().filter(|c| c.stub.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};

    #[test]
    fn every_verb_has_exactly_one_row() {
        for v in Verb::ALL {
            let n = CATALOG.iter().filter(|c| c.verb == *v).count();
            assert_eq!(n, 1, "{v:?} has {n} catalog rows");
            assert_eq!(for_verb(*v).unwrap().verb, *v);
        }
        assert_eq!(CATALOG.len(), Verb::ALL.len());
    }

    #[test]
    fn ids_are_unique_and_snake_dotted() {
        let mut seen = HashSet::new();
        for c in CATALOG {
            assert!(seen.insert(c.id.0), "duplicate id {}", c.id);
            let ok =
                c.id.0
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch == '.');
            assert!(ok, "{} is not snake.dotted", c.id);
            assert!(!c.id.0.starts_with('.') && !c.id.0.ends_with('.'));
            assert!(!c.summary.is_empty());
            assert!(lookup(c.id.0).is_some());
        }
        assert!(lookup("nope.nope").is_none());
        assert_eq!(CapId("sessions.list").tool_name(), "sessions_list");
    }

    #[test]
    fn admin_caps_never_reach_mcp_or_plugin() {
        for c in CATALOG {
            if scope_of(c) == Scope::Admin {
                assert!(
                    !c.surfaces.contains(Surface::Mcp) && !c.surfaces.contains(Surface::Plugin),
                    "{} is admin-scoped but exposed to MCP/plugins",
                    c.id
                );
            }
        }
    }

    #[test]
    fn streaming_caps_are_not_on_request_response_doors() {
        let attach = lookup("sessions.attach").unwrap();
        assert!(!attach.surfaces.contains(Surface::Mcp));
        assert!(!attach.surfaces.contains(Surface::Plugin));
    }

    #[test]
    fn gaps_are_real_and_unique() {
        let mut seen = BTreeSet::new();
        for (id, s, why) in SURFACE_GAPS {
            let c = lookup(id).unwrap_or_else(|| panic!("gap names unknown cap {id}"));
            assert!(
                c.surfaces.contains(*s),
                "gap {id}/{} excuses a surface the cap does not list",
                s.as_str()
            );
            assert!(!why.is_empty());
            assert!(seen.insert((*id, *s)), "duplicate gap {id}/{}", s.as_str());
            assert!(is_gap(id, *s));
        }
        assert!(!is_gap("sessions.list", Surface::Http));
    }

    #[test]
    fn surface_set_ops() {
        let s = SurfaceSet::of(&[Surface::Http, Surface::Plugin]);
        assert!(s.contains(Surface::Http) && s.contains(Surface::Plugin));
        assert!(!s.contains(Surface::Cli));
        assert_eq!(s.names(), ["http", "plugin"]);
        assert_eq!(SurfaceSet::NONE.names(), Vec::<&str>::new());
        assert_eq!(SurfaceSet::ALL.iter().count(), Surface::ALL.len());
        assert_eq!(serde_json::to_string(&Surface::Grpc).unwrap(), "\"grpc\"");
        assert_eq!(format!("{}", CapId("a.b")), "a.b");
        assert_eq!(SurfaceSet::default(), SurfaceSet::NONE);
    }

    #[test]
    fn coverage_problems_reports_each_kind_of_drift() {
        // A complete, honest surface: every required row implemented.
        let http_done: Vec<&str> = required_for(Surface::Http).map(|c| c.id.0).collect();
        assert!(coverage_problems(Surface::Http, &http_done).is_empty());

        // Missing a required row.
        let mut missing = http_done.clone();
        missing.retain(|id| *id != "sessions.list");
        let p = coverage_problems(Surface::Http, &missing);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("sessions.list") && p[0].contains("not implemented"));

        // Implementing an excused row ⇒ stale gap (MCP still carries excuses).
        let mut stale: Vec<&str> = required_for(Surface::Mcp).map(|c| c.id.0).collect();
        stale.push("git.status"); // excused on MCP
        let p = coverage_problems(Surface::Mcp, &stale);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("stale"));

        // Implementing an unknown id / a surface the cap does not list.
        let mut unknown = http_done.clone();
        unknown.push("bogus.cap");
        let p = coverage_problems(Surface::Http, &unknown);
        assert!(p.iter().any(|m| m.contains("not in the catalog")), "{p:?}");
        let p = coverage_problems(Surface::Mcp, &["pairings.issue"]);
        assert!(
            p.iter().any(|m| m.contains("does not list this surface")),
            "{p:?}"
        );
    }

    #[test]
    fn required_for_excludes_gaps() {
        // The rows gRPC mirrors are required of it; the ones it still owes
        // (the CLI-first operator families) are excused, as MCP's state tools are.
        let grpc: Vec<&str> = required_for(Surface::Grpc).map(|c| c.id.0).collect();
        assert!(grpc.contains(&"sessions.list"));
        assert!(grpc.contains(&"merge.list"), "gRPC now mirrors merge.list");
        let mcp: Vec<&str> = required_for(Surface::Mcp).map(|c| c.id.0).collect();
        assert!(mcp.contains(&"sessions.list"));
        assert!(!mcp.contains(&"git.status"), "git.status is excused on MCP");
    }

    #[test]
    fn pairing_and_shutdown_are_http_cli_policy_not_excuses() {
        // The permanent policy lives in the surface set, never in SURFACE_GAPS.
        for id in [
            "pairings.issue",
            "pairings.list",
            "pairings.revoke",
            "pairings.approve",
            "daemon.shutdown",
        ] {
            let c = lookup(id).unwrap();
            assert!(c.surfaces.contains(Surface::Http), "{id} on http");
            assert!(c.surfaces.contains(Surface::Cli), "{id} on cli");
            assert!(!c.surfaces.contains(Surface::Grpc), "{id} off grpc");
            assert!(!c.surfaces.contains(Surface::Mcp), "{id} off mcp");
            assert!(!c.surfaces.contains(Surface::Plugin), "{id} off plugin");
            // …and none of these is a SURFACE_GAPS excuse on any surface.
            for s in Surface::ALL {
                assert!(!is_gap(id, *s), "{id}/{} must not be excused", s.as_str());
            }
        }
    }

    #[test]
    fn browser_drive_is_the_only_stub_and_is_not_deprecated() {
        let stubbed: Vec<&str> = stubs().map(|c| c.id.0).collect();
        assert_eq!(stubbed, ["browser.drive"]);
        // A stub is a live-but-inert slot, never a compatibility shim.
        for c in stubs() {
            assert!(
                c.deprecated.is_none(),
                "{} is both a stub and deprecated",
                c.id
            );
        }
    }

    #[test]
    fn ledger_counts_stub_apart_from_working() {
        // The HTTP surface routes browser.drive (a stub) plus real verbs.
        let http_impl = ["sessions.list", "browser.drive"];
        let l = ledger(Surface::Http, &http_impl);
        assert_eq!(l.implemented, 1, "sessions.list is working");
        assert_eq!(l.stub, 1, "browser.drive is a routed stub");
        // HTTP is no longer excuse-free: the CLI-first operator families
        // (`secret.*`, `project.*`, `containers.*`, `doctor.bundle`,
        // `launch.preset`) declare an HTTP surface whose route is still owed.
        // Derive the count from the table so this pins the ledger's arithmetic,
        // not a number that rots with every burn-down.
        let http_gaps = SURFACE_GAPS
            .iter()
            .filter(|(_, s, _)| *s == Surface::Http)
            .count();
        assert_eq!(l.excused, http_gaps, "the ledger lists every HTTP excuse");
        assert_eq!(l.gaps.len(), http_gaps);
        assert_eq!(l.declared, for_surface(Surface::Http).count());
        // MCP carries the remaining excuses; its ledger lists them.
        let mcp = ledger(Surface::Mcp, &["sessions.list"]);
        assert!(mcp.excused > 0);
        assert!(mcp.gaps.iter().any(|(id, _)| *id == "git.status"));
    }

    #[test]
    fn ratchet_pins_surface_gaps() {
        // Shrink-only allowlist: SURFACE_GAPS and the committed file are the
        // same set. Adding an excuse fails until the file grows a line; burning
        // one fails until the line is deleted. Empty ⇒ pinned empty.
        const RATCHET: &str = include_str!("../../../test/surface-gaps-ratchet.txt");
        let file_set: BTreeSet<(String, String)> = RATCHET
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| {
                // `<id>\t<surface>\t<reason>`; the reason column is prose for the
                // reader (regenerated from SURFACE_GAPS) and not part of the set.
                let mut cols = l.split('\t');
                let id = cols
                    .next()
                    .filter(|c| !c.trim().is_empty())
                    .unwrap_or_else(|| panic!("ratchet line missing id: {l:?}"));
                let surface = cols
                    .next()
                    .filter(|c| !c.trim().is_empty())
                    .unwrap_or_else(|| panic!("ratchet line missing TAB + surface: {l:?}"));
                let reason = cols.next().unwrap_or("").trim();
                assert!(
                    !reason.is_empty(),
                    "ratchet line needs a written reason in its third column: {l:?}"
                );
                (id.trim().to_string(), surface.trim().to_string())
            })
            .collect();
        let gaps_set: BTreeSet<(String, String)> = SURFACE_GAPS
            .iter()
            .map(|(id, s, _)| (id.to_string(), s.as_str().to_string()))
            .collect();
        let unratcheted: Vec<_> = gaps_set.difference(&file_set).collect();
        assert!(
            unratcheted.is_empty(),
            "SURFACE_GAPS entries not in test/surface-gaps-ratchet.txt \
             (add a line with a written reason): {unratcheted:?}"
        );
        let stale: Vec<_> = file_set.difference(&gaps_set).collect();
        assert!(
            stale.is_empty(),
            "test/surface-gaps-ratchet.txt lines with no matching SURFACE_GAPS \
             entry (delete them): {stale:?}"
        );
        // Terminal state: an empty table is pinned empty.
        if gaps_set.is_empty() {
            assert!(SURFACE_GAPS.is_empty());
        }
    }

    /// Regenerate `test/surface-gaps-ratchet.txt` from `SURFACE_GAPS`, keeping
    /// the leading comment/blank header. Ignored (run via `just ratchet-update`
    /// with `THEGN_RATCHET_UPDATE=1`); never adds debt — the pin is the guard.
    #[test]
    #[ignore = "regenerates test/surface-gaps-ratchet.txt; run via `just ratchet-update`"]
    fn surface_gaps_ratchet_update() {
        if std::env::var_os("THEGN_RATCHET_UPDATE").is_none() {
            return;
        }
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test/surface-gaps-ratchet.txt"
        );
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        // Preserve the leading comment/blank header block (up to the first data line).
        let mut out = String::new();
        for line in existing.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                out.push_str(line);
                out.push('\n');
            } else {
                break;
            }
        }
        let mut rows: Vec<(String, String, String)> = SURFACE_GAPS
            .iter()
            .map(|(id, s, why)| (id.to_string(), s.as_str().to_string(), why.to_string()))
            .collect();
        rows.sort();
        for (id, s, why) in rows {
            out.push_str(&format!("{id}\t{s}\t{why}\n"));
        }
        std::fs::write(path, out).expect("write surface-gaps-ratchet.txt");
    }
}
