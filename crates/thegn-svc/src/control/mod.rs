//! The control-plane service seam: the API a pane daemon exposes and every
//! transport adapts.
//!
//! [`ControlApi`] is implemented once (by the daemon's session table, host
//! side) and adapted thinly: the axum HTTP+WS surface ([`http`]), the tonic
//! gRPC surface (feature `control-grpc`), and the CLI's [`client`]. Auth is
//! NOT this trait's job — adapters resolve the caller's [`auth::AuthCtx`]
//! ([`auth`]) and check [`thegn_core::control::required_scope`] *before*
//! calling in, so a rejected request performs no action.
//!
//! Methods return [`BoxFuture`]s (not native `async fn`) so the trait stays
//! dyn-compatible — adapters hold an `Arc<dyn ControlApi>`.

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use thegn_core::control::Scope;
pub use thegn_core::control_error::ControlErrorCode;
use thegn_core::control_wire::{EventFrame, PairingState};
pub use thegn_core::plugin_status::PluginStatusRow;
use thegn_core::store::LeaseRow;

/// The `agent.sessions` response row — re-exported from the harness seam so the
/// control-wire snapshot pins it alongside the other v1 wire types.
pub use thegn_core::harness::SessionRecord;

pub mod auth;
pub mod client;
#[cfg(feature = "control-grpc")]
pub mod grpc;
pub mod http;
pub mod http_ci;
pub mod http_lead;
#[cfg(feature = "iroh")]
pub mod iroh;
pub mod open_fields;
pub mod route_docs;
pub mod route_types;
pub mod routes;
#[cfg(test)]
mod tests;

/// One worktree registered with thegn (the `worktrees.list` capability). A
/// wire type, not the DB row: clients see what they can act on, not sort
/// keys and tab names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorktreeInfo {
    /// Absolute path (the session `worktree` hint / `open_worktree` argument).
    pub path: String,
    pub branch: String,
    pub repo_root: String,
    /// Remote-location descriptor (JSON) for a remote worktree; empty = local.
    #[serde(default)]
    pub location: String,
    /// Unix seconds (the DB's `created_at`).
    pub created_at: i64,
}

/// Metadata-only description of one embedded or configured skill. The control
/// API never returns target paths and never exposes a seed operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub harnesses: Vec<String>,
    pub gate: String,
    pub when: Vec<String>,
    pub source: thegn_core::skills::SkillSource,
}

/// The bounded registry projection returned by `GET /v1/skills`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SkillsList {
    pub skills: Vec<SkillInfo>,
    pub diagnostics: Vec<String>,
}

/// A safe editor-handoff request (`editor.open`).
///
/// The caller selects only a worktree and an optional relative file location;
/// provider choice, executable argv and environment stay local to the owning
/// compositor. [`EditorOpenRequest::target`] applies the core target policy at
/// every transport boundary before the request reaches [`ControlApi`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditorOpenRequest {
    /// Absolute worktree path on the daemon/compositor host.
    pub worktree: String,
    /// File path relative to `worktree`; omitted to open the project itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 1-based line number; requires `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// 1-based column number; requires `path` and `line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col: Option<usize>,
}

impl EditorOpenRequest {
    /// Validate this wire request through the core editor target policy.
    pub fn target(
        &self,
    ) -> Result<thegn_core::editor::EditorTarget, thegn_core::editor::EditorError> {
        thegn_core::editor::EditorTarget::new(
            &self.worktree,
            self.path.as_deref(),
            self.line,
            self.col,
        )
    }
}

/// One daemon-owned session (= one PTY + emulator). The compositor's tab/pane
/// layout stays client-side; the daemon's registry is flat.
///
/// `Default` is for construction sites (mostly tests) that care about a few
/// fields — see the note on [`OpenSpec`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionInfo {
    pub id: String,
    /// Worktree hint (path) when the session was opened for one.
    pub worktree: Option<String>,
    pub program: String,
    pub cwd: Option<String>,
    pub rows: u16,
    pub cols: u16,
    pub created_at_ms: i64,
    pub attached_clients: u32,
    /// Whether the live session currently has a harness-level error banner.
    /// This is included in session listings so a new event subscriber can
    /// bootstrap its cache before consuming activity deltas.
    #[serde(default)]
    pub error_active: bool,
    /// Set while a relay lease is keeping this detached session warm.
    pub lease_expires_at: Option<i64>,
    /// The PTY child's pid on the daemon's host. A same-host compositor uses
    /// it for `/proc`-based cwd/foreground-command capture at persist time.
    #[serde(default)]
    pub pid: Option<u32>,
    /// Set when this row is a session that has **finished** and is still being
    /// held readable (see the daemon's tombstones). `None` means live.
    ///
    /// A supervisor polling its fleet needs "which of my workers are done" to
    /// be answerable in one call, and a worker that exited thirty seconds ago
    /// must not simply vanish from the roster — that is the same lost-result
    /// race the tombstones exist to close, one level up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited_at_ms: Option<i64>,
    /// The finished child's exit code, when it exited and could be reaped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The agent state this session held at the moment it finished
    /// (`blocked` · `working` · `done` · `idle`). `None` for a live session —
    /// ask `wait`, or read the `Activity` feed, for that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_state: Option<String>,
    /// Set while (or just after) this session is being recorded as an
    /// asciicast: the on-disk path of the `.cast` file. The API returns the
    /// path so a client can audit and locate the recording — never its
    /// contents. `None` when nothing is being recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<String>,
    /// Stable display form of the source session when this session was forked.
    /// This is lineage metadata only; no launch recipe is included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// Unix seconds when the session last produced PTY output. `None` when it
    /// has produced none yet, or when the daemon predates this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_unix_secs: Option<i64>,
    /// Security-relevant `OpenSpec` fields the daemon applied to this session's
    /// spawn (the open reply only). Empty, and so never serialised, until a
    /// field's owner fills it; see [`open_fields`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub honoured: Vec<String>,
}

/// What to run when opening a fresh session.
///
/// `Default` exists so callers can spread `..Default::default()` and pick up
/// new optional fields without a construction-site sweep — the three added for
/// agent launch cost exactly that sweep, and there will be more.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OpenSpec {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    /// Worktree this session belongs to (listing/grouping hint).
    #[serde(default)]
    pub worktree: Option<String>,
    /// Launch a configured agent instead of a raw `argv`.
    ///
    /// With this set the daemon resolves the command, the sandbox and the
    /// environment itself — the same composition an interactive pane gets — so
    /// a caller does not have to know how to build any of it. `argv` is then
    /// ignored and may be empty.
    #[serde(default)]
    pub agent: Option<AgentLaunch>,
    /// Also file an `adopt_session` intent, so a running compositor grafts this
    /// session into a real pane instead of leaving it headless.
    #[serde(default)]
    pub adopt: bool,
    /// The caller already CPU-capped `argv`.
    ///
    /// The compositor does: it wraps via `sandbox::enter_argv` before building
    /// this spec, so the daemon must not wrap a second time. Everyone else
    /// leaves this `false` and gets the cap applied for them.
    #[serde(default)]
    pub already_capped: bool,
    /// Automation ancestry for sessions opened by a rule. Daemons preserve
    /// this on activity/exit facts so generated sessions cannot re-trigger
    /// their own rule chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_origin: Option<AutomationOrigin>,
}

/// Who is asking for an MCP proxy profile's env (`mcp_proxy.profile_env`,
/// THE-2222). Built by the transport from the verified token, never from the
/// request: `lead_label` is the Lead token's pairing label when the caller is
/// Lead-confined; `admin` is an admin-scoped or implicit-admin caller.
#[derive(Debug, Clone)]
pub struct ProfileEnvCaller {
    pub lead_label: Option<String>,
    pub admin: bool,
}

/// The authenticated Lead caller behind a [`ControlApi::open_confined`] call,
/// for the audit record. Built by the transport from the verified token,
/// never from the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadCaller {
    /// The token's public id.
    pub pairing_id: String,
    /// The pairing label (`lead` / `lead:<group>/<tab>`).
    pub label: String,
}

/// The parts of an [`OpenSpec`] the Lead confinement
/// ([`thegn_core::lead_confine::check`]) inspects. Shared by the HTTP
/// transport and the daemon's own re-check, so both judge the same fields.
pub fn lead_open_shape(spec: &OpenSpec) -> thegn_core::lead_confine::OpenShape<'_> {
    use thegn_core::lead_confine::{AgentShape, OpenShape};
    OpenShape {
        argv_len: spec.argv.len(),
        env_len: spec.env.len(),
        cwd: spec.cwd.as_deref(),
        worktree: spec.worktree.as_deref(),
        agent: spec.agent.as_ref().map(|a| AgentShape {
            name: a.agent.as_str(),
            prompt_len: a.prompt.len(),
            widen_permissions: a.widen_permissions,
            fork: a.fork,
            native_session_id: a.native_session_id.is_some(),
        }),
        already_capped: spec.already_capped,
        automation_origin: spec.automation_origin.is_some(),
    }
}

impl OpenSpec {
    /// The security-relevant fields this spec sets, which the daemon must echo
    /// in the open reply's `honoured` ([`open_fields`], WS00-P1-11). Always
    /// empty until the first such field lands; its owner (WS02-P4-15,
    /// WS04-P2-09, WS09-P5-05) adds the mapping here.
    pub fn security_fields(&self) -> Vec<open_fields::OpenField> {
        Vec::new()
    }
}

/// Largest PTY row count the daemon accepts: an upper bound on any PTY
/// geometry the daemon accepts; the emulator allocates rows x cols cells.
pub const MAX_PTY_ROWS: u16 = 1000;

/// Largest PTY column count the daemon accepts: an upper bound on any PTY
/// geometry the daemon accepts; the emulator allocates rows x cols cells.
pub const MAX_PTY_COLS: u16 = 1000;

/// Refuse a PTY geometry over [`MAX_PTY_ROWS`] x [`MAX_PTY_COLS`].
///
/// Only the upper end is bounded: zero keeps its existing meaning (`open`
/// raises it to 1; `attach` treats it as "no size").
pub fn check_pty_size(rows: u16, cols: u16) -> Result<(), ControlError> {
    if rows > MAX_PTY_ROWS || cols > MAX_PTY_COLS {
        return Err(ControlError::InvalidArgument(format!(
            "terminal size {cols}x{rows} exceeds the {MAX_PTY_COLS}x{MAX_PTY_ROWS} limit"
        )));
    }
    Ok(())
}

/// Launch a configured agent in a worktree.
///
/// The point of this type is that a supervisor should say *what it wants*, not
/// reconstruct how thegn launches things. Given an `[[agents]]` name it gets the
/// worktree's sandbox, the bundle/identity environment, the provider credential
/// directory (`CLAUDE_CONFIG_DIR` and friends), the resource cap, and the
/// `worktrees.agent` binding — all the machinery an interactive pane already
/// goes through, none of which a raw `argv` can reach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentLaunch {
    /// An `[[agents]]`/`[[tools]]` name, or — when no entry is named that — a
    /// provider id from thegn's provider registry (`claude`, `codex`), so a
    /// supervisor need not know the operator's entry names. Anything else is
    /// an error rather than a guess.
    pub agent: String,
    /// The task to seed the first turn with. Empty ⇒ launch interactively.
    #[serde(default)]
    pub prompt: String,
    /// Run headlessly (`claude -p …`) rather than as an interactive TUI.
    /// Defaults to headless exactly when a prompt is given.
    #[serde(default)]
    pub headless: Option<bool>,
    /// Record this agent as the worktree's own (`worktrees.agent`), so
    /// resurrection relaunches it and the sidebar attributes its activity.
    #[serde(default)]
    pub bind_worktree: bool,
    /// Resume a prior harness session by id instead of launching cold. The id
    /// is untrusted input (it crosses MCP/HTTP/CLI): it is validated against the
    /// harness's discovered-id shape and refused if it fails, never interpolated
    /// raw. A harness without resume support, or an empty id, launches normally.
    /// See [`thegn_core::harness`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// Continue the harness's own most recent session in the worktree — the
    /// id-free relaunch form the transport-retry observer uses (THE-86).
    /// Resolved through [`thegn_core::harness::Harness::continue_command`]; a
    /// harness without a continue form refuses, exactly like an unsupported
    /// `resume`. Wire-additive with a `false` default: older clients are
    /// unaffected.
    #[serde(default)]
    pub continue_last: bool,
    /// A `[[pipeline.stages]]` name whose `model` / `env` / `permissions`
    /// overrides are layered over the agent entry for this launch — how one
    /// entry runs a cheap tier for coders and a strong one for reviewers.
    /// Unknown stage ⇒ error. See `thegn_core::agent_task::effective_agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// Per-launch permission grants over the agent entry's configured grants.
    /// Claude only; by default this can narrow the configured grants, while
    /// `widen_permissions` permits a wider launch-specific set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<String>,
    /// Allow `permissions` to widen the configured grants for this launch.
    #[serde(default)]
    pub widen_permissions: bool,
    /// Ask the selected harness to create a native child session rather than
    /// resume the source in place. The daemon validates and resolves this
    /// operation through the harness seam.
    #[serde(default)]
    pub fork: bool,
    /// Native session id passed to the harness fork operation. It is kept
    /// separate from `resume` so a fork cannot silently become an in-place
    /// resume when the two launch forms are composed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
}

/// Intent for creating a new session from a live daemon or recorded harness
/// session. `harness` discriminates the source: absent means `session` is a
/// live daemon id; present means it is a native id discovered from
/// `agent.sessions`. No process recipe, environment, prompt, or transcript
/// data crosses the control boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForkSpec {
    /// Live daemon id, or native harness id when `harness` is set.
    pub session: String,
    /// Harness id for a recorded native session source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Configured agent name used to resolve the child launch context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Optional child working-directory override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Optional child worktree override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// Request a bounded plain-text scrollback handoff file.
    #[serde(default)]
    pub scrollback: bool,
    /// Ask a connected compositor to adopt the child.
    #[serde(default)]
    pub adopt: bool,
    /// Adopt the child in a new tab instead of beside the source.
    #[serde(default)]
    pub tab: bool,
}

/// How a client attaches. `Observer` never resizes the PTY and never holds the
/// relay lease open (read-mostly thin clients); `Interactive` is the
/// compositor/CLI case — last interactive writer wins resizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AttachKind {
    Interactive,
    Observer,
}

/// A successful attach: the warm snapshot, then the live per-subscriber frame
/// stream (bounded; a lagging subscriber gets a fresh
/// [`EventFrame::PaneSnapshot`] resync instead of blocking the PTY).
pub struct AttachReply {
    /// [`EventFrame::PaneSnapshot`] of the current screen — apply first.
    pub snapshot: EventFrame,
    /// Live frames from the snapshot's `seq + 1` on (deltas, resyncs, exit is
    /// signaled by the channel closing after a `Lease`/exit event).
    pub frames: tokio::sync::mpsc::Receiver<EventFrame>,
}

/// A bounded, credential-free preview fetch request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewFetchRequest {
    /// Absolute `http`/`https` URL. The host applies its loopback policy before
    /// connecting and again after every redirect.
    pub url: String,
    /// Optional worktree identity used only to select pane diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// Include bounded, redacted dev-server pane error lines. This is not a
    /// browser JavaScript-console claim.
    #[serde(default)]
    pub include_console: bool,
}

/// The bounded, JSON-safe result of [`PreviewFetchRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewFetchReply {
    /// Final URL after validated redirects.
    pub url: String,
    /// Origin server HTTP status, preserved as data (including non-2xx).
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Lossy UTF-8 rendering of at most `[preview] max_body_bytes` bytes.
    pub body: String,
    pub truncated: bool,
    /// Bounded, redacted error-shaped lines from the associated dev-server pane.
    #[serde(default)]
    pub console_errors: Vec<String>,
    /// `dev-server-pane` when those diagnostics were available, otherwise
    /// `unavailable`.
    pub diagnostics_source: String,
}

/// The payload of an [`EventFrame::Activity`] frame: one session's agent state
/// changed.
///
/// Emitted on **transition only**, never per observation, so a fleet of
/// chattering agents does not flood the feed. `state` is the four-word
/// vocabulary a supervisor actually reasons about (`blocked · working · done ·
/// idle`); `activity` is the underlying FSM state, exposed because the sidebar's
/// dots speak it and a client correlating the two should not have to guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionActivityEvent {
    pub session: String,
    /// Worktree owning this session, when it is attached to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// `blocked` | `working` | `done` | `idle`.
    pub state: String,
    /// `none` | `active` | `waiting` | `read`.
    pub activity: String,
    /// Unix ms at which this state was entered.
    pub since_ms: i64,
    /// The output sequence at the transition, so a client can order this
    /// against the `PaneDelta` frames it has applied.
    pub seq: u64,
    /// What the agent said when it raised its hand, for a `blocked` state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whether the live agent has emitted a harness failure banner that has
    /// not yet been cleared by resumed normal output. Drives the sidebar's
    /// `Failure` tier (with `AttentionReason::AgentFailed`) via the
    /// `AttentionInputs::agent_error_active` input the attention model
    /// already understands.
    ///
    /// `#[serde(default)]` so older clients (v1.0 alpha) can still decode new
    /// frames; the field defaults to `false` for them, which is exactly the
    /// pre-THE-89 behaviour (no error banner ⇒ no error state).
    #[serde(default)]
    pub error_active: bool,
}

/// A condition for the agent-driving `wait` verb: block until a session reaches
/// a state instead of polling it.
///
/// The activity-derived conditions (`Idle`/`Blocked`/`Done`) are the pane
/// daemon's per-session observer of the activity FSM, projected through
/// `thegn_core::attention::pane_agent_state`; `OutputMatches` scans the
/// session's ANSI-stripped scrollback. **A session that has exited resolves
/// every condition** as `exited` with its code rather than hanging or 404ing —
/// nothing ever waits on a corpse (see the daemon's tombstones).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WaitCondition {
    /// The session's PTY child exited.
    Exited,
    /// The agent went idle after being active (finished a turn).
    Idle,
    /// The agent is blocked waiting on the user (asked for input).
    Blocked,
    /// The agent reported done.
    Done,
    /// The session's output matched this regex.
    OutputMatches { regex: String },
}

/// The result of a `wait`: `matched=false` means the timeout elapsed first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WaitOutcome {
    pub matched: bool,
    /// Which condition fired (or the one waited on, if timed out).
    pub condition: String,
    /// The PTY exit code, when the condition was `Exited`.
    pub exit_code: Option<i32>,
}

/// One account whose `issues.list` result is missing or incomplete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueListGap {
    pub account: String,
    pub provider: String,
    /// `partial` (some rows returned) or `failed` (none).
    pub kind: String,
    pub reason: String,
}

/// The `issues.list` answer: rows plus every account that was not read completely.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueListing {
    pub issues: Vec<thegn_core::issue::Issue>,
    /// True only when every configured account answered completely.
    pub complete: bool,
    pub incomplete: Vec<IssueListGap>,
}

/// Where a `split` places the new pane relative to the target session. Mirrors
/// the compositor's `center::Dir`; the wire type lives here so `thegn-svc` does
/// not depend on `thegn-host`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum SplitDir {
    /// New pane to the right (vertical divider).
    #[default]
    Right,
    /// New pane below (horizontal divider).
    Down,
}

/// The `sessions.record` request: what to do with a session's recording. The
/// daemon owns the file, so a start returns the path but the caller never
/// receives contents over the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum RecordSpec {
    /// Begin recording this session's PTY output to a fresh `.cast` file.
    #[default]
    Start,
    /// Stop and finalize the current recording.
    Stop,
    /// Report the recording state without changing it.
    Status,
}

/// The `sessions.record` response: the recording state after the operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecordStatus {
    /// Whether a recording is currently active.
    pub recording: bool,
    /// The on-disk path of the (active or just-finalized) `.cast` file, if any.
    /// A path is a locator for audit — the contents are never returned.
    pub path: Option<String>,
    /// Bytes written to the cast file so far.
    pub bytes: u64,
    /// Set when the recording stopped because it reached `[recording] max_bytes`
    /// (the file is finalized and valid; the session was unaffected).
    pub capped: bool,
    /// `Some(reason)` when the recording could NOT be finalized cleanly (the
    /// final write or flush failed — a full disk, a quota). The `.cast` at
    /// `path` is short of the session's last output, so a client must report it
    /// as truncated rather than saved. `None` on every healthy path.
    pub truncated: Option<String>,
    /// PTY output bytes dropped under recording overload. Each gap has an `m`
    /// marker in the cast; non-zero means the file is valid but incomplete.
    #[serde(default)]
    pub dropped_bytes: u64,
    /// Admission is closed and the worker has not reported completion; the file is not yet final.
    #[serde(default)]
    pub stopping: bool,
}

/// One changed file in a worktree (the mobile stage/commit contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitFileStatus {
    pub path: String,
    /// Porcelain-style two-letter code (`"M "`, `" M"`, `"??"`, …).
    pub code: String,
}

/// One worktree's cached PR facts (the `pr.status` verb). Projected from the
/// daemon's `pr_cache` table — a TTL'd read-through cache of the forge's
/// answer, so `fetched_at` tells the client how stale the row is (the forge
/// itself stays the source of truth).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PrStatusRow {
    /// Absolute worktree path the PR is cached for.
    pub worktree: String,
    /// The PR's head branch.
    pub branch: String,
    pub number: u64,
    pub title: String,
    /// Forge state word: `OPEN` | `CLOSED` | `MERGED`.
    pub state: String,
    pub url: String,
    #[serde(default)]
    pub is_draft: bool,
    /// Unix seconds the cache row was fetched at.
    pub fetched_at: i64,
}

/// Cached CI run history projected through the control plane. `runs` is kept
/// as JSON because the provider-neutral CI model intentionally belongs to
/// `thegn-core` without a control-wire schema dependency; the response still
/// carries its source (`cache`, `provider`, or `stale-cache`) and timestamp
/// explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CiRunsReply {
    pub worktree: String,
    pub runs: serde_json::Value,
    /// Malformed rows discarded from provider output or cached values.
    #[serde(default)]
    pub discarded_rows: usize,
    pub source: String,
    pub fetched_at: i64,
    /// Why a cached list was served after a failed or incomplete provider read (`source == "stale-cache"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
}

/// One cache-first CI log projection. Entries contain only the bounded,
/// redacted cache shape; the response metadata makes a provider fill explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CiLogsReply {
    pub worktree: String,
    pub run_id: String,
    pub job_id: String,
    pub logs: Vec<serde_json::Value>,
    pub source: String,
    pub fetched_at: i64,
    pub truncated: bool,
    pub redacted: bool,
    /// Whether the redactor changed the served entry text: Some(true) when redactor output is present, Some(false) when it found nothing to change, null for a row cached before schema v103. Metadata only, not proof that every secret shape was recognised.
    #[serde(default)]
    pub redaction_changed: Option<bool>,
    /// Provider-fill problems with the run detail (unavailable, incomplete job list, unlisted job); empty on cache hits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// The `notify.push` verb payload — a desktop-notification-shaped note pushed
/// into the tray over the API (the wire mirror of `thegn notify push`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PushedNote {
    /// Short summary line (the inbox message when `body` is empty).
    pub title: String,
    /// Longer detail, appended to the inbox message when non-empty.
    #[serde(default)]
    pub body: String,
    /// `"alert"`/`"critical"` raise the red-flag priority; anything else (or
    /// absent) lands at the normal notice tier.
    #[serde(default)]
    pub urgency: Option<String>,
    /// Opaque source reference stored on the row; defaults to `"api"`.
    #[serde(default)]
    pub source: Option<String>,
    /// Automation ancestry for loop prevention; absent for human/API pushes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_origin: Option<AutomationOrigin>,
}

/// Wire-safe automation ancestry shared by generated sessions and notes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AutomationOrigin {
    pub root_event_id: String,
    pub rule_id: String,
    pub run_id: String,
}

/// One trusted automation rule projected for control clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AutomationRuleInfo {
    pub name: String,
    pub enabled: bool,
    pub trusted_layer: String,
    pub event: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inert_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_outcome: Option<String>,
}

/// Pure `automations.test` input. `event` is decoded as the core normalized
/// event by the implementation so the transport schema stays substrate-free.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AutomationTestRequest {
    pub rule: String,
    pub event: serde_json::Value,
    #[serde(default)]
    pub at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AutomationTestReply {
    pub rule: String,
    pub decisions: serde_json::Value,
    pub executed: bool,
}

/// Name-only configured-tool request. The daemon resolves the command,
/// sandbox, environment, and resource limits from its trusted config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolRunRequest {
    pub name: String,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_origin: Option<AutomationOrigin>,
}

/// One upstream instance in the mcp-proxy hub, as `mcp_proxy.status` reports.
/// Never carries a secret value — env refs are named by their pointer only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpProxyUpstreamStatus {
    /// The `[mcp_servers.<name>]` key.
    pub name: String,
    /// Partition key of this instance (`global` / `workspace:<w>` / …).
    pub partition_key: String,
    /// Declared scope (`global` | `workspace` | `worktree`).
    pub scope: String,
    /// Whether an upstream process is currently running for this instance.
    pub running: bool,
    /// Breaker state: `closed` | `open` | `half_open`.
    pub breaker: String,
    /// How long ago (ms) the last health check ran, if any.
    #[serde(default)]
    pub health_checked_ms_ago: Option<i64>,
    /// Count of tools this upstream exposes through the proxy (post-filter).
    pub exposed_tools: usize,
    /// Count of the upstream's tools hidden by the default-deny filter.
    pub hidden_tools: usize,
    /// The exposed tools' original names (for `mcp list` / doctor).
    #[serde(default)]
    pub exposed_names: Vec<String>,
    /// Set when this upstream is withheld from the reporting context (e.g. a
    /// scoped upstream and no worktree context) — the inspectable reason.
    #[serde(default)]
    pub withheld_reason: Option<String>,
}

/// The `mcp_proxy.status` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpProxyStatus {
    /// `[mcp_proxy] enabled`.
    pub enabled: bool,
    /// Whether the daemon owns shared upstream processes (vs. per-shim
    /// in-process fallback).
    pub daemon_owned: bool,
    pub upstreams: Vec<McpProxyUpstreamStatus>,
}

/// The `plugins.status` payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginsStatusReply {
    /// A valid snapshot file was found.
    pub published: bool,
    /// The process that wrote it (`pid`) is still running.
    pub live: bool,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub published_at_ms: Option<i64>,
    #[serde(default)]
    pub rows: Vec<PluginStatusRow>,
}

/// One reconcile action taken by `mcp_proxy.reload`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpProxyReloadAction {
    /// `start` | `stop` | `restart` | `refilter`.
    pub kind: String,
    pub upstream: String,
    pub partition_key: String,
}

/// The `mcp_proxy.reload` payload — what reconciling the config did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpProxyReloadReport {
    pub actions: Vec<McpProxyReloadAction>,
    /// Whether the advertised tool set changed (⇒ `notifications/tools/
    /// list_changed` was emitted to connected agents).
    pub tools_changed: bool,
}

/// The `worktrees.create` verb payload (THE-57). Creates a worktree, optionally
/// from a tracker issue — deriving the branch from the issue's provider hint and
/// linking the issue to the new worktree — the headless twin of the `D` key's
/// dispatch pipeline, sharing the same branch-derivation rule so the two cannot
/// drift.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorktreeCreateReq {
    /// A path inside the repo to anchor to (any worktree of it). Empty ⇒ the
    /// daemon resolves the repo from its own cwd.
    #[serde(default)]
    pub repo: Option<String>,
    /// Tracker issue id (`"<provider>:<key>"`). When given and `branch` is
    /// empty, the branch derives from the issue's `branch_hint` (naming fallback
    /// otherwise) and the issue is linked to the new worktree.
    #[serde(default)]
    pub issue: Option<String>,
    /// Explicit branch name. Overrides issue-hint derivation when set.
    #[serde(default)]
    pub branch: Option<String>,
}

/// Assign or clear a worktree's repo-local sidebar folder (`folders.assign`).
/// A name is resolved within the worktree's own repository. Clearing is always
/// explicit (`clear: true`) and never deletes the folder; supplying neither or
/// both of `folder` / `clear` is an invalid argument, so a malformed request can
/// never silently unfile a worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FolderAssignReq {
    /// Absolute path to a Git worktree.
    pub worktree: String,
    /// Folder name to assign. Mutually exclusive with `clear`.
    #[serde(default)]
    pub folder: Option<String>,
    /// `true` to unfile the worktree (the folder itself is kept). Mutually
    /// exclusive with `folder`.
    #[serde(default)]
    pub clear: bool,
}

/// Query for `worktrees.sandbox_profile`: which worktree's pin to read. The
/// path is matched exactly as registered (no canonicalization).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxProfileQuery {
    /// Absolute path of a registered worktree, exactly as registered.
    pub worktree: String,
}

/// Pin or clear a worktree's sandbox profile (`worktrees.sandbox_profile_set`).
/// Clearing is always explicit (`clear: true`), and unknown keys are refused, so
/// a misspelled `profile` can never silently clear a pin. The path is matched
/// exactly as registered (no canonicalization).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxProfileSetReq {
    /// Absolute path of a registered worktree, exactly as registered.
    pub worktree: String,
    /// `standard` or `sealed`. Mutually exclusive with `clear`.
    #[serde(default)]
    pub profile: Option<String>,
    /// `true` to clear the pin (the configured policy applies again). Mutually
    /// exclusive with `profile`.
    #[serde(default)]
    pub clear: bool,
}

impl SandboxProfileSetReq {
    /// Exactly one of `{profile: Some, clear: true}` is valid. `Ok(Some(p))`
    /// pins `p`, `Ok(None)` clears.
    pub fn action(&self) -> ControlResult<Option<&str>> {
        match (self.profile.as_deref(), self.clear) {
            (Some(profile), false) => Ok(Some(profile)),
            (None, true) => Ok(None),
            (Some(_), true) => Err(ControlError::InvalidArgument(
                "worktrees.sandbox_profile_set takes either `profile` or `clear: true`, not both"
                    .into(),
            )),
            (None, false) => Err(ControlError::InvalidArgument(
                "worktrees.sandbox_profile_set needs `profile` (pin) or `clear: true` (clear)"
                    .into(),
            )),
        }
    }
}

/// A worktree's pinned sandbox profile; `profile` is null when nothing is
/// pinned (the configured policy applies).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SandboxProfileReply {
    /// The worktree path the pin belongs to.
    pub worktree: String,
    /// The pinned profile's wire name, or null when inheriting.
    pub profile: Option<String>,
}

/// The validated intent of a [`FolderAssignReq`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderAction<'a> {
    Assign(&'a str),
    Clear,
}

impl FolderAssignReq {
    /// Exactly one of `{folder: Some, clear: true}` is valid.
    pub fn action(&self) -> ControlResult<FolderAction<'_>> {
        match (self.folder.as_deref(), self.clear) {
            (Some(name), false) => Ok(FolderAction::Assign(name)),
            (None, true) => Ok(FolderAction::Clear),
            (Some(_), true) => Err(ControlError::InvalidArgument(
                "folders.assign takes either `folder` or `clear: true`, not both".into(),
            )),
            (None, false) => Err(ControlError::InvalidArgument(
                "folders.assign needs `folder` (assign) or `clear: true` (unfile)".into(),
            )),
        }
    }
}

/// A Lead-lease proof presented with a pipeline mutation (THE-208). Optional
/// on the wire; absent means no proof, so a lease-bound row is refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DispatchLeadReq {
    pub lease: String,
    pub owner: String,
    /// The generation returned by `thegn dispatch lease acquire`; when present the proof is fenced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_gen: Option<i64>,
}

impl DispatchLeadReq {
    pub fn proof(&self) -> thegn_core::db::LeadProof<'_> {
        match self.lease_gen {
            Some(generation) => thegn_core::db::LeadProof::Fenced {
                name: &self.lease,
                owner: &self.owner,
                generation,
            },
            None => thegn_core::db::LeadProof::Lease {
                name: &self.lease,
                owner: &self.owner,
            },
        }
    }
}

/// The `dispatches.put` verb payload (THE-57): one row appended to the roster.
///
/// The four pipeline fields (v56) are optional and default-absent, so a caller
/// written against the three-string version keeps working unchanged. `put`
/// carries them all: the roster gains columns, not verbs — there is deliberately
/// no `dispatches.update`, because a mutable stage field invites thegn to become
/// the thing that advances it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DispatchPutReq {
    pub issue_id: String,
    pub worktree_path: String,
    pub agent_name: String,
    /// Which `[[pipeline.stages]]` step this row is. Stored and grouped by;
    /// never advanced by thegn.
    #[serde(default)]
    pub stage: Option<String>,
    /// The roster row this one was chunked out of (architect → coder fan-out).
    #[serde(default)]
    pub parent_id: Option<i64>,
    /// The daemon session running this dispatch — the row's identity for
    /// pane-exit attribution when several stages share one worktree.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Path to the handoff artifact committed in the worktree. A pointer, never
    /// the payload.
    #[serde(default)]
    pub artifact_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead: Option<DispatchLeadReq>,
}

/// One newest-first page of the dispatch roster (`dispatches.list`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchPage {
    /// Dispatch rows in newest-first order.
    pub dispatches: Vec<thegn_core::issue::AgentDispatch>,
    /// Whether older rows exist beyond this page.
    #[serde(default)]
    pub truncated: bool,
    /// Pass this row id as `before` to read older rows; set only when truncated.
    #[serde(default)]
    pub next_before: Option<i64>,
}

/// A capability-authenticated worker write. The token is never included in
/// formatted diagnostics.
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DispatchWorkerWriteReq {
    pub text: String,
    pub token: String,
}

impl std::fmt::Debug for DispatchWorkerWriteReq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchWorkerWriteReq")
            .field("text", &self.text)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DispatchReportReply {
    pub id: i64,
    pub bytes: usize,
    pub pass_without_gate_evidence: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DispatchNoteReply {
    pub id: i64,
    pub note_id: i64,
    pub created_at_ms: i64,
}

/// Why a control call failed. Adapters map these to transport status codes
/// (HTTP 404/403/409/501/500; gRPC NotFound/PermissionDenied/…).
#[derive(Debug)]
pub enum ControlError {
    NotFound(String),
    InvalidArgument(String),
    /// The caller's token lacks the required scope. Produced by adapters (the
    /// trait impl never sees an under-scoped call).
    NoScope {
        need: Scope,
    },
    Conflict(String),
    FailedPrecondition(String),
    ResourceExhausted(String),
    Unavailable(String),
    Unimplemented(&'static str),
    Internal(anyhow::Error),
}

impl ControlError {
    /// The stable machine-readable code for this control error.
    pub fn code(&self) -> ControlErrorCode {
        match self {
            Self::NotFound(_) => ControlErrorCode::NotFound,
            // `InvalidArgument` reuses the existing `BadRequest` identifier
            // rather than adding a synonym for the same condition.
            Self::InvalidArgument(_) => ControlErrorCode::BadRequest,
            Self::NoScope { .. } => ControlErrorCode::NoScope,
            Self::Conflict(_) => ControlErrorCode::Conflict,
            Self::FailedPrecondition(_) => ControlErrorCode::FailedPrecondition,
            Self::ResourceExhausted(_) => ControlErrorCode::ResourceExhausted,
            Self::Unavailable(_) => ControlErrorCode::Unavailable,
            Self::Unimplemented(_) => ControlErrorCode::Unimplemented,
            Self::Internal(_) => ControlErrorCode::Internal,
        }
    }
}

/// The additive HTTP error envelope. `error` retains the existing human
/// message; `code` is stable for machine-readable branching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ErrorBody {
    pub error: String,
    pub code: ControlErrorCode,
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::NotFound(what) => write!(f, "not found: {what}"),
            ControlError::InvalidArgument(what) => write!(f, "invalid argument: {what}"),
            ControlError::NoScope { need } => {
                write!(f, "missing required scope: {}", need.as_str())
            }
            ControlError::Conflict(what) => write!(f, "conflict: {what}"),
            ControlError::FailedPrecondition(what) => {
                write!(f, "failed precondition: {what}")
            }
            ControlError::ResourceExhausted(what) => {
                write!(f, "resource exhausted: {what}")
            }
            ControlError::Unavailable(what) => write!(f, "unavailable: {what}"),
            ControlError::Unimplemented(what) => write!(f, "not implemented: {what}"),
            ControlError::Internal(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ControlError {}

impl From<anyhow::Error> for ControlError {
    fn from(e: anyhow::Error) -> Self {
        ControlError::Internal(e)
    }
}

pub type ControlResult<T> = Result<T, ControlError>;

/// Calendar event occurrences and cache-quality metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CalendarEventsReply {
    pub events: Vec<thegn_core::calendar::CalEvent>,
    pub skipped_rows: u32,
    pub incomplete: bool,
}

/// The service trait. One impl (the daemon), many thin adapters.
///
/// Everything here is async-off-the-render-loop by construction: impls run on
/// the daemon's tokio runtime; the compositor only ever consumes results
/// through its mpsc + `TerminalWaker` path.
pub trait ControlApi: Send + Sync + 'static {
    fn list_sessions(&self) -> BoxFuture<'_, ControlResult<Vec<SessionInfo>>>;

    /// Read-only provider fleet report (`fleet.list`). The payload is the
    /// existing CLI JSON report so the control surface cannot drift models.
    fn fleet_list(&self) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "fleet listing is not available",
            ))
        })
    }

    /// Read-only remote sandbox health of one worktree (`env.status`): the
    /// existing `thegn env status --json` report, so the control surface cannot
    /// drift from the CLI model.
    fn env_status<'a>(
        &'a self,
        _worktree: &'a str,
    ) -> BoxFuture<'a, ControlResult<serde_json::Value>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "remote sandbox health is not available",
            ))
        })
    }

    /// Worktrees registered with thegn (`worktrees.list`). Defaulted like the
    /// calendar verbs so transport-only impls and test fakes need no wiring.
    fn list_worktrees(&self) -> BoxFuture<'_, ControlResult<Vec<WorktreeInfo>>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "worktree listing is not available",
            ))
        })
    }

    /// Embedded and configured skill metadata (`skills.list`). Read-only and
    /// path-free; seeding remains a local CLI operation.
    fn list_skills(&self) -> BoxFuture<'_, ControlResult<SkillsList>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "skill listing is not available",
            ))
        })
    }

    /// Open a fresh session (a PTY running `spec.argv`).
    fn open(&self, spec: OpenSpec) -> BoxFuture<'_, ControlResult<SessionInfo>>;

    /// Open a session for a Lead-confined caller ([`thegn_core::lead_confine`]).
    /// The transport has already checked the request's shape (a configured
    /// agent by name, no argv/env); the impl must also refuse a worktree that
    /// is not registered and a launch that does not resolve to a contained
    /// sandbox backend ([`thegn_core::lead_confine::check_resolved_open`]),
    /// auditing the refusal. The default refuses: an impl that cannot prove
    /// both launches nothing for the Lead.
    fn open_confined(
        &self,
        _spec: OpenSpec,
        _caller: LeadCaller,
    ) -> BoxFuture<'_, ControlResult<SessionInfo>> {
        Box::pin(async {
            Err(ControlError::FailedPrecondition(format!(
                "{}this daemon cannot verify a Lead worker's sandbox",
                thegn_core::lead_confine::REFUSAL_PREFIX
            )))
        })
    }

    /// Fork a live daemon or recorded harness session. The default keeps
    /// transport-only adapters honest until they implement the spawn owner.
    fn fork(&self, _spec: ForkSpec) -> BoxFuture<'_, ControlResult<SessionInfo>> {
        Box::pin(async { Err(ControlError::Unimplemented("session fork is not available")) })
    }

    /// Warm-attach: registers `client_id` as a subscriber and returns the
    /// current screen snapshot + live stream. An `Interactive` attach cancels
    /// any relay lease; an `Observer` never touches it. `history` selects
    /// whether the snapshot carries the scrollback history tail — the first
    /// attach of a fresh client emulator wants it, a reconnect re-feeding an
    /// emulator that already holds the history must pass `false` or the tail
    /// duplicates in the client's scrollback.
    fn attach<'a>(
        &'a self,
        client_id: &'a str,
        session: &'a str,
        kind: AttachKind,
        rows: u16,
        cols: u16,
        history: bool,
    ) -> BoxFuture<'a, ControlResult<AttachReply>>;

    /// Detach without killing the PTY; the last client out opens a relay lease.
    fn detach<'a>(
        &'a self,
        client_id: &'a str,
        session: &'a str,
    ) -> BoxFuture<'a, ControlResult<()>>;

    fn send_input<'a>(
        &'a self,
        session: &'a str,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, ControlResult<()>>;

    /// `sessions.input` for a Lead-confined caller ([`thegn_core::lead_confine`]).
    /// The transport has already checked the bytes are plain text and Enter
    /// ([`thegn_core::lead_input::check`]); the impl must also prove, before
    /// writing a byte, that `session` is a live session a Lead opened (and
    /// still the same session), auditing both outcomes with the caller's label
    /// and the session id, never the text. The default refuses: an impl that
    /// does not record who opened a session types nothing for the Lead.
    fn send_input_confined<'a>(
        &'a self,
        session: &'a str,
        _bytes: Vec<u8>,
        _caller: LeadCaller,
    ) -> BoxFuture<'a, ControlResult<()>> {
        Box::pin(async move {
            Err(ControlError::FailedPrecondition(format!(
                "{}this daemon does not record who opened {session}",
                thegn_core::lead_confine::REFUSAL_PREFIX
            )))
        })
    }

    fn resize<'a>(
        &'a self,
        session: &'a str,
        rows: u16,
        cols: u16,
    ) -> BoxFuture<'a, ControlResult<()>>;

    /// One-shot screen snapshot ([`EventFrame::PaneSnapshot`]) without attaching.
    fn snapshot<'a>(&'a self, session: &'a str) -> BoxFuture<'a, ControlResult<EventFrame>>;

    /// Kill the session's PTY and drop it from the registry.
    fn kill<'a>(&'a self, session: &'a str) -> BoxFuture<'a, ControlResult<()>>;

    /// Open/focus a worktree in the owning instance (the `thegn open` verb).
    fn open_worktree<'a>(
        &'a self,
        repo: &'a str,
        branch: Option<&'a str>,
    ) -> BoxFuture<'a, ControlResult<()>>;

    /// Queue a validated editor target for the owning compositor. The daemon
    /// acknowledges the mailbox write only; it never resolves or launches an
    /// editor itself.
    fn open_editor(
        &self,
        _target: thegn_core::editor::EditorTarget,
    ) -> BoxFuture<'_, ControlResult<()>> {
        Box::pin(async { Err(ControlError::Unimplemented("open_editor")) })
    }

    /// Perform one bounded, credential-free HTTP GET for `preview.fetch`.
    fn preview_fetch(
        &self,
        _req: PreviewFetchRequest,
    ) -> BoxFuture<'_, ControlResult<PreviewFetchReply>> {
        Box::pin(async { Err(ControlError::Unimplemented("preview_fetch")) })
    }

    /// Block until `session` reaches `cond` (or `timeout_ms` elapses). The
    /// default answers `Unimplemented`; the daemon overrides it to implement the
    /// `Exited` condition off its event feed (no polling). Activity-derived
    /// conditions light up when the per-pane state feed lands.
    fn wait<'a>(
        &'a self,
        _session: &'a str,
        _cond: WaitCondition,
        _timeout_ms: Option<i64>,
    ) -> BoxFuture<'a, ControlResult<WaitOutcome>> {
        Box::pin(async { Err(ControlError::Unimplemented("wait")) })
    }

    /// Split `session`: create a sibling pane running `spec`. The default opens
    /// a sibling *session* (the daemon registry is flat), which the compositor
    /// places beside the target when it observes the new session; full in-layout
    /// placement over the API lands with server-side layout. `dir` is recorded
    /// for that future placement.
    fn split<'a>(
        &'a self,
        _session: &'a str,
        _dir: SplitDir,
        spec: OpenSpec,
    ) -> BoxFuture<'a, ControlResult<SessionInfo>> {
        self.open(spec)
    }

    /// Start/stop/query a daemon-side asciicast recording of `session`. The
    /// default answers `Unimplemented`; the daemon owns the recorder in the
    /// session actor so recording continues while every client is detached. The
    /// returned [`RecordStatus`] carries the file path and byte count — never
    /// the recorded contents.
    fn record_session<'a>(
        &'a self,
        _session: &'a str,
        _spec: RecordSpec,
    ) -> BoxFuture<'a, ControlResult<RecordStatus>> {
        Box::pin(async { Err(ControlError::Unimplemented("record_session")) })
    }

    // Git verbs (the mobile stage/commit contract) — impls route through the
    // GitBackend seam on spawn_blocking; git stays the source of truth.
    fn git_status<'a>(
        &'a self,
        worktree: &'a str,
    ) -> BoxFuture<'a, ControlResult<Vec<GitFileStatus>>>;

    fn git_stage<'a>(
        &'a self,
        worktree: &'a str,
        paths: &'a [String],
    ) -> BoxFuture<'a, ControlResult<()>>;

    /// Returns the new commit id.
    fn git_commit<'a>(
        &'a self,
        worktree: &'a str,
        message: &'a str,
    ) -> BoxFuture<'a, ControlResult<String>>;

    // Merge-queue verbs — add the worktree's branch / clear / list the queue for
    // the worktree's repo. Impls reuse the host `merge_ops` primitive so behavior
    // matches the CLI and MCP surfaces.
    /// Add the worktree's current branch; returns a status message.
    fn merge_add<'a>(&'a self, worktree: &'a str) -> BoxFuture<'a, ControlResult<String>>;

    /// Clear the queue for the worktree's repo; returns the number removed.
    fn merge_clear<'a>(&'a self, worktree: &'a str) -> BoxFuture<'a, ControlResult<usize>>;

    /// The queue rows for the worktree's repo.
    fn merge_list<'a>(
        &'a self,
        worktree: &'a str,
    ) -> BoxFuture<'a, ControlResult<Vec<thegn_core::db::MergeQueueRow>>>;

    // --- calendar -----------------------------------------------------------
    // The third plugin surface: a daemon-style integration that runs on its own
    // schedule can read the merged calendar and push its own events in, rather
    // than being polled as a subprocess.
    //
    // Defaulted to `Unsupported` so transport-only impls and test fakes need no
    // wiring, following the `publish_pairing` precedent.

    /// The merged, recurrence-expanded calendar over `[from, to]` (inclusive
    /// ISO dates), plus cache-quality metadata. `skipped_rows` counts rows
    /// that could not be decoded or expanded; `incomplete` is true when that
    /// count is nonzero.
    fn calendar_events<'a>(
        &'a self,
        _from: &'a str,
        _to: &'a str,
    ) -> BoxFuture<'a, ControlResult<CalendarEventsReply>> {
        Box::pin(async { Err(ControlError::Unimplemented("calendar is not configured")) })
    }

    /// The resolved world clocks, evaluated now.
    ///
    /// Returns an array of objects with `label`, `zone`, ISO-like `local`,
    /// popup-rendered `time`, `format` (`"h12"`, `"h24"`, or the custom
    /// strftime string), `show_date`, and timezone metadata including
    /// `utc_offset_secs`, `is_dst`, `abbrev`, `delta_from_home_mins`,
    /// `day_delta`, and `is_home`.
    fn calendar_clocks(&self) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("calendar is not configured")) })
    }

    /// Push events into one source's cache. Returns how many were stored.
    ///
    /// This is *ingest*, not a write to an upstream provider — thegn stays
    /// read-only towards those.
    fn calendar_ingest<'a>(
        &'a self,
        _account: &'a str,
        _events: Vec<thegn_core::calendar::CalEvent>,
    ) -> BoxFuture<'a, ControlResult<usize>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "calendar ingest is not enabled",
            ))
        })
    }

    /// Cached PR status, one row per worktree with a `pr_cache` entry (the
    /// `pr.status` verb). A cache read — the forge is the source of truth;
    /// each row's `fetched_at` carries its staleness.
    fn pr_status(&self) -> BoxFuture<'_, ControlResult<Vec<PrStatusRow>>>;

    /// Cached CI run history, filled from the provider only on a cache miss.
    fn ci_runs<'a>(
        &'a self,
        _worktree: &'a str,
        _limit: usize,
    ) -> BoxFuture<'a, ControlResult<CiRunsReply>> {
        Box::pin(async { Err(ControlError::Unimplemented("CI runs are not available")) })
    }

    /// One bounded CI job-log entry, filled from the provider only on a cache
    /// miss. `tail_lines` may narrow an already cached entry.
    fn ci_logs<'a>(
        &'a self,
        _worktree: &'a str,
        _run_id: &'a str,
        _job_id: &'a str,
        _tail_lines: Option<usize>,
    ) -> BoxFuture<'a, ControlResult<CiLogsReply>> {
        Box::pin(async { Err(ControlError::Unimplemented("CI logs are not available")) })
    }

    /// Push a notification into the tray (the `notify.push` verb). Returns
    /// the stored notification's row id.
    fn notify_push(&self, note: PushedNote) -> BoxFuture<'_, ControlResult<i64>>;

    fn automations_list(&self) -> BoxFuture<'_, ControlResult<Vec<AutomationRuleInfo>>> {
        Box::pin(async { Err(ControlError::Unimplemented("automations.list")) })
    }

    fn automations_test(
        &self,
        _request: AutomationTestRequest,
    ) -> BoxFuture<'_, ControlResult<AutomationTestReply>> {
        Box::pin(async { Err(ControlError::Unimplemented("automations.test")) })
    }

    fn tools_run(&self, _request: ToolRunRequest) -> BoxFuture<'_, ControlResult<SessionInfo>> {
        Box::pin(async { Err(ControlError::Unimplemented("tools.run")) })
    }

    // --- Lead tool surface (THE-2172) ---
    // JSON in, JSON out: each verb's typed DTOs live in its owning thegn-core
    // module, so the trait, the routes and MCP stay independent of behaviour.
    // Defaulted to `Unimplemented`; the daemon overrides each.

    /// The cross-workspace Lead overview (`overview.get`).
    fn overview_get(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("overview.get")) })
    }

    /// Headless producer arming, leases and per-account inbox state
    /// (`producers.status`, b163). The reply is a
    /// `thegn_core::producer_status::ProducersStatusReply`.
    fn producers_status(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("producers.status")) })
    }

    /// Filtered notification listing (`notifications.list`).
    fn notifications_list(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("notifications.list")) })
    }

    /// Acknowledge notifications (`notifications.ack`).
    fn notifications_ack(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("notifications.ack")) })
    }

    /// Create or replace a Lead-authored automation rule (`automations.put`).
    fn automations_put(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("automations.put")) })
    }

    /// Remove a Lead-authored automation rule (`automations.rm`).
    fn automations_rm(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("automations.rm")) })
    }

    /// Enable or disable a Lead-authored automation rule (`automations.enable`).
    fn automations_enable(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("automations.enable")) })
    }

    /// Recent audited automation runs (`automations.runs`).
    fn automations_runs(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("automations.runs")) })
    }

    /// Approval history and pending requests (`approvals.list`).
    fn approvals_list(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("approvals.list")) })
    }

    /// Ask a human to approve a pipeline stage (`approvals.request`).
    fn approvals_request(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("approvals.request")) })
    }

    /// Grant or deny a pending approval (`approvals.resolve`, Admin).
    fn approvals_resolve(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("approvals.resolve")) })
    }

    /// Start the merge-queue drain for a worktree's repo (`merge.drain`).
    fn merge_drain(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("merge.drain")) })
    }

    /// Pause every automation rule (`automations.pause`, THE-2197); replies
    /// `{paused: true}`.
    fn automations_pause(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("automations.pause")) })
    }

    /// Resume paused automations (`automations.resume`, Admin, THE-2197);
    /// replies `{paused: false}`.
    fn automations_resume(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("automations.resume")) })
    }

    // --- b155 Lead spine ------------------------------------------------------

    /// Answer an approval with a single-use push nonce (`approvals.redeem`,
    /// Write; HTTP + CLI; refused for the Lead). Body `{nonce, decision}`.
    fn approvals_redeem(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("approvals.redeem")) })
    }

    /// Kill automation-started and Lead-opened sessions (`automations.halt`,
    /// Admin). Body `{keep_sessions}`.
    fn automations_halt(
        &self,
        _req: serde_json::Value,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("automations.halt")) })
    }

    /// Resolve an MCP proxy profile's server env (`mcp_proxy.profile_env`,
    /// Read). Body `{profile}`. The daemon answers only the Lead token for its
    /// own `[lead] mcp_profile`, or an admin caller.
    fn mcp_proxy_profile_env(
        &self,
        _req: serde_json::Value,
        _caller: ProfileEnvCaller,
    ) -> BoxFuture<'_, ControlResult<serde_json::Value>> {
        Box::pin(async { Err(ControlError::Unimplemented("mcp_proxy.profile_env")) })
    }

    // --- agent orchestration (THE-57) ---------------------------------------
    // The supervisor's hands. Defaulted to `Unimplemented` so transport-only
    // impls and test fakes need no wiring (the calendar precedent); the daemon
    // overrides each. The issue verbs route through `IssueRouter` (the same
    // provider seam the panel uses); the dispatch verbs and `worktree_create`
    // are local DB / git, like the merge verbs.

    /// List tracker issues matching `filter` (`issues.list`); `complete` is false when any account
    /// was partial or failed.
    fn issues_list<'a>(
        &'a self,
        _filter: &'a thegn_core::issue::IssueFilter,
        _repo: Option<&'a str>,
    ) -> BoxFuture<'a, ControlResult<IssueListing>> {
        Box::pin(async { Err(ControlError::Unimplemented("no issue tracker configured")) })
    }

    /// Read one issue with its detail and comments (`issues.get`). `account`
    /// names the configured tracker account that owns the issue; it routes by
    /// exact provider + account instead of by id prefix.
    fn issues_get<'a>(
        &'a self,
        _id: &'a str,
        _repo: Option<&'a str>,
        _account: Option<&'a str>,
    ) -> BoxFuture<'a, ControlResult<thegn_core::issue::IssueDetail>> {
        Box::pin(async { Err(ControlError::Unimplemented("no issue tracker configured")) })
    }

    /// Patch an issue (`issues.update`); returns the updated issue. `account`
    /// names the configured tracker account that owns the issue; it routes by
    /// exact provider + account instead of by id prefix.
    fn issues_update<'a>(
        &'a self,
        _id: &'a str,
        _patch: &'a thegn_core::issue::IssuePatch,
        _repo: Option<&'a str>,
        _account: Option<&'a str>,
    ) -> BoxFuture<'a, ControlResult<thegn_core::issue::Issue>> {
        Box::pin(async { Err(ControlError::Unimplemented("no issue tracker configured")) })
    }

    /// [`Self::issues_update`] for a Lead-confined caller (6d): the daemon
    /// needs a live owner approval bound to this call's digest first.
    /// Fails closed by default.
    fn issues_update_confined<'a>(
        &'a self,
        _id: &'a str,
        _patch: &'a thegn_core::issue::IssuePatch,
        _repo: Option<&'a str>,
        _account: Option<&'a str>,
        _caller: LeadCaller,
    ) -> BoxFuture<'a, ControlResult<thegn_core::issue::Issue>> {
        Box::pin(async { Err(ControlError::Unimplemented("issues.update (Lead-confined)")) })
    }

    /// [`Self::issues_comment`] for a Lead-confined caller (6d). Fails
    /// closed by default.
    fn issues_comment_confined<'a>(
        &'a self,
        _id: &'a str,
        _body: &'a str,
        _repo: Option<&'a str>,
        _account: Option<&'a str>,
        _caller: LeadCaller,
    ) -> BoxFuture<'a, ControlResult<()>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "issues.comment (Lead-confined)",
            ))
        })
    }

    /// Post a comment on an issue (`issues.comment`). `account` names the
    /// configured tracker account that owns the issue; it routes by exact
    /// provider + account instead of by id prefix.
    fn issues_comment<'a>(
        &'a self,
        _id: &'a str,
        _body: &'a str,
        _repo: Option<&'a str>,
        _account: Option<&'a str>,
    ) -> BoxFuture<'a, ControlResult<()>> {
        Box::pin(async { Err(ControlError::Unimplemented("no issue tracker configured")) })
    }

    /// One newest-first keyset page of the agent-dispatch roster (`dispatches.list`).
    fn dispatches_list(
        &self,
        _before: Option<i64>,
        _limit: usize,
    ) -> BoxFuture<'_, ControlResult<DispatchPage>> {
        Box::pin(async { Err(ControlError::Unimplemented("dispatch roster unavailable")) })
    }

    /// Record a new dispatch (`dispatches.put`); returns the stored row.
    fn dispatch_put(
        &self,
        _req: DispatchPutReq,
    ) -> BoxFuture<'_, ControlResult<thegn_core::issue::AgentDispatch>> {
        Box::pin(async { Err(ControlError::Unimplemented("dispatch roster unavailable")) })
    }

    /// [`Self::dispatch_put`] for a Lead-confined caller (b155): the daemon
    /// refuses it while automations are paused. Fails closed by default, so
    /// an impl that cannot apply the pause check refuses the Lead.
    fn dispatch_put_confined(
        &self,
        _req: DispatchPutReq,
        _caller: LeadCaller,
    ) -> BoxFuture<'_, ControlResult<thegn_core::issue::AgentDispatch>> {
        Box::pin(async {
            Err(ControlError::FailedPrecondition(format!(
                "{}this daemon cannot apply the Lead pause check",
                thegn_core::lead_confine::REFUSAL_PREFIX
            )))
        })
    }

    /// Advance a dispatch's status (`dispatches.set_status`).
    fn dispatch_set_status(
        &self,
        _id: i64,
        _status: thegn_core::issue::AgentDispatchStatus,
    ) -> BoxFuture<'_, ControlResult<()>> {
        Box::pin(async { Err(ControlError::Unimplemented("dispatch roster unavailable")) })
    }

    /// Advance a dispatch's status with an explicit Lead-lease proof.
    fn dispatch_set_status_led(
        &self,
        _id: i64,
        _status: thegn_core::issue::AgentDispatchStatus,
        _lead: DispatchLeadReq,
    ) -> BoxFuture<'_, ControlResult<()>> {
        Box::pin(async { Err(ControlError::Unimplemented("dispatch roster unavailable")) })
    }

    fn dispatch_report(
        &self,
        _id: i64,
        _req: DispatchWorkerWriteReq,
    ) -> BoxFuture<'_, ControlResult<DispatchReportReply>> {
        Box::pin(async { Err(ControlError::Unimplemented("dispatch roster unavailable")) })
    }

    fn dispatch_note(
        &self,
        _id: i64,
        _req: DispatchWorkerWriteReq,
    ) -> BoxFuture<'_, ControlResult<DispatchNoteReply>> {
        Box::pin(async { Err(ControlError::Unimplemented("dispatch roster unavailable")) })
    }

    /// Create a worktree, optionally from an issue (`worktrees.create`).
    fn worktree_create(
        &self,
        _req: WorktreeCreateReq,
    ) -> BoxFuture<'_, ControlResult<WorktreeInfo>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "worktree creation is not available",
            ))
        })
    }

    /// Assign or clear a worktree's sidebar folder (`folders.assign`).
    fn folder_assign(&self, _req: FolderAssignReq) -> BoxFuture<'_, ControlResult<()>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "worktree folder assignment is not available",
            ))
        })
    }

    /// Read a worktree's pinned sandbox profile (`worktrees.sandbox_profile`).
    fn worktree_sandbox_profile(
        &self,
        _query: SandboxProfileQuery,
    ) -> BoxFuture<'_, ControlResult<SandboxProfileReply>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "worktree sandbox profile is not available",
            ))
        })
    }

    /// Pin or clear a worktree's sandbox profile
    /// (`worktrees.sandbox_profile_set`).
    fn set_worktree_sandbox_profile(
        &self,
        _req: SandboxProfileSetReq,
    ) -> BoxFuture<'_, ControlResult<SandboxProfileReply>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "worktree sandbox profile pinning is not available",
            ))
        })
    }

    /// Discovered coding-agent sessions from each harness's local store (the
    /// `agent.sessions` verb), optionally narrowed to one worktree / harness. A
    /// bounded read-on-demand filesystem scan — never spawns a harness, spends
    /// tokens, or returns credential material. Defaulted `Unimplemented` so
    /// transport-only impls and test fakes need no wiring; the daemon overrides
    /// it to run the scan on `spawn_blocking`.
    fn agent_sessions<'a>(
        &'a self,
        _worktree: Option<&'a str>,
        _harness: Option<&'a str>,
    ) -> BoxFuture<'a, ControlResult<Vec<thegn_core::harness::SessionRecord>>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "agent session discovery is not available",
            ))
        })
    }

    fn lease_status(&self) -> BoxFuture<'_, ControlResult<Vec<LeaseRow>>>;

    /// The mcp-proxy hub's per-upstream state (`mcp_proxy.status`). Defaulted so
    /// transport-only impls and test fakes need no wiring; the daemon overrides
    /// it against its upstream supervisor.
    fn mcp_proxy_status(&self) -> BoxFuture<'_, ControlResult<McpProxyStatus>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "mcp proxy is not configured on this instance",
            ))
        })
    }

    /// Re-read config and reconcile the mcp-proxy hub (`mcp_proxy.reload`).
    /// Defaulted to `Unimplemented`; the daemon overrides it.
    fn mcp_proxy_reload(&self) -> BoxFuture<'_, ControlResult<McpProxyReloadReport>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "mcp proxy is not configured on this instance",
            ))
        })
    }

    /// Read the UI's published plugin counters (`plugins.status`).
    fn plugins_status(&self) -> BoxFuture<'_, ControlResult<PluginsStatusReply>> {
        Box::pin(async {
            Err(ControlError::Unimplemented(
                "plugin status is not available on this instance",
            ))
        })
    }

    /// Publish a pairing lifecycle event on the broadcast feed
    /// ([`EventFrame::Pairing`]). The transport adapters call this after a
    /// successful pair/approve/revoke so `require_approval` pairings surface
    /// instead of parking silently. Default no-op: transport-only impls and
    /// test fakes need no feed wiring.
    fn publish_pairing(&self, _pairing_id: &str, _label: &str, _scope: &str, _state: PairingState) {
    }

    /// The broadcast event feed (activity, lease, pairing, session-list
    /// events). Pane bytes ride attach streams, not this feed.
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<EventFrame>>;

    /// Graceful daemon shutdown (admin).
    fn shutdown(&self) -> BoxFuture<'_, ()>;
}

#[cfg(test)]
mod editor_open_request_tests {
    use super::EditorOpenRequest;

    #[test]
    fn wire_shape_rejects_unknown_fields() {
        let err =
            serde_json::from_str::<EditorOpenRequest>(r#"{"worktree":"/w","provider":"cursor"}"#)
                .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn target_policy_rejects_unsafe_shapes() {
        for json in [
            r#"{"worktree":"relative"}"#,
            r#"{"worktree":"/w","path":"../escape"}"#,
            r#"{"worktree":"/w","line":1}"#,
            r#"{"worktree":"/w","path":"src/lib.rs","col":1}"#,
            r#"{"worktree":"/w","path":"src/lib.rs","line":0}"#,
        ] {
            let request: EditorOpenRequest = serde_json::from_str(json).unwrap();
            assert!(request.target().is_err(), "accepted unsafe shape: {json}");
        }
    }

    #[test]
    fn valid_wire_shape_builds_the_core_target() {
        let request: EditorOpenRequest =
            serde_json::from_str(r#"{"worktree":"/w","path":"src/./lib.rs","line":12,"col":4}"#)
                .unwrap();
        let target = request.target().unwrap();
        assert_eq!(target.worktree(), std::path::Path::new("/w"));
        assert_eq!(
            target.relative_file(),
            Some(std::path::Path::new("src/lib.rs"))
        );
        assert_eq!(target.line(), Some(12));
        assert_eq!(target.col(), Some(4));
    }
}

#[cfg(test)]
mod pty_size_tests {
    use super::{ControlError, MAX_PTY_COLS, MAX_PTY_ROWS, check_pty_size};

    #[test]
    fn the_bound_itself_is_accepted() {
        assert!(check_pty_size(MAX_PTY_ROWS, MAX_PTY_COLS).is_ok());
        assert!(check_pty_size(1000, 1000).is_ok());
        assert!(check_pty_size(0, 0).is_ok());
    }

    #[test]
    fn over_the_bound_is_invalid_argument_naming_the_limit() {
        for (rows, cols) in [(1001, 80), (24, 1001), (65535, 65535)] {
            match check_pty_size(rows, cols) {
                Err(ControlError::InvalidArgument(msg)) => {
                    assert!(msg.contains("1000x1000 limit"), "{msg}");
                    assert!(msg.contains(&format!("{cols}x{rows}")), "{msg}");
                }
                other => panic!("({rows}, {cols}) gave {other:?}"),
            }
        }
    }
}
