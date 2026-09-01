//! The slot catalog: `(command path, arg id) → SourceKind`.
//!
//! This is the single source of truth for *which argument takes which values*.
//! Nothing binds a completer on the clap derive — the host walks [`CATALOG`] and
//! decorates the already-built command tree — which is what makes the host's
//! `completion_slots_are_bound_or_pinned` drift test meaningful: a new verb with
//! an uncompletable argument fails a test instead of quietly completing nothing.
//!
//! `command_path` is the space-joined path from the root: `"wt rm"`,
//! `"api call"`, `""` for a root-level argument.

/// Where a slot's values come from. Implemented-or-`reserved` in the repo's
/// seam idiom (`docs/ARCHITECTURE.md` §5): every value a slot can carry is
//  either served today or declared with the reason it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    // --- implemented, DB-derived (read-only state DB) -----------------------
    /// Registered worktree paths.
    Worktree,
    /// Known repositories / workspaces.
    Repo,
    /// Daemon sessions with a live lease.
    Session,
    /// Registered remote hosts.
    Host,

    // --- implemented, config-derived ----------------------------------------
    /// `[env.<name>]` execution environments.
    Env,
    /// `[profiles.<name>]` keybind profiles.
    Profile,
    /// Built-in and valid local user themes.
    Theme,
    /// `[[agents]]` names.
    Agent,
    /// `[[tools]]` names.
    Tool,
    /// Trusted `[[automations.rules]]` names.
    Automation,
    /// `[[plugins]]` ids.
    Plugin,
    /// `[[pipeline.stages]]` names.
    Stage,
    /// `[mcp_servers.<name>]` names.
    McpServer,
    /// Dotted config keys, as `config get`/`config set`/`--set` take them.
    ConfigKey,

    // --- implemented, in-process --------------------------------------------
    /// Capability ids from [`crate::capability::CATALOG`].
    Capability,
    /// Bindable action ids from [`crate::keymap::BUILTINS`].
    Action,
    /// Embedded skill package names. Configured user packages are discovered by
    /// the host and deliberately not walked on the latency-sensitive TAB path.
    Skill,

    /// clap already completes this slot from the tree itself — subcommand
    /// names, flags, and `ValueEnum` arguments such as `completions <shell>`.
    /// Declared rather than left unclassified, so "clap has it" is a decision
    /// on the record and not an omission.
    Structural,

    /// Declared, accepted by the catalog, deliberately not served. The payload
    /// names *which* reserved source and carries the reason.
    Reserved(Reserved),
}

/// The reserved value sources, each with the reason it is not served at
/// `<TAB>` time. Same honesty rule as a `config_enum!` reserved `kind`: the
/// slot is on the record with its reason, rather than silently absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reserved {
    /// Git refs.
    Branch,
    /// Pull requests.
    Pr,
    /// Issues.
    Issue,
    /// Agent-dispatch roster row ids (`agent_dispatches.id`). The roster is
    /// local SQLite, so a real source is implementable; nothing serves it yet.
    DispatchRow,
    /// Free-form scalars — a millisecond count, a duration — with no
    /// enumerable source. The engine's default (filenames) is the terminal
    /// answer; declared here so that is a decision on the record.
    Freeform,
}

impl Reserved {
    /// Every reserved source. Walked by the kind-coverage test.
    pub const ALL: &'static [Reserved] = &[
        Reserved::Branch,
        Reserved::Pr,
        Reserved::Issue,
        Reserved::DispatchRow,
        Reserved::Freeform,
    ];

    /// The stable string id.
    pub fn kind(self) -> &'static str {
        match self {
            Reserved::Branch => "branch",
            Reserved::Pr => "pr",
            Reserved::Issue => "issue",
            Reserved::DispatchRow => "dispatch-row",
            Reserved::Freeform => "freeform",
        }
    }

    /// Why it is reserved. Recorded here so a future implementer inherits the
    /// argument rather than re-deriving it.
    pub fn reason(self) -> &'static str {
        match self {
            Reserved::Branch => {
                "git I/O the <TAB> path declines to pay for; revisit once the \
                 git seam can be built without a full config load"
            }
            Reserved::Pr | Reserved::Issue => "network — a <TAB> must never make a forge call",
            Reserved::DispatchRow => {
                "the roster is local SQLite — a real source once the \
                 completion engine can read it without a full config load"
            }
            Reserved::Freeform => {
                "no enumerable source — a count or duration; the engine's \
                 default is the terminal answer by design"
            }
        }
    }
}

impl SourceKind {
    /// Every source kind, reserved ones included.
    pub const ALL: &'static [SourceKind] = &[
        SourceKind::Worktree,
        SourceKind::Repo,
        SourceKind::Session,
        SourceKind::Host,
        SourceKind::Env,
        SourceKind::Profile,
        SourceKind::Theme,
        SourceKind::Agent,
        SourceKind::Tool,
        SourceKind::Automation,
        SourceKind::Plugin,
        SourceKind::Stage,
        SourceKind::McpServer,
        SourceKind::ConfigKey,
        SourceKind::Capability,
        SourceKind::Action,
        SourceKind::Skill,
        SourceKind::Structural,
        SourceKind::Reserved(Reserved::Branch),
        SourceKind::Reserved(Reserved::Pr),
        SourceKind::Reserved(Reserved::Issue),
        SourceKind::Reserved(Reserved::DispatchRow),
        SourceKind::Reserved(Reserved::Freeform),
    ];

    /// The stable string id, as `thegn doctor` and the drift test print it.
    pub fn kind(self) -> &'static str {
        match self {
            SourceKind::Worktree => "worktree",
            SourceKind::Repo => "repo",
            SourceKind::Session => "session",
            SourceKind::Host => "host",
            SourceKind::Env => "env",
            SourceKind::Profile => "profile",
            SourceKind::Theme => "theme",
            SourceKind::Agent => "agent",
            SourceKind::Tool => "tool",
            SourceKind::Automation => "automation",
            SourceKind::Plugin => "plugin",
            SourceKind::Stage => "stage",
            SourceKind::McpServer => "mcp-server",
            SourceKind::ConfigKey => "config-key",
            SourceKind::Capability => "capability",
            SourceKind::Action => "action",
            SourceKind::Skill => "skill",
            SourceKind::Structural => "structural",
            SourceKind::Reserved(r) => r.kind(),
        }
    }

    /// Whether this kind produces candidates at `<TAB>` time.
    ///
    /// [`SourceKind::Structural`] is **not** implemented in this sense: clap
    /// serves it from the tree, and this module contributes nothing.
    pub fn is_implemented(self) -> bool {
        !matches!(self, SourceKind::Structural | SourceKind::Reserved(_))
    }

    /// Why the kind is reserved, if it is.
    pub fn reserved_reason(self) -> Option<&'static str> {
        match self {
            SourceKind::Reserved(r) => Some(r.reason()),
            _ => None,
        }
    }

    /// Whether serving this kind reads the state DB.
    pub fn reads_db(self) -> bool {
        matches!(
            self,
            SourceKind::Worktree | SourceKind::Repo | SourceKind::Session | SourceKind::Host
        )
    }

    /// Whether serving this kind needs the layered config loaded.
    pub fn reads_config(self) -> bool {
        matches!(
            self,
            SourceKind::Env
                | SourceKind::Profile
                | SourceKind::Agent
                | SourceKind::Tool
                | SourceKind::Automation
                | SourceKind::Plugin
                | SourceKind::Stage
                | SourceKind::McpServer
                | SourceKind::ConfigKey
        )
    }
}

/// One catalog row: an argument somewhere in the command tree, and where its
/// values come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Space-joined path from the root (`""` for a root-level argument).
    pub command_path: &'static str,
    /// The clap arg id (the field name, or the explicit `id`/`long`).
    pub arg_id: &'static str,
    pub source: SourceKind,
}

const fn slot(command_path: &'static str, arg_id: &'static str, source: SourceKind) -> Slot {
    Slot {
        command_path,
        arg_id,
        source,
    }
}

/// The catalog. Rows are grouped by source kind, and within a kind sorted by
/// command path, so a new verb lands next to its neighbours.
///
/// Not every implemented kind has a slot yet — `tool`, `plugin` and `action`
/// are served (and tested) but nothing in today's CLI grammar takes one, so
/// they wait for a verb rather than being bound to an approximation.
/// Everything a slot could take and this does not classify is pinned in
/// `test/completion-slot-ratchet.txt`, which only shrinks.
pub const CATALOG: &[Slot] = &[
    // --- worktree ---------------------------------------------------------
    // `worktree` is the `--worktree` flag; `worktree_pos` is the positional
    // twin the shared `WorktreeTarget` flattens in beside it.
    slot("agent sessions", "worktree", SourceKind::Worktree),
    slot("ci cancel", "worktree", SourceKind::Worktree),
    slot("ci detect", "worktree", SourceKind::Worktree),
    slot("ci logs", "worktree", SourceKind::Worktree),
    slot("ci rerun", "worktree", SourceKind::Worktree),
    slot("ci runs", "worktree", SourceKind::Worktree),
    slot("ci trigger", "worktree", SourceKind::Worktree),
    slot("ci view", "worktree", SourceKind::Worktree),
    slot("clean", "worktree", SourceKind::Worktree),
    slot("diff", "worktree", SourceKind::Worktree),
    slot("disk", "worktree", SourceKind::Worktree),
    slot("dispatch put", "worktree_path", SourceKind::Worktree),
    slot("env clear", "worktree", SourceKind::Worktree),
    slot("env clear", "worktree_pos", SourceKind::Worktree),
    slot("env deprovision", "worktree", SourceKind::Worktree),
    slot("env deprovision", "worktree_pos", SourceKind::Worktree),
    slot("env down", "worktree", SourceKind::Worktree),
    slot("env down", "worktree_pos", SourceKind::Worktree),
    slot("env forward", "worktree", SourceKind::Worktree),
    slot("env forward", "worktree_pos", SourceKind::Worktree),
    slot("env image-bake", "worktree", SourceKind::Worktree),
    slot("env image-bake", "worktree_pos", SourceKind::Worktree),
    slot("env provision", "worktree", SourceKind::Worktree),
    slot("env provision", "worktree_pos", SourceKind::Worktree),
    slot("env restore", "worktree", SourceKind::Worktree),
    slot("env restore", "worktree_pos", SourceKind::Worktree),
    slot("env set", "worktree", SourceKind::Worktree),
    slot("env set", "worktree_pos", SourceKind::Worktree),
    slot("env show", "worktree", SourceKind::Worktree),
    slot("env show", "worktree_pos", SourceKind::Worktree),
    slot("env snapshot", "worktree", SourceKind::Worktree),
    slot("env snapshot", "worktree_pos", SourceKind::Worktree),
    slot("env snapshots", "worktree", SourceKind::Worktree),
    slot("env snapshots", "worktree_pos", SourceKind::Worktree),
    slot("env up", "worktree", SourceKind::Worktree),
    slot("env up", "worktree_pos", SourceKind::Worktree),
    slot("forward stop", "worktree", SourceKind::Worktree),
    slot("issue comment", "worktree", SourceKind::Worktree),
    slot("issue create", "worktree", SourceKind::Worktree),
    slot("issue list", "worktree", SourceKind::Worktree),
    slot("issue view", "worktree", SourceKind::Worktree),
    slot("land", "worktree", SourceKind::Worktree),
    slot("land", "worktree_pos", SourceKind::Worktree),
    slot("map", "worktree", SourceKind::Worktree),
    slot("merge add", "worktrees", SourceKind::Worktree),
    slot("merge land", "worktree", SourceKind::Worktree),
    slot("merge land", "worktree_pos", SourceKind::Worktree),
    slot("merge retry", "worktree", SourceKind::Worktree),
    slot("merge retry", "worktree_pos", SourceKind::Worktree),
    slot("merge rm", "worktree", SourceKind::Worktree),
    slot("merge rm", "worktree_pos", SourceKind::Worktree),
    slot("notify push", "worktree", SourceKind::Worktree),
    slot("placement explain", "worktree", SourceKind::Worktree),
    slot("placement explain", "worktree_pos", SourceKind::Worktree),
    slot("placement plan", "worktree", SourceKind::Worktree),
    slot("placement plan", "worktree_pos", SourceKind::Worktree),
    slot("pr approve", "worktree", SourceKind::Worktree),
    slot("pr auto-merge", "worktree", SourceKind::Worktree),
    slot("pr comment", "worktree", SourceKind::Worktree),
    slot("pr create", "worktree", SourceKind::Worktree),
    slot("pr diff", "worktree", SourceKind::Worktree),
    slot("pr merge", "worktree", SourceKind::Worktree),
    slot("pr open", "worktree", SourceKind::Worktree),
    slot("pr queue add", "worktree", SourceKind::Worktree),
    slot("pr ready", "worktree", SourceKind::Worktree),
    slot("pr rerun-checks", "worktree", SourceKind::Worktree),
    slot("pr review", "worktree", SourceKind::Worktree),
    slot("pr reviews", "worktree", SourceKind::Worktree),
    slot("pr status", "worktree", SourceKind::Worktree),
    slot("sandbox-argv", "worktree", SourceKind::Worktree),
    slot("sandbox-argv", "worktree_pos", SourceKind::Worktree),
    slot("session open", "worktree", SourceKind::Worktree),
    slot("session fork", "worktree", SourceKind::Worktree),
    slot("session move", "worktree", SourceKind::Worktree),
    slot("share start", "worktree", SourceKind::Worktree),
    slot("share stop", "worktree", SourceKind::Worktree),
    slot("sprite-proxy", "worktree", SourceKind::Worktree),
    slot("wt clean", "worktree", SourceKind::Worktree),
    slot("wt diff", "worktree", SourceKind::Worktree),
    slot("wt disk", "worktree", SourceKind::Worktree),
    // "Worktree path or branch name" — the source offers both.
    slot("wt rm", "target", SourceKind::Worktree),
    // --- repo -------------------------------------------------------------
    slot("autopilot status", "repo", SourceKind::Repo),
    slot("config explain", "repo", SourceKind::Repo),
    slot("open", "repo", SourceKind::Repo),
    slot("project assign", "repo", SourceKind::Repo),
    slot("repo trust", "path", SourceKind::Repo),
    // The hidden legacy top-level alias of `repo trust`.
    slot("repo-trust", "path", SourceKind::Repo),
    slot("wt new", "repo", SourceKind::Repo),
    slot("zone assign", "repo", SourceKind::Repo),
    // --- session ----------------------------------------------------------
    slot("attach", "session", SourceKind::Session),
    slot("dispatch put", "session", SourceKind::Session),
    slot("events tail", "session", SourceKind::Session),
    slot("session attach", "session", SourceKind::Session),
    slot("session browse", "session", SourceKind::Session),
    slot("session fork", "session", SourceKind::Session),
    // A native harness id is not enumerable through the daemon session source;
    // recorded rows are selected through `agent.sessions` and the remaining
    // provider-specific value is deliberately free-form.
    slot(
        "session fork",
        "harness",
        SourceKind::Reserved(Reserved::Freeform),
    ),
    slot("session record", "session", SourceKind::Session),
    slot("session send", "session", SourceKind::Session),
    slot("session snapshot", "session", SourceKind::Session),
    slot("session split", "session", SourceKind::Session),
    slot("session wait", "session", SourceKind::Session),
    // --- host -------------------------------------------------------------
    // The `hosts` table — hosts thegn has actually registered. NOT the same as
    // `sandbox prune --host`, which takes a `[host.<name>]` config key and is
    // therefore still pinned.
    slot("host drain", "name", SourceKind::Host),
    slot("host probe", "name", SourceKind::Host),
    slot("host provision", "name", SourceKind::Host),
    slot("host rm", "name", SourceKind::Host),
    slot("host rm-cache", "name", SourceKind::Host),
    slot("host status", "name", SourceKind::Host),
    // --- env (`[env.<name>]`) ---------------------------------------------
    // `env create name` is deliberately absent: it names a NEW env, so
    // completing the existing ones would be actively misleading.
    slot("env deprovision", "env", SourceKind::Env),
    slot("env rm", "name", SourceKind::Env),
    slot("env set", "name", SourceKind::Env),
    slot("env test", "name", SourceKind::Env),
    slot("wt new", "env", SourceKind::Env),
    // --- profile (`[profiles.<name>]`) -------------------------------------
    // A `global = true` arg: classified once at the root, and clap propagates
    // the binding into every subcommand with the arg itself.
    slot("", "profile", SourceKind::Profile),
    slot("session move", "to_profile", SourceKind::Profile),
    // --- agent (`[[agents]]`) ----------------------------------------------
    // Both slots also accept an `[[tools]]` name or a bare provider id; the
    // configured agents are the useful majority and the arg stays free-form.
    slot("dispatch put", "agent_name", SourceKind::Agent),
    slot("session open", "agent", SourceKind::Agent),
    slot("session fork", "agent", SourceKind::Agent),
    // --- pipeline stage (`[[pipeline.stages]]` names) ----------------------
    slot("dispatch put", "stage", SourceKind::Stage),
    slot("session open", "stage", SourceKind::Stage),
    // --- automation (`[[automations.rules]]`) -----------------------------
    slot("automations test", "rule", SourceKind::Automation),
    // A cwd is a filesystem path; clap's structural/path completer owns it.
    slot("session fork", "cwd", SourceKind::Structural),
    // --- pipeline run-completion (THE-76) -----------------------------------
    // Roster row ids: local SQLite, so a real source is implementable — but
    // nothing serves them yet (see `Reserved::DispatchRow`).
    slot(
        "dispatch verify",
        "id",
        SourceKind::Reserved(Reserved::DispatchRow),
    ),
    slot(
        "dispatch wait",
        "row",
        SourceKind::Reserved(Reserved::DispatchRow),
    ),
    slot(
        "session open",
        "parent",
        SourceKind::Reserved(Reserved::DispatchRow),
    ),
    // `--resume-work <row>` targets the same roster (THE-86 chunk 1).
    slot(
        "session open",
        "resume_work",
        SourceKind::Reserved(Reserved::DispatchRow),
    ),
    // `--chunk <path>` records the chunk file a row dispatches under (THE-86
    // chunk 3) — a path under the worktree, like `parent_artifact`, so the
    // engine's filesystem completion is the intended behavior.
    slot("dispatch put", "chunk", SourceKind::Structural),
    slot("session open", "chunk", SourceKind::Structural),
    // --- pipeline slot claim + monitor lease --------------------------------
    // `dispatch claim` takes the same operands as `dispatch put`, so it takes
    // the same sources; `--artifact` is a path under the worktree (structural),
    // which is also why the claim verb does not inherit `put`'s pinned
    // `artifact` debt.
    slot("dispatch claim", "worktree_path", SourceKind::Worktree),
    slot("dispatch claim", "agent_name", SourceKind::Agent),
    slot("dispatch claim", "stage", SourceKind::Stage),
    slot("dispatch claim", "artifact", SourceKind::Structural),
    slot("dispatch claim", "chunk", SourceKind::Structural),
    slot(
        "dispatch claim",
        "parent",
        SourceKind::Reserved(Reserved::DispatchRow),
    ),
    // The override's justification — operator prose, like a report body.
    slot("dispatch claim", "allow_duplicate", SourceKind::Structural),
    // The lease verbs take an action word, an owner token, a TTL and a lease
    // name: all operator-chosen strings with no enumerable source.
    slot("dispatch lease", "action", SourceKind::Structural),
    slot("dispatch lease", "owner", SourceKind::Structural),
    slot("dispatch lease", "ttl", SourceKind::Structural),
    slot("dispatch lease", "name", SourceKind::Structural),
    // A tracker issue id in roster form (`linear:THE-76`) — network, like
    // every issue argument.
    slot(
        "session open",
        "issue",
        SourceKind::Reserved(Reserved::Issue),
    ),
    // A millisecond count — no enumerable source, on the record.
    slot(
        "dispatch wait",
        "timeout",
        SourceKind::Reserved(Reserved::Freeform),
    ),
    // --- pipeline report/note/status (THE-88) -----------------------------
    // `dispatch report` takes an id (roster row) and free-form text.
    slot(
        "dispatch report",
        "id",
        SourceKind::Reserved(Reserved::DispatchRow),
    ),
    slot("dispatch report", "text", SourceKind::Structural),
    // `dispatch note` takes an id (roster row) and free-form text.
    slot(
        "dispatch note",
        "id",
        SourceKind::Reserved(Reserved::DispatchRow),
    ),
    slot("dispatch note", "text", SourceKind::Structural),
    // `dispatch status` takes an optional row id (roster row) and an epoch-ms
    // since timestamp.
    slot(
        "dispatch status",
        "row",
        SourceKind::Reserved(Reserved::DispatchRow),
    ),
    slot(
        "dispatch status",
        "since",
        SourceKind::Reserved(Reserved::Freeform),
    ),
    // A path under the parent row's worktree; the engine's filesystem
    // completion is the intended behavior (same shape as `--config`).
    slot("session open", "parent_artifact", SourceKind::Structural),
    // `session close` targets the same lease table as every other session
    // verb — tombstones included, so an exited session still completes.
    slot("session close", "session", SourceKind::Session),
    // --- mcp server (`[mcp_servers.<name>]`) -------------------------------
    slot("mcp install", "name", SourceKind::McpServer),
    // --- config key --------------------------------------------------------
    // `--set` takes `KEY=VALUE`; completing the KEY half is still most of the
    // work, and prefix semantics stop as soon as the user types `=`.
    slot("", "overrides", SourceKind::ConfigKey),
    slot("config explain", "key", SourceKind::ConfigKey),
    slot("config get", "key", SourceKind::ConfigKey),
    slot("config set", "key", SourceKind::ConfigKey),
    // --- capability --------------------------------------------------------
    slot("api call", "cap", SourceKind::Capability),
    // --- embedded skill ---------------------------------------------------
    slot("skills show", "name", SourceKind::Skill),
    // Theme names are the merged built-in/local catalog. The import path is a
    // filesystem value and keeps clap's structural completion behavior.
    slot("theme set", "name", SourceKind::Theme),
    slot("theme import", "name", SourceKind::Theme),
    // --- structural (clap completes these from the tree) -------------------
    // `--config <PATH>`: a path, which the engine completes from the filesystem.
    // A `global = true` arg, so it is classified once at the root.
    slot("", "config", SourceKind::Structural),
    // `config validate --repo <PATH>`: clap owns filesystem path completion.
    slot("config validate", "repo", SourceKind::Structural),
    slot("completions", "shell", SourceKind::Structural),
    slot("automations test", "fixture", SourceKind::Structural),
    slot("events tail", "kinds", SourceKind::Structural),
    slot("skills seed", "worktree", SourceKind::Structural),
    slot("theme import", "file", SourceKind::Structural),
    slot("pr merge", "method", SourceKind::Structural),
    slot("pr review", "state", SourceKind::Structural),
    // --- reserved ----------------------------------------------------------
    slot("ci runs", "branch", SourceKind::Reserved(Reserved::Branch)),
    slot("diff", "base", SourceKind::Reserved(Reserved::Branch)),
    slot("pr create", "base", SourceKind::Reserved(Reserved::Branch)),
    slot("wt diff", "base", SourceKind::Reserved(Reserved::Branch)),
    slot("wt new", "base", SourceKind::Reserved(Reserved::Branch)),
    slot(
        "automations test",
        "event",
        SourceKind::Reserved(Reserved::Freeform),
    ),
    slot(
        "automations test",
        "at",
        SourceKind::Reserved(Reserved::Freeform),
    ),
    slot("pr queue add", "pr", SourceKind::Reserved(Reserved::Pr)),
    slot("pr queue rm", "number", SourceKind::Reserved(Reserved::Pr)),
    slot(
        "dispatch put",
        "issue_id",
        SourceKind::Reserved(Reserved::Issue),
    ),
    slot(
        "dispatch claim",
        "issue_id",
        SourceKind::Reserved(Reserved::Issue),
    ),
    slot(
        "issue comment",
        "number",
        SourceKind::Reserved(Reserved::Issue),
    ),
    slot(
        "issue view",
        "number",
        SourceKind::Reserved(Reserved::Issue),
    ),
    slot(
        "wt new",
        "from_issue",
        SourceKind::Reserved(Reserved::Issue),
    ),
];

/// The row for `(command_path, arg_id)`, if any.
pub fn lookup(command_path: &str, arg_id: &str) -> Option<&'static Slot> {
    CATALOG
        .iter()
        .find(|s| s.command_path == command_path && s.arg_id == arg_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The `kind_coverage` shape every other seam's tests use: each variant
    /// round-trips its id, and no id repeats.
    #[test]
    fn every_kind_has_a_unique_id() {
        let mut seen = HashSet::new();
        for k in SourceKind::ALL {
            let id = k.kind();
            assert!(!id.is_empty(), "{k:?} has an empty id");
            assert!(seen.insert(id), "duplicate source kind id: {id}");
        }
        assert_eq!(seen.len(), SourceKind::ALL.len());
    }

    #[test]
    fn reserved_kinds_are_declared_with_a_reason() {
        for r in Reserved::ALL {
            let k = SourceKind::Reserved(*r);
            assert!(!k.is_implemented(), "{k:?} must not read as implemented");
            let reason = k.reserved_reason().expect("reserved kinds carry a reason");
            assert!(reason.len() > 10, "{k:?} needs a real reason");
            assert_eq!(k.kind(), r.kind());
            // A reserved source is served by nothing, so it must claim neither
            // input — this is what keeps a reserved slot free at <TAB> time.
            assert!(!k.reads_db() && !k.reads_config());
        }
        // `Reserved::ALL` and the `SourceKind::ALL` reserved rows agree.
        let in_all = SourceKind::ALL
            .iter()
            .filter(|k| matches!(k, SourceKind::Reserved(_)))
            .count();
        assert_eq!(in_all, Reserved::ALL.len());
    }

    #[test]
    fn structural_is_declared_but_not_served_here() {
        assert!(!SourceKind::Structural.is_implemented());
        assert_eq!(SourceKind::Structural.reserved_reason(), None);
        assert!(!SourceKind::Structural.reads_db());
        assert!(!SourceKind::Structural.reads_config());
    }

    #[test]
    fn implemented_kinds_declare_exactly_one_input_class() {
        // Every implemented kind is DB-derived, config-derived, or in-process —
        // never both of the first two, or the host's lazy loading would pay for
        // an input the slot does not use.
        for k in SourceKind::ALL.iter().filter(|k| k.is_implemented()) {
            assert!(
                !(k.reads_db() && k.reads_config()),
                "{k:?} claims both the DB and the config"
            );
        }
        let in_process: Vec<&'static str> = SourceKind::ALL
            .iter()
            .filter(|k| k.is_implemented() && !k.reads_db() && !k.reads_config())
            .map(|k| k.kind())
            .collect();
        assert_eq!(in_process, ["theme", "capability", "action", "skill"]);
    }

    #[test]
    fn catalog_rows_are_unique_and_well_formed() {
        let mut seen = HashSet::new();
        for s in CATALOG {
            assert!(
                seen.insert((s.command_path, s.arg_id)),
                "duplicate catalog row: {:?} {:?}",
                s.command_path,
                s.arg_id
            );
            assert!(!s.arg_id.is_empty(), "empty arg id in {s:?}");
            assert!(
                !s.command_path.starts_with(' ') && !s.command_path.ends_with(' '),
                "command path must be space-joined, not padded: {s:?}"
            );
        }
    }

    #[test]
    fn lookup_finds_rows_by_path_and_arg() {
        for s in CATALOG {
            assert_eq!(lookup(s.command_path, s.arg_id), Some(s));
        }
        assert_eq!(lookup("no-such-verb", "nope"), None);
    }
}
