//! Control-plane domain model: auth scopes, token/pairing-URL formats, and
//! relay-lease timing math.
//!
//! Everything here is **pure** (no I/O, no clock, no crypto) so the security
//! decisions are exhaustively unit-tested: hashing happens in `thegn-svc`
//! (which has the CSPRNG + hasher), the store persists opaque strings
//! ([`crate::store::ControlStore`]), and every time-dependent function takes an
//! injected `now_ms`.

use serde::{Deserialize, Serialize};

use crate::store::LeaseRow;

// --- scopes -----------------------------------------------------------------

/// One capability a control-API token can hold.
///
/// `Git` deliberately does **not** imply `Write` (a phone that can commit must
/// not be able to type into a terminal) and vice versa; `Exec` is a third such
/// independent silo (a client that can trigger pre-declared launches need not be
/// able to type into terminals, and vice versa). All three imply `Read`.
/// `Admin` implies everything.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// List sessions/worktrees/leases, snapshots, the event feed.
    Read,
    /// Send terminal input, open worktrees, drive the preview browser.
    Write,
    /// Stage/commit through the GitBackend seam.
    Git,
    /// Launch a pre-declared `[[presets]]` shape into a workspace
    /// (`open --preset`). Runs configured commands — its own tier so an
    /// `open`/`write` token cannot execute launches, and vice versa.
    Exec,
    /// Pairing management, daemon shutdown.
    Admin,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Write => "write",
            Scope::Git => "git",
            Scope::Exec => "exec",
            Scope::Admin => "admin",
        }
    }

    fn bit(self) -> u8 {
        match self {
            Scope::Read => 1,
            Scope::Write => 2,
            Scope::Git => 4,
            Scope::Admin => 8,
            Scope::Exec => 16,
        }
    }
}

/// A set of scopes, stored in the DB as csv (`"read,git"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScopeSet(u8);

impl ScopeSet {
    pub fn empty() -> Self {
        ScopeSet(0)
    }

    pub fn of(scopes: &[Scope]) -> Self {
        let mut s = ScopeSet(0);
        for sc in scopes {
            s.insert(*sc);
        }
        s
    }

    pub fn insert(&mut self, scope: Scope) {
        self.0 |= scope.bit();
    }

    pub fn contains(&self, scope: Scope) -> bool {
        self.0 & scope.bit() != 0
    }

    /// Parse the csv storage form. Unknown names are ignored (never an escalation:
    /// dropping a name can only *narrow* the grant), so old builds reading a newer
    /// DB degrade safely.
    pub fn parse(csv: &str) -> ScopeSet {
        let mut s = ScopeSet(0);
        for part in csv.split(',') {
            match part.trim() {
                "read" => s.insert(Scope::Read),
                "write" => s.insert(Scope::Write),
                "git" => s.insert(Scope::Git),
                "exec" => s.insert(Scope::Exec),
                "admin" => s.insert(Scope::Admin),
                _ => {}
            }
        }
        s
    }

    /// The csv storage form, in canonical order.
    pub fn to_csv(&self) -> String {
        let mut out = Vec::new();
        for sc in [
            Scope::Read,
            Scope::Write,
            Scope::Git,
            Scope::Exec,
            Scope::Admin,
        ] {
            if self.contains(sc) {
                out.push(sc.as_str());
            }
        }
        out.join(",")
    }

    /// Does this grant satisfy a verb needing `need`? The implication lattice:
    /// `Admin` ⊇ all; `Write` ⊇ `Read`; `Git` ⊇ `Read`; `Exec` ⊇ `Read`; `Git`,
    /// `Write` and `Exec` are mutually independent.
    pub fn allows(&self, need: Scope) -> bool {
        if self.contains(Scope::Admin) {
            return true;
        }
        match need {
            Scope::Read => {
                self.0
                    & (Scope::Read.bit()
                        | Scope::Write.bit()
                        | Scope::Git.bit()
                        | Scope::Exec.bit())
                    != 0
            }
            Scope::Write => self.contains(Scope::Write),
            Scope::Git => self.contains(Scope::Git),
            Scope::Exec => self.contains(Scope::Exec),
            Scope::Admin => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// The intersection of two grants — the clamp-only primitive the MCP-serve
    /// ceiling resolution is built from.
    pub fn intersect(self, other: ScopeSet) -> ScopeSet {
        ScopeSet(self.0 & other.0)
    }

    /// Every scope, the widest possible grant (the base a config ceiling clamps
    /// down from). `Admin` is inert on the MCP surface — the catalog forbids
    /// admin caps there — but included so the base is the true universe.
    pub fn universe() -> ScopeSet {
        ScopeSet::of(&[Scope::Read, Scope::Write, Scope::Git, Scope::Admin])
    }
}

/// Which config level determined the effective MCP-serve scope set — reported at
/// server start and by `thegn doctor` so an operator can see *what* narrowed the
/// grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeClamp {
    /// Nothing configured and no flag: the safe built-in default applies.
    Default,
    /// The global `[mcp.serve] scopes` ceiling.
    Global,
    /// The active profile overlay (`[profiles.<p>.mcp_serve] scopes`).
    Profile,
    /// The workspace overlay (`[workspace.<slug>.mcp_serve] scopes`).
    Workspace,
    /// The `--scopes` invocation flag.
    Flag,
}

impl ScopeClamp {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeClamp::Default => "default",
            ScopeClamp::Global => "global [mcp.serve]",
            ScopeClamp::Profile => "profile overlay",
            ScopeClamp::Workspace => "workspace overlay",
            ScopeClamp::Flag => "--scopes flag",
        }
    }
}

/// Resolve the scope set `thegn mcp serve` grants, **clamp-only**: the global
/// ceiling, then the profile overlay, then the workspace overlay, then the
/// `--scopes` flag — each `Some` level intersects the running set, so an inner
/// level can only ever narrow the outer one, never widen it.
///
/// Each argument is `Some(set)` when that level is present and `None` when it is
/// absent (an absent level does not participate). A present-but-unparseable
/// level is the caller's `Some(ScopeSet::empty())` — **fail-closed**: it clamps
/// the grant to nothing rather than widening it. When no level is present at all
/// (and no flag), the safe `default_when_unset` applies (today's `read`).
///
/// Returns the effective set and the innermost level that narrowed it — the
/// "clamped by" the operator sees.
pub fn resolve_serve_scopes(
    global: Option<ScopeSet>,
    profile: Option<ScopeSet>,
    workspace: Option<ScopeSet>,
    flag: Option<ScopeSet>,
    default_when_unset: ScopeSet,
) -> (ScopeSet, ScopeClamp) {
    let levels = [
        (global, ScopeClamp::Global),
        (profile, ScopeClamp::Profile),
        (workspace, ScopeClamp::Workspace),
        (flag, ScopeClamp::Flag),
    ];
    if levels.iter().all(|(s, _)| s.is_none()) {
        return (default_when_unset, ScopeClamp::Default);
    }
    let mut eff = ScopeSet::universe();
    let mut clamp = ScopeClamp::Default;
    for (level, which) in levels {
        if let Some(s) = level {
            let narrowed = eff.intersect(s);
            if narrowed != eff {
                clamp = which;
            }
            eff = narrowed;
        }
    }
    (eff, clamp)
}

/// Every control-API verb, for the verb→scope table. Adapters (HTTP handlers,
/// gRPC methods, CLI) MUST route their scope checks through [`required_scope`]
/// so the policy lives in exactly one tested place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verb {
    ListSessions,
    ListWorktrees,
    OpenSession,
    Attach,
    Detach,
    SendInput,
    Resize,
    Snapshot,
    KillSession,
    OpenWorktree,
    /// Fetch one preview URL with the bounded, credential-free host executor.
    PreviewFetch,
    DriveBrowser,
    /// Block until a session reaches a state — observes only.
    Wait,
    /// Create a sibling pane/session — a write-side effect like `OpenSession`.
    Split,
    /// Launch a pre-declared `[[presets]]` shape into a workspace
    /// (`open --preset`). Name-only on the wire — argv/env/cwd resolve from the
    /// receiving instance's own config, never the payload.
    LaunchPreset,
    /// Start/stop/query a daemon-side asciicast recording of a session — a
    /// write-side effect (mutates daemon state and the filesystem).
    RecordSession,
    GitStatus,
    GitStage,
    GitCommit,
    MergeList,
    MergeAdd,
    MergeClear,
    /// Read the merged calendar over a date window.
    CalendarEvents,
    /// Read the resolved world clocks.
    CalendarClocks,
    /// Push events INTO a source's own cache — how a daemon-style plugin
    /// contributes a calendar. Not a mutation of any upstream provider; thegn
    /// stays read-only towards those.
    CalendarIngest,
    Events,
    LeaseStatus,
    Me,
    IssuePairing,
    ListPairings,
    RevokePairing,
    ApprovePairing,
    Shutdown,
    /// Read a worktree's cached PR status (the PR panel's header row).
    PrStatus,
    /// Push a notification into the tray (`thegn notify push` over the API).
    NotifyPush,
    /// Produce a redacted debug support bundle (`thegn doctor bundle`). An
    /// operator verb: CLI + control API, never MCP or plugins.
    DoctorBundle,
    /// Store a secret value into the broker (keyring/file), returning a ref.
    SecretSet,
    /// Remove a stored secret.
    SecretRm,
    /// List configured secret refs and their backends (names only, no values).
    SecretList,
    /// Migrate plaintext literal secrets out of config into the store.
    SecretMigrate,
    /// Summarize configured secret refs with backend + last probe outcome.
    SecretAudit,
    /// Rotate a managed SSH key across its scope's live instances.
    SecretSshRotate,
    /// Read the mcp-proxy hub's per-upstream state (`thegn mcp status`).
    McpProxyStatus,
    /// Re-read config and reconcile the mcp-proxy hub's upstreams
    /// (`thegn mcp reload`). Write-scoped: a read-only client must not be able
    /// to flip which upstream tools an agent can reach.
    McpProxyReload,
    /// List projects (grouping of workspaces) with member counts.
    ProjectList,
    /// Create a project.
    ProjectCreate,
    /// Rename a project.
    ProjectRename,
    /// Delete a project (refused while non-empty unless forced).
    ProjectRemove,
    /// Assign (or unassign) a workspace's project membership.
    ProjectAssign,
    /// Batched cross-repo feature creation: one linked branch name + a worktree
    /// in each of a project's member repos (`thegn wt new --project`).
    ProjectNewFeature,
    // --- agent orchestration (THE-57) ---------------------------------------
    /// List tracker issues (filtered) — observes the board.
    IssuesList,
    /// Read one tracker issue with its detail/comments.
    IssuesGet,
    /// Apply a patch (status/assignee/…) to a tracker issue — a write into an
    /// external system on the user's credentials.
    IssuesUpdate,
    /// Post a comment on a tracker issue — likewise a credentialed write.
    IssuesComment,
    /// List the durable agent-dispatch roster.
    DispatchesList,
    /// Record a new dispatch on the roster.
    DispatchesPut,
    /// Advance a dispatch's status on the roster.
    DispatchesSetStatus,
    /// Report whether a roster row's handoff artifact is real — exists in the
    /// worktree and is tracked by git. Observes only.
    DispatchesVerify,
    /// Block until an active roster row's session exits. Observes only.
    DispatchesWait,
    /// File a worker's structured handoff report on a roster row — writes to
    /// the local DB (thegn's own data). CLI-only (local SQLite).
    DispatchesReport,
    /// Append a progress note to a dispatch row's queue — writes to the local
    /// DB. CLI-only.
    DispatchesNote,
    /// Read status digests for roster rows with reports and notes — observes
    /// only. CLI-only.
    DispatchesStatus,
    /// Create a worktree (optionally from an issue) — writes to git + the fs.
    WorktreeCreate,
    /// Run a workspace text/structural search (read-only; `thegn search`).
    SearchQuery,
    /// Apply a workspace search-and-replace through the guarded write path
    /// (`thegn search --replace … --apply`).
    SearchReplace,
    /// Enumerate remote-host candidates from a mesh VPN (`thegn host discover`).
    /// Observes only — reads the local tailnet client, writes nothing.
    HostDiscover,
    /// List thegn's containers across detected backends (owned + foreign, the
    /// foreign ones read-only). Observes only.
    ContainersList,
    /// Lifecycle on an OWNED container: stop/start/restart/logs. Structurally
    /// owned-only (`sandbox_manage`); write-side effect.
    ContainersControl,
    /// Owned-estate cleanup: `sandbox gc` + `sandbox prune`. Destructive and
    /// estate-wide — admin, the same tier as daemon shutdown.
    ContainersPrune,
    /// Render a worktree's ranked, budgeted repo map from the entity index —
    /// observes only (`thegn map`, the `semantic.map` MCP tool).
    SemanticMap,
    /// Read a worktree's blast-radius (changed entities + callers + risk) from
    /// the persisted semantic graph — observes only (`semantic.blast_radius`).
    SemanticBlastRadius,
    /// List discovered coding-agent sessions from each harness's local store —
    /// observes local transcripts only (read), never spends tokens.
    AgentSessions,
    /// The effective launch view of every `[[agents]]`/`[[tools]]` entry and
    /// pipeline stage (harness, model, env keys, permissions) — config-derived
    /// (read), no process is started.
    AgentList,
    /// Report the model proxy's enabled/listen/reachability status.
    ModelProxyStatus,
    /// Read the model proxy's spend/token/latency stats rollup.
    ModelProxyStats,
    /// Start (launch) the model proxy daemon.
    ModelProxyStart,
    /// Stop (terminate) the model proxy daemon.
    ModelProxyStop,
}

impl Verb {
    /// Every verb, for exhaustiveness tests and the capability catalog. Kept
    /// by hand (no `strum` in the workspace); `all_verbs_listed` pins it
    /// against the enum.
    pub const ALL: &'static [Verb] = &[
        Verb::ListSessions,
        Verb::ListWorktrees,
        Verb::OpenSession,
        Verb::Attach,
        Verb::Detach,
        Verb::SendInput,
        Verb::Resize,
        Verb::Snapshot,
        Verb::KillSession,
        Verb::OpenWorktree,
        Verb::PreviewFetch,
        Verb::DriveBrowser,
        Verb::Wait,
        Verb::Split,
        Verb::LaunchPreset,
        Verb::RecordSession,
        Verb::GitStatus,
        Verb::GitStage,
        Verb::GitCommit,
        Verb::MergeList,
        Verb::MergeAdd,
        Verb::MergeClear,
        Verb::CalendarEvents,
        Verb::CalendarClocks,
        Verb::CalendarIngest,
        Verb::Events,
        Verb::LeaseStatus,
        Verb::Me,
        Verb::IssuePairing,
        Verb::ListPairings,
        Verb::RevokePairing,
        Verb::ApprovePairing,
        Verb::Shutdown,
        Verb::PrStatus,
        Verb::NotifyPush,
        Verb::DoctorBundle,
        Verb::SecretSet,
        Verb::SecretRm,
        Verb::SecretList,
        Verb::SecretMigrate,
        Verb::SecretAudit,
        Verb::SecretSshRotate,
        Verb::McpProxyStatus,
        Verb::McpProxyReload,
        Verb::ProjectList,
        Verb::ProjectCreate,
        Verb::ProjectRename,
        Verb::ProjectRemove,
        Verb::ProjectAssign,
        Verb::ProjectNewFeature,
        Verb::IssuesList,
        Verb::IssuesGet,
        Verb::IssuesUpdate,
        Verb::IssuesComment,
        Verb::DispatchesList,
        Verb::DispatchesPut,
        Verb::DispatchesSetStatus,
        Verb::DispatchesVerify,
        Verb::DispatchesWait,
        Verb::DispatchesReport,
        Verb::DispatchesNote,
        Verb::DispatchesStatus,
        Verb::WorktreeCreate,
        Verb::SearchQuery,
        Verb::SearchReplace,
        Verb::HostDiscover,
        Verb::ContainersList,
        Verb::ContainersControl,
        Verb::ContainersPrune,
        Verb::SemanticMap,
        Verb::SemanticBlastRadius,
        Verb::AgentSessions,
        Verb::AgentList,
        Verb::ModelProxyStatus,
        Verb::ModelProxyStats,
        Verb::ModelProxyStart,
        Verb::ModelProxyStop,
    ];

    /// Whether this verb produces a *stream* (pane output on attach, the event
    /// feed) rather than a single request/response. Streaming verbs are not
    /// dispatchable through the generic request spine that the MCP and plugin
    /// `host.call` surfaces use — the plugin feed subscription bridges the event
    /// stream separately.
    pub fn is_streaming(self) -> bool {
        matches!(self, Verb::Attach | Verb::Events)
    }
}

/// The single verb→scope policy table.
pub fn required_scope(verb: Verb) -> Scope {
    match verb {
        Verb::ListSessions
        | Verb::ListWorktrees
        | Verb::Snapshot
        | Verb::Events
        | Verb::LeaseStatus
        | Verb::GitStatus
        | Verb::MergeList
        | Verb::CalendarEvents
        | Verb::CalendarClocks
        | Verb::Wait
        | Verb::PrStatus
        | Verb::McpProxyStatus
        | Verb::ProjectList
        | Verb::IssuesList
        | Verb::IssuesGet
        | Verb::DispatchesList
        // `dispatches.verify`/`wait`/`status` (THE-76/THE-88) observe only: verify reads the
        // worktree + roster, wait composes the routed `sessions.wait`,
        // status reads the roster + notes.
        | Verb::DispatchesVerify
        | Verb::DispatchesWait
        | Verb::DispatchesStatus
        | Verb::SearchQuery
        | Verb::HostDiscover
        | Verb::ContainersList
        | Verb::SemanticMap
        | Verb::SemanticBlastRadius
        | Verb::AgentSessions
        | Verb::AgentList
        | Verb::PreviewFetch
        // Model-proxy status/stats are read-only introspection.
        | Verb::ModelProxyStatus
        | Verb::ModelProxyStats
        | Verb::Me => Scope::Read,
        // Attaching streams pane output (read) but registers a client that
        // holds the session and can resize it — that is a write-side effect.
        Verb::OpenSession
        | Verb::Attach
        | Verb::Detach
        | Verb::SendInput
        | Verb::Resize
        | Verb::KillSession
        | Verb::OpenWorktree
        | Verb::DriveBrowser
        | Verb::CalendarIngest
        | Verb::NotifyPush
        | Verb::McpProxyReload
        | Verb::ProjectCreate
        | Verb::ProjectRename
        | Verb::ProjectRemove
        | Verb::ProjectAssign
        | Verb::ProjectNewFeature
        | Verb::IssuesUpdate
        | Verb::IssuesComment
        | Verb::DispatchesPut
        | Verb::DispatchesSetStatus
        | Verb::DispatchesReport
        | Verb::DispatchesNote
        | Verb::SearchReplace
        | Verb::ContainersControl
        | Verb::Split
        | Verb::RecordSession => Scope::Write,
        Verb::GitStage
        | Verb::GitCommit
        | Verb::MergeAdd
        | Verb::MergeClear
        | Verb::WorktreeCreate => Scope::Git,
        // Executing configured commands is a strictly bigger power than focusing
        // a workspace — its own exec-level scope, never `open`'s / `write`'s.
        Verb::LaunchPreset => Scope::Exec,
        Verb::IssuePairing
        | Verb::ListPairings
        | Verb::RevokePairing
        | Verb::ApprovePairing
        | Verb::ContainersPrune
        | Verb::DoctorBundle
        | Verb::Shutdown
        // Secret custody is an operator/admin concern — never reachable from a
        // tool-calling agent (the catalog rows are OPERATOR-surface, and Admin
        // scope keeps them off any lower-scoped door).
        | Verb::SecretSet
        | Verb::SecretRm
        | Verb::SecretList
        | Verb::SecretMigrate
        | Verb::SecretAudit
        | Verb::SecretSshRotate
        // Starting/stopping a spend-capable daemon is an admin action; the
        // OPERATOR surfaces + Admin scope keep it off any tool-calling door.
        | Verb::ModelProxyStart
        | Verb::ModelProxyStop => Scope::Admin,
    }
}

// --- token formats ----------------------------------------------------------

/// Which credential family a presented string belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// `tgc1_…` — a long-lived scoped bearer token (minted by redeeming a code).
    Control,
    /// `tgp1_…` — a single-use pairing code embedded in a pairing URL.
    PairingCode,
}

impl TokenKind {
    fn prefix(self) -> &'static str {
        match self {
            TokenKind::Control => "tgc1",
            TokenKind::PairingCode => "tgp1",
        }
    }
}

/// The two halves of a parsed credential: the public lookup `id` (safe to log,
/// the `pairings.pairing_id` key) and the `secret` whose sha-256 the store holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenParts {
    pub id: String,
    pub secret: String,
}

/// Expected hex length of the id half (4 random bytes).
pub const TOKEN_ID_HEX: usize = 8;
/// Expected hex length of the secret half (32 random bytes ⇒ 256-bit entropy —
/// why a fast sha-256 hash, not argon2, is the right stored form).
pub const TOKEN_SECRET_HEX: usize = 64;

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Format a credential from its raw random halves (caller generates the bytes).
pub fn format_token(kind: TokenKind, id: &str, secret: &str) -> String {
    format!("{}_{}_{}", kind.prefix(), id, secret)
}

/// Parse a presented credential. Returns `None` for anything malformed —
/// callers treat that identically to a failed hash match (one rejection path).
pub fn parse_token(s: &str) -> Option<(TokenKind, TokenParts)> {
    let mut it = s.splitn(3, '_');
    let (prefix, id, secret) = (it.next()?, it.next()?, it.next()?);
    let kind = match prefix {
        "tgc1" => TokenKind::Control,
        "tgp1" => TokenKind::PairingCode,
        _ => return None,
    };
    if !is_lower_hex(id, TOKEN_ID_HEX) || !is_lower_hex(secret, TOKEN_SECRET_HEX) {
        return None;
    }
    Some((
        kind,
        TokenParts {
            id: id.to_string(),
            secret: secret.to_string(),
        },
    ))
}

// --- pairing URL ------------------------------------------------------------

/// A pairing URL: everything a thin client needs to redeem a code against a
/// `thegn serve` instance. `fp` is reserved for a TLS certificate fingerprint
/// (v1 serves plaintext behind a trusted network; the slot keeps v2 pinning an
/// additive change, not a format break).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingUrl {
    pub host: String,
    pub port: u16,
    /// The full `tgp1_…` pairing code.
    pub code: String,
    pub fp: Option<String>,
}

impl PairingUrl {
    /// The app-scheme form: `thegn://pair?host=H&port=P&t=tgp1_…[&fp=…]`.
    pub fn encode(&self) -> String {
        let mut s = format!(
            "thegn://pair?host={}&port={}&t={}",
            self.host, self.port, self.code
        );
        if let Some(fp) = &self.fp {
            s.push_str("&fp=");
            s.push_str(fp);
        }
        s
    }

    /// The web-redeem form: `http://H:P/pair#t=tgp1_…`. The code rides in the
    /// fragment so it never appears in server request logs.
    pub fn web_form(&self) -> String {
        format!("http://{}:{}/pair#t={}", self.host, self.port, self.code)
    }

    /// Parse the app-scheme form. Hosts are restricted to URL-safe chars by
    /// construction (hostname / IP / tailnet name); anything else fails parse.
    pub fn parse(s: &str) -> Option<PairingUrl> {
        let rest = s.strip_prefix("thegn://pair?")?;
        let mut host = None;
        let mut port = None;
        let mut code = None;
        let mut fp = None;
        for kv in rest.split('&') {
            let (k, v) = kv.split_once('=')?;
            match k {
                "host" if !v.is_empty() => host = Some(v.to_string()),
                "port" => port = Some(v.parse::<u16>().ok()?),
                "t" => {
                    // Must be a well-formed pairing code, not a control token.
                    let (kind, _) = parse_token(v)?;
                    if kind != TokenKind::PairingCode {
                        return None;
                    }
                    code = Some(v.to_string());
                }
                "fp" if !v.is_empty() => fp = Some(v.to_string()),
                _ => return None,
            }
        }
        Some(PairingUrl {
            host: host?,
            port: port?,
            code: code?,
            fp,
        })
    }
}

// --- relay-lease math -------------------------------------------------------

/// What the daemon's lease supervisor should do right now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LeasePlan {
    /// Lease ids whose grace period has ended — reap their PTYs.
    pub reap: Vec<i64>,
    /// When the next un-expired relay lease ends (the supervisor's next wake);
    /// `None` when no timed lease is pending (sleep until notified).
    pub next_wake_at: Option<i64>,
}

/// Pure supervisor decision: given the daemon's leases and now, which relay
/// leases to reap and when to wake next. `attached` leases (no expiry) never
/// reap; only `kind == "relay"` rows with an expiry participate.
pub fn plan_leases(leases: &[LeaseRow], now_ms: i64) -> LeasePlan {
    let mut plan = LeasePlan::default();
    for l in leases {
        if l.kind != "relay" {
            continue;
        }
        let Some(exp) = l.expires_at else { continue };
        if exp <= now_ms {
            plan.reap.push(l.lease_id);
        } else {
            plan.next_wake_at = Some(match plan.next_wake_at {
                Some(cur) => cur.min(exp),
                None => exp,
            });
        }
    }
    plan
}

/// Expiry instant for a fresh relay lease opened at `now_ms` with the
/// configured grace period.
pub fn relay_expiry(now_ms: i64, grace_ms: i64) -> i64 {
    now_ms.saturating_add(grace_ms.max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(id: i64, kind: &str, expires_at: Option<i64>) -> LeaseRow {
        LeaseRow {
            lease_id: id,
            session_id: format!("s{id}"),
            daemon_id: "d".into(),
            client_id: None,
            kind: kind.into(),
            created_at: 0,
            expires_at,
        }
    }

    #[test]
    fn scope_set_parse_csv_round_trip() {
        for csv in ["read", "read,write", "read,git", "read,write,git,admin", ""] {
            assert_eq!(ScopeSet::parse(csv).to_csv(), csv);
        }
        // Whitespace and unknown names are tolerated; unknowns only narrow.
        let s = ScopeSet::parse(" read , FUTURE_SCOPE ,git ");
        assert_eq!(s.to_csv(), "read,git");
        assert!(ScopeSet::parse("bogus").is_empty());
    }

    #[test]
    fn scope_lattice() {
        let read = ScopeSet::of(&[Scope::Read]);
        let write = ScopeSet::of(&[Scope::Write]);
        let git = ScopeSet::of(&[Scope::Git]);
        let exec = ScopeSet::of(&[Scope::Exec]);
        let admin = ScopeSet::of(&[Scope::Admin]);

        // Read is implied by every non-empty grant.
        for s in [read, write, git, exec, admin] {
            assert!(s.allows(Scope::Read), "{s:?} should allow read");
        }
        assert!(!ScopeSet::empty().allows(Scope::Read));

        // Write, Git and Exec are independent silos: a git-scoped phone must not
        // be able to type into a terminal, a write token can't commit, and
        // neither can trigger a preset launch.
        assert!(write.allows(Scope::Write) && !write.allows(Scope::Git));
        assert!(git.allows(Scope::Git) && !git.allows(Scope::Write));
        assert!(exec.allows(Scope::Exec) && !exec.allows(Scope::Write));
        assert!(!write.allows(Scope::Exec) && !git.allows(Scope::Exec));

        // Admin implies everything; nothing else implies admin.
        for need in [
            Scope::Read,
            Scope::Write,
            Scope::Git,
            Scope::Exec,
            Scope::Admin,
        ] {
            assert!(admin.allows(need));
        }
        for s in [read, write, git, exec] {
            assert!(!s.allows(Scope::Admin));
        }
    }

    #[test]
    fn verb_scope_table_is_exhaustive_and_least_privilege() {
        use Verb::*;
        let read = [
            ListSessions,
            ListWorktrees,
            Snapshot,
            Events,
            LeaseStatus,
            GitStatus,
            MergeList,
            CalendarEvents,
            CalendarClocks,
            Wait,
            Me,
            PrStatus,
            McpProxyStatus,
            ProjectList,
            IssuesList,
            IssuesGet,
            DispatchesList,
            DispatchesVerify,
            DispatchesWait,
            DispatchesStatus,
            SearchQuery,
            HostDiscover,
            ContainersList,
            SemanticMap,
            SemanticBlastRadius,
            AgentSessions,
            AgentList,
            PreviewFetch,
            ModelProxyStatus,
            ModelProxyStats,
        ];
        let write = [
            OpenSession,
            Attach,
            Detach,
            SendInput,
            Resize,
            KillSession,
            OpenWorktree,
            DriveBrowser,
            Split,
            RecordSession,
            CalendarIngest,
            NotifyPush,
            McpProxyReload,
            ProjectCreate,
            ProjectRename,
            ProjectRemove,
            ProjectAssign,
            ProjectNewFeature,
            IssuesUpdate,
            IssuesComment,
            DispatchesPut,
            DispatchesSetStatus,
            DispatchesReport,
            DispatchesNote,
            SearchReplace,
            ContainersControl,
        ];
        let git = [GitStage, GitCommit, MergeAdd, MergeClear, WorktreeCreate];
        let exec = [LaunchPreset];
        let admin = [
            IssuePairing,
            ListPairings,
            RevokePairing,
            ApprovePairing,
            Shutdown,
            DoctorBundle,
            // Credential-broker verbs (THE-66) — secret custody is admin-only.
            SecretSet,
            SecretRm,
            SecretList,
            SecretMigrate,
            SecretAudit,
            SecretSshRotate,
            ContainersPrune,
            // Model-proxy lifecycle (THE-58) — start/stop a spend-capable daemon.
            ModelProxyStart,
            ModelProxyStop,
        ];
        for v in read {
            assert_eq!(required_scope(v), Scope::Read, "{v:?}");
        }
        for v in write {
            assert_eq!(required_scope(v), Scope::Write, "{v:?}");
        }
        for v in git {
            assert_eq!(required_scope(v), Scope::Git, "{v:?}");
        }
        for v in exec {
            assert_eq!(required_scope(v), Scope::Exec, "{v:?}");
        }
        for v in admin {
            assert_eq!(required_scope(v), Scope::Admin, "{v:?}");
        }
        // The spec scenario: a read-only view set requires only Read.
        let read_only = ScopeSet::of(&[Scope::Read]);
        for v in read {
            assert!(read_only.allows(required_scope(v)));
        }
        for v in write.iter().chain(&git).chain(&exec).chain(&admin) {
            assert!(
                !read_only.allows(required_scope(*v)),
                "{v:?} leaked to read"
            );
        }
        // `Verb::ALL` is hand-maintained: pin it to the five policy groups so a
        // verb added to the enum (and therefore to `required_scope`) cannot be
        // forgotten here.
        let mut grouped: Vec<Verb> = read
            .iter()
            .chain(&write)
            .chain(&git)
            .chain(&exec)
            .chain(&admin)
            .copied()
            .collect();
        let mut all: Vec<Verb> = Verb::ALL.to_vec();
        grouped.sort_by_key(|v| format!("{v:?}"));
        all.sort_by_key(|v| format!("{v:?}"));
        assert_eq!(all, grouped, "Verb::ALL and the policy groups disagree");
        let mut dedup = all.clone();
        dedup.dedup();
        assert_eq!(dedup.len(), all.len(), "Verb::ALL has a duplicate");
    }

    #[test]
    fn serve_scopes_default_when_nothing_configured() {
        // No level and no flag → the safe built-in default (read), reported as
        // the Default clamp. This is today's `thegn mcp serve` behaviour.
        let read = ScopeSet::of(&[Scope::Read]);
        let (eff, clamp) = resolve_serve_scopes(None, None, None, None, read);
        assert_eq!(eff.to_csv(), "read");
        assert_eq!(clamp, ScopeClamp::Default);
    }

    #[test]
    fn serve_scopes_flag_alone_narrows_from_universe() {
        // `--scopes write` with no config → write (universe ∩ write), exactly
        // as the flag behaved before config resolution existed.
        let read = ScopeSet::of(&[Scope::Read]);
        let flag = Some(ScopeSet::of(&[Scope::Write]));
        let (eff, clamp) = resolve_serve_scopes(None, None, None, flag, read);
        assert_eq!(eff.to_csv(), "write");
        assert_eq!(clamp, ScopeClamp::Flag);
        // An empty flag (`--scopes none`) fails closed to nothing.
        let (eff, _) = resolve_serve_scopes(None, None, None, Some(ScopeSet::empty()), read);
        assert!(eff.is_empty());
    }

    #[test]
    fn serve_scopes_workspace_overlay_narrows_and_is_reported() {
        // Global grants read+write; workspace lists only read → read, clamped
        // at the workspace overlay (the spec scenario).
        let read = ScopeSet::of(&[Scope::Read]);
        let global = Some(ScopeSet::of(&[Scope::Read, Scope::Write]));
        let workspace = Some(ScopeSet::of(&[Scope::Read]));
        let (eff, clamp) = resolve_serve_scopes(global, None, workspace, None, read);
        assert_eq!(eff.to_csv(), "read");
        assert_eq!(clamp, ScopeClamp::Workspace);
    }

    #[test]
    fn serve_scopes_inner_level_cannot_widen_the_outer() {
        // Global ceiling is read; a workspace overlay listing read+write cannot
        // widen it — the excess write is not honored (the spec scenario).
        let read = ScopeSet::of(&[Scope::Read]);
        let global = Some(ScopeSet::of(&[Scope::Read]));
        let workspace = Some(ScopeSet::of(&[Scope::Read, Scope::Write]));
        let (eff, _clamp) = resolve_serve_scopes(global, None, workspace, None, read);
        assert_eq!(eff.to_csv(), "read", "workspace cannot grant beyond global");
        assert!(!eff.contains(Scope::Write));
    }

    #[test]
    fn serve_scopes_fail_closed_on_unparseable_level() {
        // A present-but-unparseable level is the caller's empty set: it clamps
        // to nothing rather than widening. The widest possible result is the
        // global ceiling — never "everything".
        let read = ScopeSet::of(&[Scope::Read]);
        let global = Some(ScopeSet::of(&[Scope::Read, Scope::Write]));
        let profile = Some(ScopeSet::empty()); // e.g. `scopes = ["bogus"]`
        let (eff, _) = resolve_serve_scopes(global, profile, None, None, read);
        assert!(eff.is_empty(), "an unparseable inner level fails closed");
    }

    #[test]
    fn serve_scopes_full_ladder_intersects() {
        // global rw, profile rw+git, workspace r+w, flag r → the running
        // intersection is read; the flag is the innermost narrower here.
        let read = ScopeSet::of(&[Scope::Read]);
        let (eff, clamp) = resolve_serve_scopes(
            Some(ScopeSet::of(&[Scope::Read, Scope::Write])),
            Some(ScopeSet::of(&[Scope::Read, Scope::Write, Scope::Git])),
            Some(ScopeSet::of(&[Scope::Read, Scope::Write])),
            Some(ScopeSet::of(&[Scope::Read])),
            read,
        );
        assert_eq!(eff.to_csv(), "read");
        assert_eq!(clamp, ScopeClamp::Flag);
    }

    #[test]
    fn scope_set_intersect_and_universe() {
        let rw = ScopeSet::of(&[Scope::Read, Scope::Write]);
        let rg = ScopeSet::of(&[Scope::Read, Scope::Git]);
        assert_eq!(rw.intersect(rg).to_csv(), "read");
        assert_eq!(ScopeSet::universe().to_csv(), "read,write,git,admin");
        assert!(ScopeSet::universe().intersect(rw) == rw);
    }

    const ID: &str = "0123abcd";
    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn token_format_parse_round_trip() {
        for kind in [TokenKind::Control, TokenKind::PairingCode] {
            let s = format_token(kind, ID, SECRET);
            let (k, parts) = parse_token(&s).unwrap();
            assert_eq!(k, kind);
            assert_eq!(parts.id, ID);
            assert_eq!(parts.secret, SECRET);
        }
        assert!(format_token(TokenKind::Control, ID, SECRET).starts_with("tgc1_"));
        assert!(format_token(TokenKind::PairingCode, ID, SECRET).starts_with("tgp1_"));
    }

    #[test]
    fn malformed_tokens_reject() {
        let good = format_token(TokenKind::Control, ID, SECRET);
        assert!(parse_token(&good).is_some());
        for bad in [
            "",
            "tgc1",
            "tgc1__",
            "notaprefix_0123abcd_deadbeef",
            &format!("tgx1_{ID}_{SECRET}"),          // unknown prefix
            &format!("tgc1_{ID}"),                   // missing secret
            &format!("tgc1_short_{SECRET}"),         // id wrong length
            &format!("tgc1_{ID}_{}", &SECRET[..60]), // secret wrong length
            &format!("tgc1_{ID}_{}", SECRET.to_uppercase()), // not lower hex
            &format!("tgc1_{}_{SECRET}", "0123ABCD"),
            &good[..good.len() - 1],
        ] {
            assert!(parse_token(bad).is_none(), "should reject {bad:?}");
        }
    }

    #[test]
    fn pairing_url_round_trip() {
        let code = format_token(TokenKind::PairingCode, ID, SECRET);
        for fp in [None, Some("aabbcc".to_string())] {
            let u = PairingUrl {
                host: "studio.tail1234.ts.net".into(),
                port: 5380,
                code: code.clone(),
                fp: fp.clone(),
            };
            let parsed = PairingUrl::parse(&u.encode()).unwrap();
            assert_eq!(parsed, u);
        }
        // The web form carries the code in the fragment (never in access logs).
        let u = PairingUrl {
            host: "10.0.0.5".into(),
            port: 80,
            code: code.clone(),
            fp: None,
        };
        assert_eq!(u.web_form(), format!("http://10.0.0.5:80/pair#t={code}"));
    }

    #[test]
    fn pairing_url_rejects_malformed() {
        let code = format_token(TokenKind::PairingCode, ID, SECRET);
        let control = format_token(TokenKind::Control, ID, SECRET);
        for bad in [
            "".to_string(),
            "https://pair?host=h&port=1&t=x".to_string(),
            format!("thegn://pair?port=1&t={code}"), // no host
            format!("thegn://pair?host=h&t={code}"), // no port
            "thegn://pair?host=h&port=1".to_string(), // no code
            format!("thegn://pair?host=h&port=notaport&t={code}"),
            format!("thegn://pair?host=h&port=1&t={control}"), // control token, not a code
            format!("thegn://pair?host=h&port=1&t={code}&evil=1"), // unknown param
        ] {
            assert!(PairingUrl::parse(&bad).is_none(), "should reject {bad:?}");
        }
    }

    #[test]
    fn plan_leases_reap_boundary_and_next_wake() {
        let leases = vec![
            lease(1, "attached", None),     // never reaps
            lease(2, "relay", Some(5_000)), // expired at 5_000
            lease(3, "relay", Some(9_000)), // pending
            lease(4, "relay", Some(7_000)), // pending, earlier
            lease(5, "relay", None),        // malformed (no expiry): ignored
        ];
        // Strictly before the boundary nothing reaps.
        let p = plan_leases(&leases, 4_999);
        assert!(p.reap.is_empty());
        assert_eq!(p.next_wake_at, Some(5_000));
        // At the boundary the expired lease reaps; wake = earliest survivor.
        let p = plan_leases(&leases, 5_000);
        assert_eq!(p.reap, vec![2]);
        assert_eq!(p.next_wake_at, Some(7_000));
        // Past everything: all timed leases reap, nothing to wake for.
        let p = plan_leases(&leases, 10_000);
        assert_eq!(p.reap, vec![2, 3, 4]);
        assert_eq!(p.next_wake_at, None);
        // Empty input → idle plan.
        assert_eq!(plan_leases(&[], 0), LeasePlan::default());
    }

    #[test]
    fn relay_expiry_saturates() {
        assert_eq!(relay_expiry(1_000, 60_000), 61_000);
        assert_eq!(relay_expiry(1_000, -5), 1_000); // negative grace clamps
        assert_eq!(relay_expiry(i64::MAX - 1, 100), i64::MAX);
    }
}
