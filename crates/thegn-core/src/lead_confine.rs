//! What a Lead-confined caller may ask the daemon to do (THE-2187 follow-up).
//!
//! The Lead runs in a read-only bwrap sandbox and reaches the daemon only with a
//! scoped pairing token. Scopes alone are not enough: `sessions.open` is Write
//! scope and accepts a raw `argv`, so a Lead holding its token could ask the
//! daemon to run anything, unsandboxed, as the user. This module is the single
//! policy for that boundary:
//!
//! - [`rule`] maps every [`Verb`] to Allow / Refuse / AllowIf (an exhaustive
//!   match, so a new verb does not compile until it has a row);
//! - [`check`] applies the verb row to the request's shape at the transport;
//! - [`check_resolved_open`] runs in the daemon once a `sessions.open` has been
//!   resolved, against facts only the daemon has (is the worktree registered,
//!   what sandbox backend did the launch actually enter);
//! - [`check_rule_open_action`] holds Lead-authored automation rules to the
//!   same agent-by-name-only shape;
//! - `sessions.input` is shape-checked here (plain text and Enter only, see
//!   [`crate::lead_input`]); the daemon then allows it only into a session the
//!   Lead itself opened ([`ConfineRefusal::NotLeadWorker`]).
//!
//! Who is Lead-confined is decided from trusted state only (the pairing row's
//! label, minted by the host, and the listener the request arrived on), never
//! from a request field or `THEGN_CALLER`; see [`is_lead_label`].
//!
//! Pure and platform-neutral.

use std::borrow::Cow;

use crate::control::{TOKEN_ID_HEX, TOKEN_SECRET_HEX, Verb};

/// The pairing label of a bare Lead (`thegn mcp wire --lead`).
pub const LEAD_PAIRING_LABEL: &str = "lead";

/// The pairing-label prefix of a Lead tab (`lead:<group>/<tab>`).
pub const LEAD_PAIRING_PREFIX: &str = "lead:";

/// Every refusal message starts with this, so the transports and the daemon
/// can recognise (and audit) a confinement refusal.
pub const REFUSAL_PREFIX: &str = "lead-confined: ";

/// The largest prompt a Lead-confined `sessions.open` may carry, in bytes.
pub const MAX_LEAD_PROMPT_BYTES: usize = 64 * 1024;

/// The sandbox backend labels (`LaunchSpec::backend`, as reconciled against the
/// launched argv) that put a worker behind a filesystem boundary. Anything
/// else, including `host`, `systemd` (a resource cap, not isolation),
/// `jobobject`, the reserved `wsl`/`appcontainer` kinds, `devcontainer` and
/// any label this list has not heard of, counts as unsandboxed: fail closed.
pub const CONTAINED_BACKENDS: &[&str] = &[
    "bwrap",
    "podman-rootless",
    "podman-rootful",
    "docker",
    "smolmachines",
    "apple",
];

/// Whether a pairing label is one the Lead wiring mints (`lead::mcp`). A
/// non-Lead token minted with such a label is confined too, which is the safe
/// direction; a Lead cannot mint itself a different label (minting needs
/// Admin, and a code redeemed on the Lead listener parks for approval).
pub fn is_lead_label(label: &str) -> bool {
    label == LEAD_PAIRING_LABEL || label.starts_with(LEAD_PAIRING_PREFIX)
}

/// Whether an automation rule id is Lead-authored (`lead:<slug>`), so the
/// session it opens is held to the same rules as a direct Lead call.
pub fn is_lead_rule_id(rule_id: &str) -> bool {
    rule_id.starts_with(crate::automation_authoring::LEAD_RULE_PREFIX)
}

/// Whether a resolved sandbox backend label proves isolation.
pub fn is_contained_backend(label: &str) -> bool {
    CONTAINED_BACKENDS.contains(&label.trim())
}

/// One verb's row in the confinement table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Safe for a Lead-confined caller as-is.
    Allow,
    /// Allowed only when the request's shape passes [`check`].
    AllowIf,
    /// Never for a Lead-confined caller; the reason is user-facing.
    Refuse(&'static str),
}

const NO_LAND: &str = "the Lead lands only through a rule's land step or an owner approval; it cannot enqueue or drain the merge queue";
const ADMIN_ONLY: &str = "operator-only (admin scope); never reachable by the Lead";
const EXEC_ONLY: &str =
    "runs a configured command on the host (exec scope); never reachable by the Lead";
const NO_ATTACH: &str = "an interactive attach can type into the session; the Lead reads screens with sessions.snapshot";
const NO_FORK: &str = "a fork can re-run the source session's raw command; the Lead opens a configured agent with sessions.open instead";
const NO_AGENT_DISPATCH: &str = "agent.dispatch tracks its roster row from the MCP process; the Lead opens a configured agent with sessions.open and the daemon binds rows itself";
const NO_SPLIT: &str = "a split launches a caller-supplied argv or a login shell on the host";
const NO_HOST_UI: &str = "hands a host path to the compositor, which opens it outside any sandbox";
const NO_HOST_GIT: &str =
    "runs git on the host, which executes repository hooks and filters outside any sandbox";
const NO_HOST_WRITE: &str = "writes into a worktree from outside the sandbox";
const NO_RESUME: &str = "only the operator resumes automations (admin scope); the Lead may pause them, never restart them";
const NO_REDEEM: &str = "only the owner's phone answers approvals";
const NO_HALT: &str = "only the operator halts automations";

/// The confinement table: one explicit row per verb. No wildcard arm, so a new
/// [`Verb`] fails to compile until someone decides its row.
pub fn rule(verb: Verb) -> Rule {
    match verb {
        // Observe-only reads. The Lead already has a read-only view of the
        // host; none of these starts a process or writes outside thegn's DB.
        Verb::ListSessions
        | Verb::FleetList
        | Verb::ListWorktrees
        | Verb::WorktreeSandboxProfile
        | Verb::Snapshot
        | Verb::Events
        | Verb::LeaseStatus
        | Verb::GitStatus
        | Verb::MergeList
        | Verb::CalendarEvents
        | Verb::CalendarClocks
        | Verb::Wait
        | Verb::PrStatus
        | Verb::CiRuns
        | Verb::CiLogs
        | Verb::McpProxyStatus
        | Verb::PluginsStatus
        | Verb::ProjectList
        | Verb::IssuesList
        | Verb::IssuesGet
        | Verb::DispatchesList
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
        | Verb::AutomationsList
        | Verb::AutomationsTest
        | Verb::SkillsList
        | Verb::PreviewFetch
        | Verb::ModelProxyStatus
        | Verb::ModelProxyStats
        | Verb::AutopilotStatus
        | Verb::OverviewGet
        | Verb::NotificationsList
        | Verb::AutomationsRuns
        | Verb::ApprovalsList
        // b163: producer arming and scrubbed inbox account state.
        | Verb::ProducersStatus
        | Verb::Me => Rule::Allow,

        // Data-only writes: thegn's own DB rows, tracker writes, owned
        // container lifecycle, the trusted-config MCP reload. None takes an
        // argv, env, hook or free path.
        Verb::KillSession
        | Verb::RecordSession
        | Verb::CalendarIngest
        | Verb::NotifyPush
        | Verb::McpProxyReload
        | Verb::ProjectCreate
        | Verb::ProjectRename
        | Verb::ProjectRemove
        | Verb::ProjectAssign
        | Verb::DispatchesSetStatus
        | Verb::DispatchesReport
        | Verb::DispatchesNote
        | Verb::ContainersControl
        | Verb::NotificationsAck
        | Verb::AutomationsPut
        | Verb::AutomationsRm
        | Verb::AutomationsEnable
        | Verb::ApprovalsRequest
        | Verb::MergeClear
        | Verb::FolderAssign
        // THE-2197: pausing only stops automations; it takes no arguments.
        | Verb::AutomationsPause => Rule::Allow,

        // Shape-checked. `sessions.input` is further held, in the daemon, to
        // sessions a Lead opened.
        Verb::OpenSession | Verb::DispatchesPut | Verb::SendInput => Rule::AllowIf,
        // b155 (6d): outward tracker writes go through the daemon's
        // `issues_*_confined` path, which needs an owner approval per call.
        // (THE-2222): a profile's env is returned only for the Lead's own
        // profile; the daemon checks the label and the profile.
        Verb::IssuesUpdate | Verb::IssuesComment | Verb::McpProxyProfileEnv => Rule::AllowIf,
        // b155 (6a, 6f).
        Verb::ApprovalsRedeem => Rule::Refuse(NO_REDEEM),
        Verb::AutomationsHalt => Rule::Refuse(NO_HALT),
        // b155 (THE-2171): landing is never a Lead call. The stage executor
        // enqueues and drains with its own (unconfined) authority after a
        // rule's land step or an owner approval.
        Verb::MergeAdd | Verb::MergeDrain => Rule::Refuse(NO_LAND),

        Verb::ForkSession => Rule::Refuse(NO_FORK),
        // MCP-only, and its roster write happens in the MCP process, outside
        // the daemon's `dispatches.put` confinement.
        Verb::AgentDispatch => Rule::Refuse(NO_AGENT_DISPATCH),
        Verb::Split => Rule::Refuse(NO_SPLIT),
        Verb::Attach | Verb::Detach | Verb::Resize => Rule::Refuse(NO_ATTACH),
        Verb::OpenWorktree | Verb::OpenEditor => Rule::Refuse(NO_HOST_UI),
        Verb::GitStage | Verb::GitCommit | Verb::WorktreeCreate => Rule::Refuse(NO_HOST_GIT),
        Verb::SearchReplace | Verb::SkillsSeed | Verb::ProjectNewFeature => {
            Rule::Refuse(NO_HOST_WRITE)
        }
        Verb::LaunchPreset | Verb::ToolsRun => Rule::Refuse(EXEC_ONLY),
        Verb::IssuePairing
        | Verb::WorktreeSandboxProfileSet
        | Verb::ListPairings
        | Verb::RevokePairing
        | Verb::ApprovePairing
        | Verb::ContainersPrune
        | Verb::DoctorBundle
        | Verb::Shutdown
        | Verb::SecretSet
        | Verb::SecretRm
        | Verb::SecretList
        | Verb::SecretMigrate
        | Verb::SecretAudit
        | Verb::SecretSshRotate
        | Verb::MigrateSession
        | Verb::ModelProxyStart
        | Verb::ModelProxyStop
        | Verb::ApprovalsResolve => Rule::Refuse(ADMIN_ONLY),
        // THE-2197: a Lead that paused automations can never restart them.
        Verb::AutomationsResume => Rule::Refuse(NO_RESUME),
    }
}

/// The agent half of a `sessions.open` request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentShape<'a> {
    /// The `[[agents]]` name (or provider id) asked for.
    pub name: &'a str,
    pub prompt_len: usize,
    pub widen_permissions: bool,
    pub fork: bool,
    pub native_session_id: bool,
}

/// The parts of a `sessions.open` request the confinement looks at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenShape<'a> {
    pub argv_len: usize,
    pub env_len: usize,
    pub cwd: Option<&'a str>,
    pub worktree: Option<&'a str>,
    pub agent: Option<AgentShape<'a>>,
    pub already_capped: bool,
    pub automation_origin: bool,
}

/// What a transport knows about a request when it checks confinement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestShape<'a> {
    /// Nothing beyond the verb (every verb but the AllowIf ones).
    Opaque,
    Open(OpenShape<'a>),
    /// `dispatches.put`: whether the row names a session to bind.
    DispatchPut {
        session_id: bool,
    },
    /// `sessions.input`: the exact bytes that would reach the PTY (Enter
    /// already appended), judged by [`crate::lead_input::check`].
    Input {
        bytes: &'a [u8],
    },
    /// The transport routes this request to a daemon path that applies the
    /// verb's condition itself (`issues_*_confined`,
    /// `mcp_proxy.profile_env`).
    DaemonGated,
}

/// Why a Lead-confined request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfineRefusal {
    /// The verb's row is Refuse.
    Verb {
        verb: Verb,
        why: &'static str,
    },
    /// An AllowIf verb arrived without a shape the transport could check.
    Unshaped(Verb),
    RawArgv,
    EnvOverride,
    NoAgent,
    NoWorktree,
    CwdOutsideWorktree,
    /// A launch knob the Lead may not set.
    Field(&'static str),
    PromptTooLarge {
        len: usize,
    },
    DispatchSession,
    UnregisteredWorktree(String),
    Unsandboxed {
        worktree: String,
        backend: String,
    },
    AutomationAgent,
    /// `sessions.input` bytes that are not plain text and Enter.
    Input(crate::lead_input::InputRefusal),
    /// `sessions.input` into a session the Lead did not open (or one that is
    /// gone, or no longer the session the Lead opened under that id).
    NotLeadWorker(String),
}

fn capability(verb: Verb) -> &'static str {
    crate::capability::for_verb(verb).map_or("this verb", |c| c.id.as_str())
}

impl std::fmt::Display for ConfineRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REFUSAL_PREFIX)?;
        match self {
            ConfineRefusal::Verb { verb, why } => write!(f, "{}: {why}", capability(*verb)),
            ConfineRefusal::Unshaped(verb) => write!(
                f,
                "{} is only allowed for the Lead over its HTTP listener",
                capability(*verb)
            ),
            ConfineRefusal::RawArgv => f.write_str(
                "sessions.open with a raw argv is refused; name a configured agent instead",
            ),
            ConfineRefusal::EnvOverride => {
                f.write_str("sessions.open with an env override is refused for the Lead")
            }
            ConfineRefusal::NoAgent => {
                f.write_str("the Lead may only open a configured [[agents]] entry by name")
            }
            ConfineRefusal::NoWorktree => {
                f.write_str("the Lead must name the registered worktree the agent runs in")
            }
            ConfineRefusal::CwdOutsideWorktree => {
                f.write_str("cwd must be omitted or equal the worktree for the Lead")
            }
            ConfineRefusal::Field(field) => {
                write!(f, "sessions.open `{field}` is not available to the Lead")
            }
            ConfineRefusal::PromptTooLarge { len } => write!(
                f,
                "prompt is {len} bytes; the Lead's limit is {MAX_LEAD_PROMPT_BYTES}"
            ),
            ConfineRefusal::DispatchSession => f.write_str(
                "dispatches.put may not bind a session for the Lead; the daemon binds rows itself",
            ),
            ConfineRefusal::UnregisteredWorktree(wt) => {
                write!(f, "{wt} is not a worktree registered with thegn")
            }
            ConfineRefusal::Unsandboxed { worktree, backend } => write!(
                f,
                "the Lead may only launch sandboxed workers, but {worktree} resolves to sandbox \
                 backend `{backend}`; give that worktree a sandbox ([sandbox] backend, or its \
                 per-worktree sandbox setting) or launch the agent yourself"
            ),
            ConfineRefusal::AutomationAgent => f.write_str(
                "a Lead-authored sessions.open rule must name a configured agent (`then.agent`)",
            ),
            ConfineRefusal::Input(why) => write!(f, "sessions.input: {why}"),
            ConfineRefusal::NotLeadWorker(session) => write!(
                f,
                "sessions.input into {session} is refused: the Lead may only answer a live \
                 worker it opened itself"
            ),
        }
    }
}

impl std::error::Error for ConfineRefusal {}

/// Check one Lead-confined request at the transport. `Ok` means the transport
/// may dispatch it; an AllowIf `sessions.open` still faces
/// [`check_resolved_open`] in the daemon.
pub fn check(verb: Verb, req: &RequestShape<'_>) -> Result<(), ConfineRefusal> {
    match rule(verb) {
        Rule::Allow => Ok(()),
        Rule::Refuse(why) => Err(ConfineRefusal::Verb { verb, why }),
        Rule::AllowIf => match (verb, req) {
            (Verb::OpenSession, RequestShape::Open(open)) => check_open(open),
            (Verb::DispatchesPut, RequestShape::DispatchPut { session_id }) => {
                if *session_id {
                    Err(ConfineRefusal::DispatchSession)
                } else {
                    Ok(())
                }
            }
            (Verb::SendInput, RequestShape::Input { bytes }) => {
                crate::lead_input::check(bytes).map_err(ConfineRefusal::Input)
            }
            (
                Verb::IssuesUpdate | Verb::IssuesComment | Verb::McpProxyProfileEnv,
                RequestShape::DaemonGated,
            ) => Ok(()),
            _ => Err(ConfineRefusal::Unshaped(verb)),
        },
    }
}

fn check_open(open: &OpenShape<'_>) -> Result<(), ConfineRefusal> {
    if open.argv_len > 0 {
        return Err(ConfineRefusal::RawArgv);
    }
    if open.env_len > 0 {
        return Err(ConfineRefusal::EnvOverride);
    }
    let Some(agent) = open.agent.filter(|a| !a.name.trim().is_empty()) else {
        return Err(ConfineRefusal::NoAgent);
    };
    if open.already_capped {
        return Err(ConfineRefusal::Field("already_capped"));
    }
    if open.automation_origin {
        return Err(ConfineRefusal::Field("automation_origin"));
    }
    if agent.widen_permissions {
        return Err(ConfineRefusal::Field("widen_permissions"));
    }
    if agent.fork || agent.native_session_id {
        return Err(ConfineRefusal::Field("fork"));
    }
    if agent.prompt_len > MAX_LEAD_PROMPT_BYTES {
        return Err(ConfineRefusal::PromptTooLarge {
            len: agent.prompt_len,
        });
    }
    let Some(worktree) = open.worktree.filter(|w| !w.trim().is_empty()) else {
        return Err(ConfineRefusal::NoWorktree);
    };
    match open.cwd.filter(|c| !c.is_empty()) {
        Some(cwd) if cwd != worktree => Err(ConfineRefusal::CwdOutsideWorktree),
        _ => Ok(()),
    }
}

/// The daemon's facts about a resolved Lead-confined `sessions.open`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedOpen<'a> {
    pub worktree: &'a str,
    /// The worktree is a row in thegn's registry.
    pub registered: bool,
    /// `LaunchSpec::backend` of the resolved launch (`None` before resolving).
    pub backend: Option<&'a str>,
}

/// The daemon-side half of the `sessions.open` rule: the worktree must be
/// registered, and (once resolved) the launch must have entered a contained
/// sandbox backend. Call it before resolving with `backend: None` to refuse an
/// unregistered path without side effects, then again with the backend.
pub fn check_resolved_open(facts: &ResolvedOpen<'_>) -> Result<(), ConfineRefusal> {
    if !facts.registered {
        return Err(ConfineRefusal::UnregisteredWorktree(
            facts.worktree.to_string(),
        ));
    }
    match facts.backend {
        Some(backend) if !is_contained_backend(backend) => Err(ConfineRefusal::Unsandboxed {
            worktree: facts.worktree.to_string(),
            backend: backend.trim().to_string(),
        }),
        _ => Ok(()),
    }
}

/// A Lead-authored automation rule's `sessions.open` action: a configured agent
/// by name, with a bounded prompt. The action type carries no argv or env
/// field at all; the worktree comes from the triggering event and the daemon
/// re-checks it (registered, sandboxed) when the rule fires.
pub fn check_rule_open_action(
    agent: Option<&str>,
    prompt: Option<&str>,
) -> Result<(), ConfineRefusal> {
    if agent.is_none_or(|a| a.trim().is_empty()) {
        return Err(ConfineRefusal::AutomationAgent);
    }
    let len = prompt.map_or(0, str::len);
    if len > MAX_LEAD_PROMPT_BYTES {
        return Err(ConfineRefusal::PromptTooLarge { len });
    }
    Ok(())
}

/// What replaces a credential in Lead-visible screen text.
pub const REDACTED_CREDENTIAL: &str = "[redacted credential]";

/// Replace every control token (`tgc1_<id>_<secret>`) and pairing code
/// (`tgp1_<id>_<secret>`) in `text`, in exactly the shape
/// [`crate::control::parse_token`] accepts, with [`REDACTED_CREDENTIAL`].
/// Defense in depth for a Lead-confined `sessions.snapshot`; borrowed when
/// nothing matched.
pub fn redact_credentials(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let token_len = 5 + TOKEN_ID_HEX + 1 + TOKEN_SECRET_HEX;
    let mut out: Option<String> = None;
    let mut last = 0;
    let mut i = 0;
    while i + token_len <= bytes.len() {
        if credential_at(bytes, i) {
            let o = out.get_or_insert_with(|| String::with_capacity(text.len()));
            // `i` and `last` sit on ASCII bytes, so both are char boundaries.
            o.push_str(&text[last..i]);
            o.push_str(REDACTED_CREDENTIAL);
            i += token_len;
            last = i;
        } else {
            i += 1;
        }
    }
    match out {
        None => Cow::Borrowed(text),
        Some(mut o) => {
            o.push_str(&text[last..]);
            Cow::Owned(o)
        }
    }
}

fn credential_at(b: &[u8], i: usize) -> bool {
    let lower_hex = |s: &[u8]| {
        s.iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
    };
    let rest = &b[i..];
    let id_end = 5 + TOKEN_ID_HEX;
    (rest.starts_with(b"tgc1_") || rest.starts_with(b"tgp1_"))
        && lower_hex(&rest[5..id_end])
        && rest[id_end] == b'_'
        && lower_hex(&rest[id_end + 1..id_end + 1 + TOKEN_SECRET_HEX])
}

#[cfg(test)]
mod tests {
    use super::{
        AgentShape, CONTAINED_BACKENDS, ConfineRefusal, MAX_LEAD_PROMPT_BYTES, OpenShape,
        REDACTED_CREDENTIAL, REFUSAL_PREFIX, RequestShape, ResolvedOpen, Rule, check,
        check_resolved_open, check_rule_open_action, is_contained_backend, is_lead_label,
        is_lead_rule_id, redact_credentials, rule,
    };
    use crate::control::{Scope, Verb, required_scope};
    use crate::lead_input::{InputRefusal, MAX_LEAD_INPUT_BYTES};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Kind {
        Allow,
        AllowIf,
        Refuse,
    }
    use Kind::{Allow, AllowIf, Refuse};

    /// The pinned per-verb table. Adding a [`Verb`] fails
    /// `every_verb_has_a_pinned_row` until a row is added here, on purpose:
    /// each new verb needs a deliberate Lead decision.
    const TABLE: &[(Verb, Kind)] = &[
        (Verb::ListSessions, Allow),
        (Verb::FleetList, Allow),
        (Verb::MigrateSession, Refuse),
        (Verb::ListWorktrees, Allow),
        (Verb::OpenSession, AllowIf),
        (Verb::ForkSession, Refuse),
        (Verb::Attach, Refuse),
        (Verb::Detach, Refuse),
        (Verb::SendInput, AllowIf),
        (Verb::Resize, Refuse),
        (Verb::Snapshot, Allow),
        (Verb::KillSession, Allow),
        (Verb::OpenWorktree, Refuse),
        (Verb::OpenEditor, Refuse),
        (Verb::PreviewFetch, Allow),
        (Verb::Wait, Allow),
        (Verb::Split, Refuse),
        (Verb::LaunchPreset, Refuse),
        (Verb::RecordSession, Allow),
        (Verb::GitStatus, Allow),
        (Verb::GitStage, Refuse),
        (Verb::GitCommit, Refuse),
        (Verb::MergeList, Allow),
        (Verb::MergeAdd, Refuse),
        (Verb::MergeClear, Allow),
        (Verb::CalendarEvents, Allow),
        (Verb::CalendarClocks, Allow),
        (Verb::CalendarIngest, Allow),
        (Verb::Events, Allow),
        (Verb::LeaseStatus, Allow),
        (Verb::Me, Allow),
        (Verb::IssuePairing, Refuse),
        (Verb::ListPairings, Refuse),
        (Verb::RevokePairing, Refuse),
        (Verb::ApprovePairing, Refuse),
        (Verb::Shutdown, Refuse),
        (Verb::PrStatus, Allow),
        (Verb::CiRuns, Allow),
        (Verb::CiLogs, Allow),
        (Verb::NotifyPush, Allow),
        (Verb::DoctorBundle, Refuse),
        (Verb::SecretSet, Refuse),
        (Verb::SecretRm, Refuse),
        (Verb::SecretList, Refuse),
        (Verb::SecretMigrate, Refuse),
        (Verb::SecretAudit, Refuse),
        (Verb::SecretSshRotate, Refuse),
        (Verb::McpProxyStatus, Allow),
        (Verb::McpProxyReload, Allow),
        (Verb::PluginsStatus, Allow),
        (Verb::ProjectList, Allow),
        (Verb::ProjectCreate, Allow),
        (Verb::ProjectRename, Allow),
        (Verb::ProjectRemove, Allow),
        (Verb::ProjectAssign, Allow),
        (Verb::ProjectNewFeature, Refuse),
        (Verb::IssuesList, Allow),
        (Verb::IssuesGet, Allow),
        (Verb::IssuesUpdate, AllowIf),
        (Verb::IssuesComment, AllowIf),
        (Verb::DispatchesList, Allow),
        (Verb::DispatchesPut, AllowIf),
        (Verb::AgentDispatch, Refuse),
        (Verb::DispatchesSetStatus, Allow),
        (Verb::DispatchesVerify, Allow),
        (Verb::DispatchesWait, Allow),
        (Verb::DispatchesReport, Allow),
        (Verb::DispatchesNote, Allow),
        (Verb::DispatchesStatus, Allow),
        (Verb::WorktreeCreate, Refuse),
        (Verb::FolderAssign, Allow),
        (Verb::WorktreeSandboxProfile, Allow),
        (Verb::WorktreeSandboxProfileSet, Refuse),
        (Verb::SearchQuery, Allow),
        (Verb::SearchReplace, Refuse),
        (Verb::HostDiscover, Allow),
        (Verb::ContainersList, Allow),
        (Verb::ContainersControl, Allow),
        (Verb::ContainersPrune, Refuse),
        (Verb::SemanticMap, Allow),
        (Verb::SemanticBlastRadius, Allow),
        (Verb::AgentSessions, Allow),
        (Verb::AgentList, Allow),
        (Verb::AutomationsList, Allow),
        (Verb::AutomationsTest, Allow),
        (Verb::ToolsRun, Refuse),
        (Verb::SkillsList, Allow),
        (Verb::SkillsSeed, Refuse),
        (Verb::ModelProxyStatus, Allow),
        (Verb::ModelProxyStats, Allow),
        (Verb::AutopilotStatus, Allow),
        (Verb::ModelProxyStart, Refuse),
        (Verb::ModelProxyStop, Refuse),
        (Verb::OverviewGet, Allow),
        (Verb::NotificationsList, Allow),
        (Verb::NotificationsAck, Allow),
        (Verb::AutomationsPut, Allow),
        (Verb::AutomationsRm, Allow),
        (Verb::AutomationsEnable, Allow),
        (Verb::AutomationsRuns, Allow),
        (Verb::ApprovalsList, Allow),
        (Verb::ApprovalsRequest, Allow),
        (Verb::ApprovalsResolve, Refuse),
        (Verb::MergeDrain, Refuse),
        (Verb::AutomationsPause, Allow),
        (Verb::AutomationsResume, Refuse),
        (Verb::ApprovalsRedeem, Refuse),
        (Verb::AutomationsHalt, Refuse),
        (Verb::McpProxyProfileEnv, AllowIf),
        (Verb::ProducersStatus, Allow),
    ];

    fn kind(r: Rule) -> Kind {
        match r {
            Rule::Allow => Allow,
            Rule::AllowIf => AllowIf,
            Rule::Refuse(_) => Refuse,
        }
    }

    fn good_open() -> OpenShape<'static> {
        OpenShape {
            worktree: Some("/w/feature"),
            agent: Some(AgentShape {
                name: "coder",
                prompt_len: 12,
                ..AgentShape::default()
            }),
            ..OpenShape::default()
        }
    }

    fn open(shape: OpenShape<'_>) -> Result<(), ConfineRefusal> {
        check(Verb::OpenSession, &RequestShape::Open(shape))
    }

    #[test]
    fn every_verb_has_a_pinned_row() {
        assert_eq!(TABLE.len(), Verb::ALL.len(), "TABLE must pin every verb");
        for verb in Verb::ALL {
            let pinned = TABLE
                .iter()
                .find(|(v, _)| v == verb)
                .unwrap_or_else(|| panic!("{verb:?} has no pinned Lead row"));
            assert_eq!(kind(rule(*verb)), pinned.1, "{verb:?}");
        }
    }

    #[test]
    fn admin_and_exec_verbs_are_always_refused() {
        for verb in Verb::ALL {
            if matches!(required_scope(*verb), Scope::Admin | Scope::Exec) {
                assert_eq!(kind(rule(*verb)), Refuse, "{verb:?}");
            }
        }
    }

    #[test]
    fn refused_verbs_refuse_with_the_prefix_and_capability() {
        for verb in Verb::ALL {
            let got = check(*verb, &RequestShape::Opaque);
            match kind(rule(*verb)) {
                Allow => assert!(got.is_ok(), "{verb:?}"),
                AllowIf => assert_eq!(got, Err(ConfineRefusal::Unshaped(*verb))),
                Refuse => {
                    let msg = got.unwrap_err().to_string();
                    assert!(msg.starts_with(REFUSAL_PREFIX), "{msg}");
                }
            }
        }
        let msg = check(Verb::WorktreeSandboxProfileSet, &RequestShape::Opaque)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("worktrees.sandbox_profile_set"), "{msg}");
    }

    #[test]
    fn b155_rows_redeem_and_halt_refused_outward_and_profile_env_daemon_gated() {
        for verb in [Verb::ApprovalsRedeem, Verb::AutomationsHalt] {
            let msg = check(verb, &RequestShape::Opaque).unwrap_err().to_string();
            assert!(msg.starts_with(REFUSAL_PREFIX), "{msg}");
        }
        assert!(
            check(Verb::ApprovalsRedeem, &RequestShape::Opaque)
                .unwrap_err()
                .to_string()
                .contains("phone")
        );
        for verb in [
            Verb::IssuesUpdate,
            Verb::IssuesComment,
            Verb::McpProxyProfileEnv,
        ] {
            assert_eq!(rule(verb), Rule::AllowIf, "{verb:?}");
            assert!(check(verb, &RequestShape::DaemonGated).is_ok(), "{verb:?}");
            assert_eq!(
                check(verb, &RequestShape::Opaque),
                Err(ConfineRefusal::Unshaped(verb))
            );
        }
        // DaemonGated is not a pass for any other verb.
        assert_eq!(
            check(Verb::OpenSession, &RequestShape::DaemonGated),
            Err(ConfineRefusal::Unshaped(Verb::OpenSession))
        );
    }

    #[test]
    fn redact_credentials_replaces_both_token_shapes() {
        let id = "0123abcd";
        let secret = "f".repeat(64);
        let control = format!("tgc1_{id}_{secret}");
        let code = format!("tgp1_{id}_{secret}");
        assert!(crate::control::parse_token(&control).is_some());
        assert!(crate::control::parse_token(&code).is_some());
        let text = format!("token={control}\ncode:{code}{code} end é");
        let got = redact_credentials(&text);
        assert_eq!(
            got,
            format!(
                "token={REDACTED_CREDENTIAL}\ncode:{REDACTED_CREDENTIAL}{REDACTED_CREDENTIAL} end é"
            )
        );
        // Nothing to redact: borrowed, unchanged.
        for clean in [
            "plain text".to_string(),
            format!("tgp1_{id}_{}", "g".repeat(64)),
            format!("tgp1_{}_{secret}", "ABCDEF01"),
            format!("tgc1_{id}_{}", "f".repeat(63)),
            format!("tgx1_{id}_{secret}"),
            "tgc1_".to_string(),
        ] {
            let got = redact_credentials(&clean);
            assert!(matches!(got, std::borrow::Cow::Borrowed(_)), "{clean}");
            assert_eq!(got, clean);
        }
    }

    #[test]
    fn lead_may_pause_automations_but_never_resume_them() {
        // THE-2197: the kill switch is one-way for the Lead.
        assert_eq!(rule(Verb::AutomationsPause), Rule::Allow);
        assert!(check(Verb::AutomationsPause, &RequestShape::Opaque).is_ok());
        assert!(matches!(rule(Verb::AutomationsResume), Rule::Refuse(_)));
        let msg = check(Verb::AutomationsResume, &RequestShape::Opaque)
            .unwrap_err()
            .to_string();
        assert!(msg.starts_with(REFUSAL_PREFIX), "{msg}");
        assert!(
            msg.contains("only the operator resumes automations"),
            "{msg}"
        );
    }

    #[test]
    fn sandbox_profile_set_is_refused() {
        assert!(matches!(
            check(Verb::WorktreeSandboxProfileSet, &RequestShape::Opaque),
            Err(ConfineRefusal::Verb { .. })
        ));
    }

    #[test]
    fn split_fork_and_attach_are_refused() {
        for verb in [
            Verb::Split,
            Verb::ForkSession,
            Verb::Attach,
            Verb::GitCommit,
            Verb::OpenEditor,
        ] {
            assert!(
                matches!(
                    check(verb, &RequestShape::Opaque),
                    Err(ConfineRefusal::Verb { .. })
                ),
                "{verb:?}"
            );
        }
    }

    #[test]
    fn session_input_is_allow_if_plain_text() {
        assert_eq!(rule(Verb::SendInput), Rule::AllowIf);
        assert_eq!(
            check(
                Verb::SendInput,
                &RequestShape::Input {
                    bytes: b"yes, use the second option\r"
                }
            ),
            Ok(())
        );
        assert_eq!(
            check(Verb::SendInput, &RequestShape::Input { bytes: b"\x1b[A" }),
            Err(ConfineRefusal::Input(InputRefusal::Escape))
        );
        assert_eq!(
            check(Verb::SendInput, &RequestShape::Input { bytes: b"\x03" }),
            Err(ConfineRefusal::Input(InputRefusal::Control(3)))
        );
        let big = vec![b'x'; MAX_LEAD_INPUT_BYTES + 1];
        assert!(matches!(
            check(Verb::SendInput, &RequestShape::Input { bytes: &big }),
            Err(ConfineRefusal::Input(InputRefusal::TooLarge { .. }))
        ));
        // gRPC carries no shape: refused, not allowed.
        assert_eq!(
            check(Verb::SendInput, &RequestShape::Opaque),
            Err(ConfineRefusal::Unshaped(Verb::SendInput))
        );
        // An input shape never satisfies another verb.
        assert_eq!(
            check(Verb::OpenSession, &RequestShape::Input { bytes: b"x" }),
            Err(ConfineRefusal::Unshaped(Verb::OpenSession))
        );
    }

    #[test]
    fn a_configured_agent_in_a_worktree_is_allowed() {
        assert_eq!(open(good_open()), Ok(()));
        let same_cwd = OpenShape {
            cwd: Some("/w/feature"),
            ..good_open()
        };
        assert_eq!(open(same_cwd), Ok(()));
        let max_prompt = OpenShape {
            agent: Some(AgentShape {
                name: "coder",
                prompt_len: MAX_LEAD_PROMPT_BYTES,
                ..AgentShape::default()
            }),
            ..good_open()
        };
        assert_eq!(open(max_prompt), Ok(()));
    }

    #[test]
    fn raw_argv_is_refused() {
        let shape = OpenShape {
            argv_len: 1,
            ..good_open()
        };
        assert_eq!(open(shape), Err(ConfineRefusal::RawArgv));
        let bare = OpenShape {
            argv_len: 2,
            agent: None,
            ..OpenShape::default()
        };
        assert_eq!(open(bare), Err(ConfineRefusal::RawArgv));
    }

    #[test]
    fn env_is_refused() {
        let shape = OpenShape {
            env_len: 1,
            ..good_open()
        };
        assert_eq!(open(shape), Err(ConfineRefusal::EnvOverride));
    }

    #[test]
    fn a_missing_or_blank_agent_is_refused() {
        let none = OpenShape {
            agent: None,
            ..good_open()
        };
        assert_eq!(open(none), Err(ConfineRefusal::NoAgent));
        let blank = OpenShape {
            agent: Some(AgentShape {
                name: "  ",
                ..AgentShape::default()
            }),
            ..good_open()
        };
        assert_eq!(open(blank), Err(ConfineRefusal::NoAgent));
    }

    #[test]
    fn launch_knobs_the_lead_may_not_set_are_refused() {
        let capped = OpenShape {
            already_capped: true,
            ..good_open()
        };
        assert_eq!(open(capped), Err(ConfineRefusal::Field("already_capped")));
        let origin = OpenShape {
            automation_origin: true,
            ..good_open()
        };
        assert_eq!(
            open(origin),
            Err(ConfineRefusal::Field("automation_origin"))
        );
        for agent in [
            AgentShape {
                name: "coder",
                widen_permissions: true,
                ..AgentShape::default()
            },
            AgentShape {
                name: "coder",
                fork: true,
                ..AgentShape::default()
            },
            AgentShape {
                name: "coder",
                native_session_id: true,
                ..AgentShape::default()
            },
        ] {
            let shape = OpenShape {
                agent: Some(agent),
                ..good_open()
            };
            assert!(matches!(open(shape), Err(ConfineRefusal::Field(_))));
        }
        let big = OpenShape {
            agent: Some(AgentShape {
                name: "coder",
                prompt_len: MAX_LEAD_PROMPT_BYTES + 1,
                ..AgentShape::default()
            }),
            ..good_open()
        };
        assert!(matches!(
            open(big),
            Err(ConfineRefusal::PromptTooLarge { .. })
        ));
    }

    #[test]
    fn worktree_and_cwd_rules() {
        let none = OpenShape {
            worktree: None,
            ..good_open()
        };
        assert_eq!(open(none), Err(ConfineRefusal::NoWorktree));
        let blank = OpenShape {
            worktree: Some(" "),
            ..good_open()
        };
        assert_eq!(open(blank), Err(ConfineRefusal::NoWorktree));
        let elsewhere = OpenShape {
            cwd: Some("/etc"),
            ..good_open()
        };
        assert_eq!(open(elsewhere), Err(ConfineRefusal::CwdOutsideWorktree));
    }

    #[test]
    fn an_unshaped_or_mismatched_allow_if_is_refused() {
        assert_eq!(
            check(
                Verb::OpenSession,
                &RequestShape::DispatchPut { session_id: false }
            ),
            Err(ConfineRefusal::Unshaped(Verb::OpenSession))
        );
        assert_eq!(
            check(Verb::DispatchesPut, &RequestShape::Open(good_open())),
            Err(ConfineRefusal::Unshaped(Verb::DispatchesPut))
        );
    }

    #[test]
    fn dispatch_put_may_not_bind_a_session() {
        assert_eq!(
            check(
                Verb::DispatchesPut,
                &RequestShape::DispatchPut { session_id: false }
            ),
            Ok(())
        );
        assert_eq!(
            check(
                Verb::DispatchesPut,
                &RequestShape::DispatchPut { session_id: true }
            ),
            Err(ConfineRefusal::DispatchSession)
        );
    }

    #[test]
    fn unregistered_worktree_is_refused() {
        let got = check_resolved_open(&ResolvedOpen {
            worktree: "/tmp/x",
            registered: false,
            backend: Some("bwrap"),
        });
        assert_eq!(
            got,
            Err(ConfineRefusal::UnregisteredWorktree("/tmp/x".into()))
        );
        assert!(got.unwrap_err().to_string().contains("/tmp/x"));
    }

    #[test]
    fn an_unsandboxed_resolution_is_refused_naming_the_backend() {
        for backend in ["host", "systemd", "jobobject", "devcontainer", "", "novel"] {
            let got = check_resolved_open(&ResolvedOpen {
                worktree: "/w/feature",
                registered: true,
                backend: Some(backend),
            });
            let err = got.unwrap_err();
            assert!(matches!(err, ConfineRefusal::Unsandboxed { .. }));
            let msg = err.to_string();
            assert!(
                msg.contains("/w/feature") && msg.contains("sandbox"),
                "{msg}"
            );
        }
        for backend in CONTAINED_BACKENDS.iter().copied() {
            assert_eq!(
                check_resolved_open(&ResolvedOpen {
                    worktree: "/w/feature",
                    registered: true,
                    backend: Some(backend),
                }),
                Ok(())
            );
        }
        // The pre-resolution check (no backend yet) only needs registration.
        assert_eq!(
            check_resolved_open(&ResolvedOpen {
                worktree: "/w/feature",
                registered: true,
                backend: None,
            }),
            Ok(())
        );
    }

    #[test]
    fn contained_backends_are_an_allowlist() {
        assert!(is_contained_backend("bwrap"));
        assert!(is_contained_backend(" podman-rootless "));
        assert!(!is_contained_backend("host"));
        assert!(!is_contained_backend("BWRAP"));
    }

    #[test]
    fn lead_labels_and_rule_ids() {
        assert!(is_lead_label("lead"));
        assert!(is_lead_label("lead:g/0"));
        assert!(!is_lead_label("leader"));
        assert!(!is_lead_label("lead-phone"));
        assert!(!is_lead_label("my lead"));
        assert!(!is_lead_label(""));
        assert!(is_lead_rule_id("lead:nightly"));
        assert!(!is_lead_rule_id("nightly"));
    }

    #[test]
    fn automation_rule_open_actions_name_an_agent_with_a_bounded_prompt() {
        assert_eq!(check_rule_open_action(Some("coder"), None), Ok(()));
        assert_eq!(check_rule_open_action(Some("coder"), Some("go")), Ok(()));
        assert_eq!(
            check_rule_open_action(None, Some("go")),
            Err(ConfineRefusal::AutomationAgent)
        );
        assert_eq!(
            check_rule_open_action(Some(" "), None),
            Err(ConfineRefusal::AutomationAgent)
        );
        let big = "x".repeat(MAX_LEAD_PROMPT_BYTES + 1);
        assert!(matches!(
            check_rule_open_action(Some("coder"), Some(&big)),
            Err(ConfineRefusal::PromptTooLarge { .. })
        ));
    }

    #[test]
    fn every_refusal_renders_with_the_prefix() {
        let all = [
            ConfineRefusal::Verb {
                verb: Verb::Split,
                why: "x",
            },
            ConfineRefusal::Unshaped(Verb::OpenSession),
            ConfineRefusal::RawArgv,
            ConfineRefusal::EnvOverride,
            ConfineRefusal::NoAgent,
            ConfineRefusal::NoWorktree,
            ConfineRefusal::CwdOutsideWorktree,
            ConfineRefusal::Field("fork"),
            ConfineRefusal::PromptTooLarge { len: 1 },
            ConfineRefusal::DispatchSession,
            ConfineRefusal::UnregisteredWorktree("/w".into()),
            ConfineRefusal::Unsandboxed {
                worktree: "/w".into(),
                backend: "host".into(),
            },
            ConfineRefusal::AutomationAgent,
            ConfineRefusal::Input(InputRefusal::Escape),
            ConfineRefusal::NotLeadWorker("abc".into()),
        ];
        for refusal in all {
            let msg = refusal.to_string();
            assert!(msg.starts_with(REFUSAL_PREFIX), "{msg}");
            assert!(msg.len() > REFUSAL_PREFIX.len(), "{msg}");
        }
    }
}
