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
        "browser.drive",
        Verb::DriveBrowser,
        SurfaceSet::ALL,
        "Drive the preview browser (navigate, reload)",
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
    // --- admin ---------------------------------------------------------------
    cap(
        "pairings.issue",
        Verb::IssuePairing,
        SurfaceSet::OPERATOR,
        "Mint a single-use pairing code",
    ),
    cap(
        "pairings.list",
        Verb::ListPairings,
        SurfaceSet::OPERATOR,
        "List pairings",
    ),
    cap(
        "pairings.revoke",
        Verb::RevokePairing,
        SurfaceSet::OPERATOR,
        "Revoke a pairing",
    ),
    cap(
        "pairings.approve",
        Verb::ApprovePairing,
        SurfaceSet::OPERATOR,
        "Approve a parked pairing",
    ),
    cap(
        "daemon.shutdown",
        Verb::Shutdown,
        SurfaceSet::OPERATOR,
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
];

/// Documented, shrink-only gaps: `(capability id, surface, why)`. A surface's
/// coverage test fails when a catalog row for that surface is neither
/// implemented nor listed here — and when an entry here names something the
/// surface now implements (stale excuses are removed, never accumulated).
pub const SURFACE_GAPS: &[(&str, Surface, &str)] = &[
    // -- gRPC: the mirror lags HTTP; each is a proto + handler addition -------
    (
        "sessions.wait",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "sessions.split",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "worktrees.list",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "merge.list",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "merge.add",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "merge.clear",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "calendar.events",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "calendar.clocks",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "calendar.ingest",
        Surface::Grpc,
        "not yet mirrored in control.proto",
    ),
    (
        "pairings.issue",
        Surface::Grpc,
        "pairing management is HTTP + CLI only",
    ),
    (
        "pairings.list",
        Surface::Grpc,
        "pairing management is HTTP + CLI only",
    ),
    (
        "pairings.revoke",
        Surface::Grpc,
        "pairing management is HTTP + CLI only",
    ),
    (
        "pairings.approve",
        Surface::Grpc,
        "pairing management is HTTP + CLI only",
    ),
    (
        "daemon.shutdown",
        Surface::Grpc,
        "shutdown is HTTP + CLI only",
    ),
    (
        "daemon.shutdown",
        Surface::Http,
        "no route: the daemon stops on signal / last-client policy, not by request",
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
    // -- CLI: verbs without a `thegn` subcommand yet ---------------------------
    ("daemon.shutdown", Surface::Cli, "no CLI verb yet"),
    // -- MCP / plugin: state tools land in the client-API / plugin-runtime phases
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
    (
        "sessions.open",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "sessions.detach",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "sessions.input",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "sessions.resize",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "sessions.snapshot",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "sessions.kill",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "sessions.wait",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "sessions.split",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "worktrees.open",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "browser.drive",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "git.status",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "git.stage",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "git.commit",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "merge.list",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "merge.add",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "merge.clear",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "pr.status",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "calendar.events",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "calendar.clocks",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "calendar.ingest",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "notify.push",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "events.subscribe",
        Surface::Plugin,
        "plugin subscribe lands in the plugin-runtime phase",
    ),
    (
        "leases.list",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
    ),
    (
        "me",
        Surface::Plugin,
        "host.call dispatches a first verb set; generic catalog dispatch lands in the client-API phase",
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

        // Implementing an excused row ⇒ stale gap.
        let mut stale = http_done.clone();
        stale.push("daemon.shutdown");
        let p = coverage_problems(Surface::Http, &stale);
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
        let req: Vec<&str> = required_for(Surface::Grpc).map(|c| c.id.0).collect();
        assert!(req.contains(&"sessions.list"));
        assert!(!req.contains(&"merge.list"));
    }
}
