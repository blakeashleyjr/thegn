//! The coding-agent **harness** provider seam — one object-safe trait carrying
//! every per-vendor fact about a coding-agent CLI ("harness"), so adding a new
//! one (Gemini CLI, OpenCode, Copilot CLI …) is one implementation rather than
//! a sweep across six scattered sites.
//!
//! Before this seam, a harness's knowledge lived in at least six places:
//!   * identity / login / auth marker — `account::PROVIDERS`;
//!   * the headless launch form — `agent_task::headless_command`'s `match`;
//!   * bare-provider resolution — `daemon/agent_open::bare_provider`;
//!   * usage parsing — `usage::parse_claude_usage` / `parse_codex_rollup` / …;
//!   * the session-store layout — the `thegn-svc` usage walkers;
//!   * the sandbox login-carry allowlist.
//!
//! They now converge here. Following `thegn_core::seam` conventions this is an
//! **object-safe** trait (no `async fn` — the provider-trait ratchet), with
//! **caps ⇔ optional ops** (an optional operation is present iff its
//! [`HarnessCaps`] bit is set) and a **closed registry** ([`HARNESSES`]): an id
//! outside it is an error or a declared `reserved` entry — thegn never
//! synthesizes a command for an unknown harness. Config cannot define a harness
//! by supplying commands (that would be arbitrary-command execution from config,
//! and parsers cannot be expressed in TOML anyway); the registry is the closed
//! door and plugins are the future extension.
//!
//! **Substrate-free.** Everything here is pure knowledge and pure parsers
//! (`&[u8] → …`); the filesystem walk, the opt-in live fetch, the spawn, and the
//! doctor probes live in `thegn-svc` / `thegn-host`. The parsers themselves stay
//! in [`crate::usage`] / [`crate::usage_tokens`] (where their fixture tests live)
//! and are *delegated to* here — the seam consolidates the dispatch, not the
//! byte-level logic, so the retrofit is behaviour-identical by construction.

use crate::usage::AccountUsage;
use crate::usage_tokens::TokenRollup;
use crate::util;
use std::collections::HashSet;

// --- capability bits --------------------------------------------------------

/// The optional operations a harness may advertise. Each bit MUST agree with
/// the presence of its op (`caps_agree_with_ops` pins it per impl).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HarnessCaps(u8);

impl HarnessCaps {
    /// Discovers local session transcripts ([`Harness::session_layout`] +
    /// [`Harness::parse_session_summary`]).
    pub const SESSIONS: HarnessCaps = HarnessCaps(1);
    /// Can resume a prior session by id ([`Harness::resume_command`]).
    pub const RESUME: HarnessCaps = HarnessCaps(2);
    /// Parses local rate-limit / usage state ([`Harness::parse_usage`]).
    pub const USAGE: HarnessCaps = HarnessCaps(4);
    /// Folds its transcripts into a host-wide token rollup
    /// ([`Harness::fold_transcript`]).
    pub const TOKENS: HarnessCaps = HarnessCaps(8);
    /// Teammate / native-split sessions with sidebar metadata. **Reserved** —
    /// the on-disk shape is not yet verified well enough to spec discovery, so
    /// no impl advertises it and no op is wired. A follow-up change specs it
    /// against a real teammate session (see the change's design.md).
    pub const TEAMMATES: HarnessCaps = HarnessCaps(16);
    /// Can continue its most recent session in a worktree, id-free
    /// ([`Harness::continue_command`]). The transport-retry relaunch form
    /// (THE-86): unlike `RESUME` it takes no session id — thegn does not track
    /// native session ids for every harness — so the relaunched agent picks
    /// up its own latest session in the worktree.
    pub const CONTINUE: HarnessCaps = HarnessCaps(32);
    /// Can fork a native session into a new native session
    /// ([`Harness::fork_command`]).
    pub const FORK: HarnessCaps = HarnessCaps(64);

    pub const NONE: HarnessCaps = HarnessCaps(0);

    /// Union of the given bits.
    pub const fn of(bits: &[HarnessCaps]) -> HarnessCaps {
        let mut acc = 0u8;
        let mut i = 0;
        while i < bits.len() {
            acc |= bits[i].0;
            i += 1;
        }
        HarnessCaps(acc)
    }

    pub const fn contains(self, bit: HarnessCaps) -> bool {
        self.0 & bit.0 == bit.0 && bit.0 != 0
    }

    /// Stable names for the set bits, for `thegn doctor` and serialization.
    pub fn names(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (bit, name) in [
            (HarnessCaps::SESSIONS, "sessions"),
            (HarnessCaps::RESUME, "resume"),
            (HarnessCaps::USAGE, "usage"),
            (HarnessCaps::TOKENS, "tokens"),
            (HarnessCaps::TEAMMATES, "teammates"),
            (HarnessCaps::CONTINUE, "continue"),
            (HarnessCaps::FORK, "fork"),
        ] {
            if self.contains(bit) {
                out.push(name);
            }
        }
        out
    }
}

impl serde::Serialize for HarnessCaps {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.names().serialize(s)
    }
}

// --- credential home --------------------------------------------------------

/// How a harness locates and proves a login in its credential home. This is the
/// data `account::PROVIDERS` used to hold, now sourced here so the sandbox
/// login-carve, the account switcher, and usage discovery read one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HomeSpec {
    /// Env var that relocates the whole credential home (`CLAUDE_CONFIG_DIR`,
    /// `CODEX_HOME`). Empty when the harness has no relocation var (a fixed
    /// path, e.g. Antigravity) — such a harness is not account-switchable.
    pub home_env: &'static str,
    /// The CLI's default home dir basename under `$HOME` (`.claude`, `.codex`).
    pub default_dir: &'static str,
    /// File whose presence under the home proves a successful login.
    pub auth_marker: &'static str,
    /// The auth-critical files a working login needs, for the sandbox
    /// login-carry allowlist and the doctor probe. The `auth_marker` is always
    /// the first entry.
    pub auth_files: &'static [&'static str],
}

impl HomeSpec {
    /// Whether this home can be relocated to a per-account directory (the
    /// account switcher only lists harnesses for which this is true).
    pub fn is_relocatable(&self) -> bool {
        !self.home_env.is_empty()
    }
}

// --- session store ----------------------------------------------------------

/// The on-disk layout of a harness's local session transcripts, relative to its
/// credential home. Drives the one generic session-store walker in `thegn-svc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLayout {
    /// Subdirectory under the credential home holding the transcripts, walked
    /// recursively (`"sessions"` for Codex, `"projects"` for Claude Code).
    pub store_subdir: &'static str,
    /// A transcript file's name starts with this (`"rollout-"` for Codex, `""`
    /// for Claude Code where the filename is the bare session id).
    pub name_prefix: &'static str,
    /// …and ends with this extension, without the dot (`"jsonl"`).
    pub extension: &'static str,
}

impl SessionLayout {
    /// Whether `filename` is one of this layout's transcript files.
    pub fn matches(&self, filename: &str) -> bool {
        filename.starts_with(self.name_prefix)
            && std::path::Path::new(filename)
                .extension()
                .is_some_and(|e| e == self.extension)
    }

    /// The session id carried by a transcript filename: its stem (the name with
    /// the extension stripped). `None` when the name is not one of ours.
    pub fn session_id(&self, filename: &str) -> Option<String> {
        if !self.matches(filename) {
            return None;
        }
        std::path::Path::new(filename)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
    }
}

/// A one-line, credential-free summary parsed from a transcript's head. Never a
/// transcript body — a single truncated line and the recorded working dir.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSummary {
    /// The working directory the session recorded, mapped to a worktree/project
    /// by the caller. `None` when the transcript head carried none.
    pub cwd: Option<String>,
    /// A truncated single line describing the session (its first user turn).
    pub summary: String,
}

/// One discovered session, as `agent.sessions` projects it. Assembled by the
/// svc walker from a [`SessionLayout`] match plus a [`SessionSummary`] parse.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SessionRecord {
    pub harness: String,
    pub id: String,
    /// The session's worktree/project, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// Last-modified time, Unix seconds.
    pub mtime: i64,
    /// A truncated one-line summary — never transcript bodies or credentials.
    pub summary: String,
    /// `true` when the session's worktree is not one thegn tracks (harness
    /// stores are host-wide; toktrack/codeg show the host-wide view is expected).
    #[serde(default)]
    pub unlinked: bool,
}

// --- the seam ---------------------------------------------------------------

/// One coding-agent CLI. Object-safe (`&dyn Harness`), no `async fn`. Required
/// ops describe identity + launch; optional ops are present iff the matching
/// [`HarnessCaps`] bit is set (`caps_agree_with_ops`).
pub trait Harness: Send + Sync {
    /// Stable registry id (`"claude"`, `"codex"`).
    fn id(&self) -> &'static str;
    /// Human display name (`"Claude"`, `"Codex"`).
    fn display_name(&self) -> &'static str;
    /// The interactive launch program (`"claude"`, `"codex"`). The command a
    /// bare provider id resolves to.
    fn interactive_command(&self) -> &'static str;
    /// argv performing an interactive login into the credential home.
    fn login_argv(&self) -> &'static [&'static str];
    /// The relocatable credential home, when this harness has one. `None` for a
    /// harness with no per-account home (`aider` keeps no login; Antigravity's
    /// token is at a fixed path).
    fn home(&self) -> Option<HomeSpec>;
    /// The headless (non-interactive) command template with a `{prompt}`
    /// placeholder (`"claude -p {prompt} --permission-mode acceptEdits"`), or
    /// `None` when the harness has no headless form thegn knows.
    fn headless_template(&self) -> Option<&'static str>;
    /// How this CLI selects a model: a template with a `{model}` placeholder
    /// (`"--model {model}"`, `"-m {model}"`) that is appended to a launch
    /// command when an `[[agents]]`/stage `model` is configured. `None` when
    /// the harness has no model switch thegn knows — a configured `model` is
    /// then a config error, never silently dropped.
    fn model_flag(&self) -> Option<&'static str> {
        None
    }
    /// The advertised optional-operation bits.
    fn caps(&self) -> HarnessCaps;

    // --- optional ops (present iff the cap bit is set) ---------------------

    /// The local session-store layout (`SESSIONS`).
    fn session_layout(&self) -> Option<SessionLayout> {
        None
    }
    /// Parse a transcript's head into a credential-free summary (`SESSIONS`).
    fn parse_session_summary(&self, _bytes: &[u8]) -> Option<SessionSummary> {
        None
    }
    /// The command that resumes session `id`, id shell-quoted (`RESUME`).
    /// Callers MUST validate the id shape ([`session_id_ok`]) first.
    fn resume_command(&self, _session_id: &str) -> Option<String> {
        None
    }
    /// Parse local usage/rate-limit state into an [`AccountUsage`] (`USAGE`).
    /// `now` is Unix seconds for relative-reset resolution.
    fn parse_usage(&self, _bytes: &[u8], _now: i64) -> Option<AccountUsage> {
        None
    }
    /// The command that continues the harness's own most recent session in the
    /// worktree, with no id argument (`CONTINUE`). The transport-retry
    /// relaunch form: the caller hands the opening message (`continue where
    /// you left off`) as with an interactive-with-task launch. `None` for a
    /// harness with no id-free continue form — those relaunch cold with a
    /// re-rendered stage prompt instead.
    fn continue_command(&self) -> Option<String> {
        None
    }
    /// The command that creates a new native session from `native_session_id`
    /// (`FORK`). Callers MUST validate the id shape ([`session_id_ok`]) first.
    /// The command is vendor-owned and may use a native fork or resume form;
    /// generic code must never guess one.
    fn fork_command(&self, _native_session_id: &str) -> Option<String> {
        None
    }
    /// Fold one transcript's token counters into a host-wide rollup (`TOKENS`).
    fn fold_transcript(
        &self,
        _bytes: &[u8],
        _default_project: &str,
        _seen: &mut HashSet<String>,
        _acc: &mut TokenRollup,
    ) {
    }
}

// --- the closed registry ----------------------------------------------------

static CODEX: Codex = Codex;
static CLAUDE: Claude = Claude;
static AIDER: Aider = Aider;
static ANTIGRAVITY: Antigravity = Antigravity;
static PI: Pi = Pi;

/// The supported harnesses. Closed: an id outside it is an error, never a
/// guessed command. Ordered so the account-switcher projection
/// ([`crate::account::providers`]) keeps its historical `[codex, claude]` order.
pub const HARNESSES: &[&'static dyn Harness] = &[&CODEX, &CLAUDE, &AIDER, &ANTIGRAVITY, &PI];

/// Look a harness up by id. `None` for an id outside the closed registry.
pub fn harness(id: &str) -> Option<&'static dyn Harness> {
    HARNESSES.iter().copied().find(|h| h.id() == id)
}

/// Whether a session id is safe to interpolate into a resume command: non-empty,
/// bounded, and only the characters real harness ids use (uuids, iso stamps).
/// Resume ids cross MCP/HTTP/CLI as untrusted input, so an id that fails this is
/// refused rather than interpolated (the resume command shell-quotes on top).
pub fn session_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

// --- Codex ------------------------------------------------------------------

struct Codex;

impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn display_name(&self) -> &'static str {
        "Codex"
    }
    fn interactive_command(&self) -> &'static str {
        "codex"
    }
    fn login_argv(&self) -> &'static [&'static str] {
        &["codex", "login"]
    }
    fn home(&self) -> Option<HomeSpec> {
        Some(HomeSpec {
            home_env: "CODEX_HOME",
            default_dir: ".codex",
            auth_marker: "auth.json",
            auth_files: &["auth.json"],
        })
    }
    fn headless_template(&self) -> Option<&'static str> {
        Some("codex exec {prompt}")
    }
    fn model_flag(&self) -> Option<&'static str> {
        Some("-m {model}")
    }
    fn caps(&self) -> HarnessCaps {
        HarnessCaps::of(&[
            HarnessCaps::SESSIONS,
            HarnessCaps::RESUME,
            HarnessCaps::USAGE,
            HarnessCaps::FORK,
        ])
    }
    fn session_layout(&self) -> Option<SessionLayout> {
        Some(SessionLayout {
            store_subdir: "sessions",
            name_prefix: "rollout-",
            extension: "jsonl",
        })
    }
    fn parse_session_summary(&self, bytes: &[u8]) -> Option<SessionSummary> {
        parse_codex_session_summary(bytes)
    }
    fn resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!("codex resume {}", util::sh_quote(session_id)))
    }
    fn fork_command(&self, native_session_id: &str) -> Option<String> {
        Some(format!("codex fork {}", util::sh_quote(native_session_id)))
    }
    fn parse_usage(&self, bytes: &[u8], now: i64) -> Option<AccountUsage> {
        crate::usage::parse_codex_rollup(bytes, now)
    }
}

// --- Claude Code ------------------------------------------------------------

struct Claude;

impl Harness for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn display_name(&self) -> &'static str {
        "Claude"
    }
    fn interactive_command(&self) -> &'static str {
        "claude"
    }
    fn login_argv(&self) -> &'static [&'static str] {
        &["claude"]
    }
    fn home(&self) -> Option<HomeSpec> {
        Some(HomeSpec {
            home_env: "CLAUDE_CONFIG_DIR",
            default_dir: ".claude",
            auth_marker: ".credentials.json",
            auth_files: &[".credentials.json", ".claude.json"],
        })
    }
    fn headless_template(&self) -> Option<&'static str> {
        Some("claude -p {prompt} --permission-mode acceptEdits")
    }
    fn model_flag(&self) -> Option<&'static str> {
        Some("--model {model}")
    }
    fn caps(&self) -> HarnessCaps {
        HarnessCaps::of(&[
            HarnessCaps::SESSIONS,
            HarnessCaps::RESUME,
            HarnessCaps::USAGE,
            HarnessCaps::TOKENS,
            HarnessCaps::CONTINUE,
            HarnessCaps::FORK,
        ])
    }
    fn session_layout(&self) -> Option<SessionLayout> {
        Some(SessionLayout {
            store_subdir: "projects",
            name_prefix: "",
            extension: "jsonl",
        })
    }
    fn parse_session_summary(&self, bytes: &[u8]) -> Option<SessionSummary> {
        parse_claude_session_summary(bytes)
    }
    fn resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!("claude --resume {}", util::sh_quote(session_id)))
    }
    fn fork_command(&self, native_session_id: &str) -> Option<String> {
        Some(format!(
            "claude --resume {} --fork-session",
            util::sh_quote(native_session_id)
        ))
    }
    fn continue_command(&self) -> Option<String> {
        // thegn does not hold claude's native session id (`--resume <id>` needs
        // it), so the id-free continue form is the honest relaunch: the CLI
        // itself picks up its latest session in the worktree.
        Some("claude --continue".into())
    }
    fn parse_usage(&self, bytes: &[u8], _now: i64) -> Option<AccountUsage> {
        // Plan/tier are folded by the svc layer from the credentials file (they
        // are not in this body), exactly as before this seam existed.
        crate::usage::parse_claude_usage(bytes, None)
    }
    fn fold_transcript(
        &self,
        bytes: &[u8],
        default_project: &str,
        seen: &mut HashSet<String>,
        acc: &mut TokenRollup,
    ) {
        crate::usage_tokens::fold_transcript(bytes, default_project, seen, acc);
    }
}

// --- aider (headless form only) ---------------------------------------------

struct Aider;

impl Harness for Aider {
    fn id(&self) -> &'static str {
        "aider"
    }
    fn display_name(&self) -> &'static str {
        "Aider"
    }
    fn interactive_command(&self) -> &'static str {
        "aider"
    }
    fn login_argv(&self) -> &'static [&'static str] {
        // aider authenticates via provider API keys in the environment, not a
        // login flow of its own.
        &[]
    }
    fn home(&self) -> Option<HomeSpec> {
        None
    }
    fn headless_template(&self) -> Option<&'static str> {
        Some("aider --yes --message {prompt}")
    }
    fn model_flag(&self) -> Option<&'static str> {
        Some("--model {model}")
    }
    fn caps(&self) -> HarnessCaps {
        HarnessCaps::NONE
    }
}

// --- pi (interactive + headless; models via `provider/id`) ------------------

/// The `pi` coding agent (`@earendil-works/pi-coding-agent`). Headless is
/// `pi -p <prompt>`; a model is `provider/id` (`model-proxy/standard`), which
/// is how a stage is pointed at the local model proxy's tiers. No credential
/// home projection: pi keeps its providers in `~/.pi/agent` (relocatable via
/// `PI_CODING_AGENT_DIR` — an `[[agents]].env` overlay, not an account switch).
struct Pi;

impl Harness for Pi {
    fn id(&self) -> &'static str {
        "pi"
    }
    fn display_name(&self) -> &'static str {
        "Pi"
    }
    fn interactive_command(&self) -> &'static str {
        "pi"
    }
    fn login_argv(&self) -> &'static [&'static str] {
        &["pi", "auth"]
    }
    fn home(&self) -> Option<HomeSpec> {
        None
    }
    fn headless_template(&self) -> Option<&'static str> {
        Some("pi -p {prompt}")
    }
    fn model_flag(&self) -> Option<&'static str> {
        Some("--model {model}")
    }
    fn caps(&self) -> HarnessCaps {
        HarnessCaps::of(&[HarnessCaps::CONTINUE])
    }
    fn continue_command(&self) -> Option<String> {
        // pi's first continue form (THE-86): `pi --continue` picks up the
        // most recent session in the worktree, no id needed.
        Some("pi --continue".into())
    }
}

// --- Antigravity (usage only) -----------------------------------------------

struct Antigravity;

impl Harness for Antigravity {
    fn id(&self) -> &'static str {
        "antigravity"
    }
    fn display_name(&self) -> &'static str {
        "Antigravity"
    }
    fn interactive_command(&self) -> &'static str {
        "antigravity"
    }
    fn login_argv(&self) -> &'static [&'static str] {
        &[]
    }
    fn home(&self) -> Option<HomeSpec> {
        // No relocation env var — the token lives at one fixed path — so it is
        // not account-switchable and gets no `account::Provider` projection.
        None
    }
    fn headless_template(&self) -> Option<&'static str> {
        None
    }
    fn caps(&self) -> HarnessCaps {
        HarnessCaps::USAGE
    }
    fn parse_usage(&self, bytes: &[u8], now: i64) -> Option<AccountUsage> {
        crate::usage::parse_antigravity_quota(bytes, now)
    }
}

// --- pure session-summary parsers -------------------------------------------

/// Collapse to a single truncated display line: first non-empty line, internal
/// whitespace collapsed, capped at `max` chars with an ellipsis. Never carries
/// newlines, so a summary can never smuggle a second line into a listing.
fn one_line(s: &str, max: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let collapsed = collapsed.trim();
    if collapsed.chars().count() <= max {
        return collapsed.to_string();
    }
    let cut: String = collapsed.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// Extract the plain text of a message `content` field, which is either a bare
/// string or Claude's block array (`[{"type":"text","text":"…"}, …]`).
fn message_text(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let blocks = content.as_array()?;
    let mut out = String::new();
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) == Some("text")
            && let Some(t) = b.get("text").and_then(|t| t.as_str())
        {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(t);
        }
    }
    (!out.trim().is_empty()).then_some(out)
}

/// One-line cap for a discovered session's summary.
const SUMMARY_MAX: usize = 120;

/// Parse a Claude Code transcript (`projects/**/<id>.jsonl`): the recorded `cwd`
/// and the first user turn's text. Lenient and best-effort — a transcript being
/// written can have a truncated tail — and never returns transcript bodies.
pub fn parse_claude_session_summary(bytes: &[u8]) -> Option<SessionSummary> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut out = SessionSummary::default();
    let mut found = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        found = true;
        if out.cwd.is_none()
            && let Some(cwd) = v
                .get("cwd")
                .and_then(|c| c.as_str())
                .filter(|s| !s.is_empty())
        {
            out.cwd = Some(cwd.to_string());
        }
        if out.summary.is_empty()
            && v.get("type").and_then(|t| t.as_str()) == Some("user")
            && let Some(content) = v.pointer("/message/content")
            && let Some(t) = message_text(content)
        {
            out.summary = one_line(&t, SUMMARY_MAX);
        }
        if out.cwd.is_some() && !out.summary.is_empty() {
            break;
        }
    }
    found.then_some(out)
}

/// Parse a Codex rollout (`sessions/**/rollout-*.jsonl`): the recorded `cwd`
/// (from a `session_meta` / `turn_context` payload) and the first user message.
/// Lenient — Codex has drifted its record shapes — and credential-free.
pub fn parse_codex_session_summary(bytes: &[u8]) -> Option<SessionSummary> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut out = SessionSummary::default();
    let mut found = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        found = true;
        // Records may be wrapped under `payload` (with a top-level timestamp).
        let payload = v.get("payload").unwrap_or(&v);
        if out.cwd.is_none() {
            for probe in [payload, &v] {
                if let Some(cwd) = probe
                    .get("cwd")
                    .and_then(|c| c.as_str())
                    .filter(|s| !s.is_empty())
                {
                    out.cwd = Some(cwd.to_string());
                    break;
                }
            }
        }
        if out.summary.is_empty() {
            let ty = payload.get("type").and_then(|t| t.as_str());
            if matches!(ty, Some("user_message") | Some("user"))
                && let Some(msg) = payload
                    .get("message")
                    .and_then(|m| m.as_str())
                    .filter(|s| !s.trim().is_empty())
            {
                out.summary = one_line(msg, SUMMARY_MAX);
            }
        }
        if out.cwd.is_some() && !out.summary.is_empty() {
            break;
        }
    }
    found.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- caps bitset -------------------------------------------------------

    #[test]
    fn caps_of_contains_and_names() {
        let c = HarnessCaps::of(&[HarnessCaps::SESSIONS, HarnessCaps::USAGE]);
        assert!(c.contains(HarnessCaps::SESSIONS));
        assert!(c.contains(HarnessCaps::USAGE));
        assert!(!c.contains(HarnessCaps::RESUME));
        assert!(!c.contains(HarnessCaps::TOKENS));
        assert_eq!(c.names(), vec!["sessions", "usage"]);
        // NONE contains nothing, including itself (the empty bit is not "in" a set).
        assert!(!HarnessCaps::NONE.contains(HarnessCaps::NONE));
        assert_eq!(HarnessCaps::NONE.names(), Vec::<&str>::new());
        // Serializes as its names.
        assert_eq!(
            serde_json::to_value(c).unwrap(),
            serde_json::json!(["sessions", "usage"])
        );
    }

    // --- registry ----------------------------------------------------------

    #[test]
    fn registry_is_closed_and_ids_unique() {
        let ids: Vec<&str> = HARNESSES.iter().map(|h| h.id()).collect();
        assert_eq!(ids, vec!["codex", "claude", "aider", "antigravity", "pi"]);
        let set: HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(set.len(), ids.len(), "duplicate harness id");
        assert!(harness("codex").is_some());
        assert!(
            harness("gemini").is_none(),
            "unknown id is refused, not guessed"
        );
        assert!(harness("").is_none());
    }

    /// The caps⇔ops agreement, per impl. An optional op is present exactly when
    /// its bit is set — the seam invariant that keeps `thegn doctor` honest.
    #[test]
    fn caps_agree_with_ops() {
        for h in HARNESSES {
            let caps = h.caps();
            assert_eq!(
                h.session_layout().is_some(),
                caps.contains(HarnessCaps::SESSIONS),
                "{}: SESSIONS bit vs session_layout()",
                h.id()
            );
            // A SESSIONS harness parses a summary out of its own store; a
            // non-SESSIONS one never returns one.
            assert_eq!(
                h.parse_session_summary(b"{\"cwd\":\"/w\"}").is_some(),
                caps.contains(HarnessCaps::SESSIONS),
                "{}: SESSIONS bit vs parse_session_summary()",
                h.id()
            );
            assert_eq!(
                h.resume_command("abc").is_some(),
                caps.contains(HarnessCaps::RESUME),
                "{}: RESUME bit vs resume_command()",
                h.id()
            );
            assert_eq!(
                h.continue_command().is_some(),
                caps.contains(HarnessCaps::CONTINUE),
                "{}: CONTINUE bit vs continue_command()",
                h.id()
            );
            assert_eq!(
                h.fork_command("abc").is_some(),
                caps.contains(HarnessCaps::FORK),
                "{}: FORK bit vs fork_command()",
                h.id()
            );
            // USAGE: a non-USAGE harness never parses usage; a USAGE one is
            // exercised with a real body in its own unit test below.
            if !caps.contains(HarnessCaps::USAGE) {
                assert!(
                    h.parse_usage(b"{\"limits\":[]}", 0).is_none(),
                    "{}: no USAGE bit but parse_usage returned Some",
                    h.id()
                );
            }
        }
    }

    /// Every RESUME impl yields a command containing its id, shell-quoted, and
    /// refuses to interpolate an id that fails the shape check upstream.
    #[test]
    fn every_resume_impl_quotes_its_id() {
        for h in HARNESSES
            .iter()
            .filter(|h| h.caps().contains(HarnessCaps::RESUME))
        {
            let cmd = h.resume_command("sess-123").expect("resume cmd");
            assert!(cmd.contains("sess-123"), "{}: {cmd}", h.id());
            // A metacharacter-laden id is quoted, never free-standing.
            let nasty = h.resume_command("a b; rm -rf /").expect("resume cmd");
            assert!(
                !nasty.contains("; rm -rf / "),
                "{}: unquoted: {nasty}",
                h.id()
            );
        }
    }

    /// Every FORK impl yields a vendor command containing its id, shell-quoted,
    /// and unsupported harnesses leave the operation reserved.
    #[test]
    fn every_fork_impl_quotes_its_id() {
        for h in HARNESSES
            .iter()
            .filter(|h| h.caps().contains(HarnessCaps::FORK))
        {
            let cmd = h.fork_command("sess-123").expect("fork cmd");
            assert!(cmd.contains("sess-123"), "{}: {cmd}", h.id());
            let nasty = h.fork_command("a b; rm -rf /").expect("fork cmd");
            assert!(
                !nasty.contains("; rm -rf / "),
                "{}: unquoted: {nasty}",
                h.id()
            );
        }
        for id in ["aider", "antigravity", "pi"] {
            let h = harness(id).unwrap();
            assert!(!h.caps().contains(HarnessCaps::FORK));
            assert!(h.fork_command("sess-123").is_none());
        }
    }

    /// The CONTINUE impls yield the id-free continue forms (THE-86) — and no
    /// session id is interpolated anywhere, because there is no id to hold.
    #[test]
    fn continue_impls_are_id_free_forms() {
        assert_eq!(
            harness("claude").unwrap().continue_command().as_deref(),
            Some("claude --continue")
        );
        assert_eq!(
            harness("pi").unwrap().continue_command().as_deref(),
            Some("pi --continue")
        );
        // Harnesses without an id-free continue form advertise no CONTINUE bit
        // (`caps_agree_with_ops` pins bit ⇔ op) — they relaunch cold.
        for id in ["codex", "aider", "antigravity"] {
            assert!(
                harness(id).unwrap().continue_command().is_none(),
                "{id} must have no continue form"
            );
        }
    }

    #[test]
    fn session_id_shape_is_strict() {
        assert!(session_id_ok("2026-08-25T10_00_00-abc.def"));
        assert!(session_id_ok("0c1f2e3d-uuid"));
        assert!(!session_id_ok(""));
        assert!(!session_id_ok("has space"));
        assert!(!session_id_ok("with/slash"));
        assert!(!session_id_ok("semi;colon"));
        assert!(!session_id_ok(&"x".repeat(257)));
    }

    // --- home / login facts ------------------------------------------------

    #[test]
    fn home_facts_match_the_pre_seam_provider_table() {
        // These are the exact facts `account::PROVIDERS` held before the seam.
        let codex = harness("codex").unwrap().home().unwrap();
        assert_eq!(codex.home_env, "CODEX_HOME");
        assert_eq!(codex.default_dir, ".codex");
        assert_eq!(codex.auth_marker, "auth.json");
        assert!(codex.is_relocatable());
        assert_eq!(harness("codex").unwrap().login_argv(), &["codex", "login"]);

        let claude = harness("claude").unwrap().home().unwrap();
        assert_eq!(claude.home_env, "CLAUDE_CONFIG_DIR");
        assert_eq!(claude.default_dir, ".claude");
        assert_eq!(claude.auth_marker, ".credentials.json");
        assert_eq!(harness("claude").unwrap().login_argv(), &["claude"]);

        // The auth marker is always the first auth-critical file.
        for h in HARNESSES
            .iter()
            .filter_map(|h| h.home().map(|s| (h.id(), s)))
        {
            assert_eq!(
                h.1.auth_files.first().copied(),
                Some(h.1.auth_marker),
                "{}",
                h.0
            );
        }

        // aider and antigravity have no relocatable home (not account-switchable).
        assert!(harness("aider").unwrap().home().is_none());
        assert!(harness("antigravity").unwrap().home().is_none());
    }

    #[test]
    fn headless_templates_match_the_pre_seam_match_arms() {
        assert_eq!(
            harness("claude").unwrap().headless_template(),
            Some("claude -p {prompt} --permission-mode acceptEdits")
        );
        assert_eq!(
            harness("codex").unwrap().headless_template(),
            Some("codex exec {prompt}")
        );
        assert_eq!(
            harness("aider").unwrap().headless_template(),
            Some("aider --yes --message {prompt}")
        );
        // Antigravity is a usage-only harness with no headless launch form.
        assert_eq!(harness("antigravity").unwrap().headless_template(), None);
    }

    /// Every declared headless template must be a valid command template — the
    /// same contract `agent_task` enforces on configured commands.
    #[test]
    fn headless_templates_are_valid_command_templates() {
        use crate::agent_task::{COMMAND_VARS, validate_template};
        for h in HARNESSES {
            if let Some(t) = h.headless_template() {
                assert_eq!(
                    validate_template(t, COMMAND_VARS, true),
                    Ok(()),
                    "{}: headless template is not a valid command template: {t}",
                    h.id()
                );
            }
        }
    }

    /// Every model flag is a template over exactly `{model}`, and every
    /// harness with a headless form can be pointed at a model (a stage `model`
    /// on a launchable harness must never be a silent no-op).
    #[test]
    fn model_flags_are_model_templates() {
        for h in HARNESSES {
            if let Some(t) = h.model_flag() {
                assert!(
                    t.contains("{model}"),
                    "{}: model flag lacks {{model}}: {t}",
                    h.id()
                );
                assert!(
                    !t.contains("{prompt}"),
                    "{}: model flag must not take the prompt",
                    h.id()
                );
            }
            if h.headless_template().is_some() {
                assert!(
                    h.model_flag().is_some(),
                    "{}: launchable but no model flag",
                    h.id()
                );
            }
        }
        assert_eq!(
            harness("pi").unwrap().headless_template(),
            Some("pi -p {prompt}")
        );
        assert_eq!(harness("codex").unwrap().model_flag(), Some("-m {model}"));
    }

    // --- session layout ----------------------------------------------------

    #[test]
    fn session_layout_matches_its_own_fixture() {
        let codex = harness("codex").unwrap().session_layout().unwrap();
        assert_eq!(codex.store_subdir, "sessions");
        assert!(codex.matches("rollout-2026-08-12T10-00-00-abc.jsonl"));
        assert!(
            !codex.matches("history.jsonl"),
            "non-rollout is not a session"
        );
        assert!(!codex.matches("rollout-2026.txt"), "wrong extension");
        assert_eq!(
            codex
                .session_id("rollout-2026-08-12T10-00-00-abc.jsonl")
                .as_deref(),
            Some("rollout-2026-08-12T10-00-00-abc")
        );

        let claude = harness("claude").unwrap().session_layout().unwrap();
        assert_eq!(claude.store_subdir, "projects");
        assert!(claude.matches("0c1f-uuid.jsonl"));
        assert!(!claude.matches("notes.md"));
        assert_eq!(
            claude.session_id("0c1f-uuid.jsonl").as_deref(),
            Some("0c1f-uuid")
        );
        assert_eq!(claude.session_id("notes.md"), None);
    }

    // --- session summaries (pure parsers) ----------------------------------

    #[test]
    fn claude_session_summary_extracts_cwd_and_first_user_turn() {
        let lines = [
            r#"{"type":"summary","summary":"prior"}"#,
            r#"{"type":"user","cwd":"/home/u/code/thegn","message":{"role":"user","content":"Fix the flaky test in usage.rs please"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"sure"}]}}"#,
        ];
        let s = parse_claude_session_summary(lines.join("\n").as_bytes()).unwrap();
        assert_eq!(s.cwd.as_deref(), Some("/home/u/code/thegn"));
        assert_eq!(s.summary, "Fix the flaky test in usage.rs please");
    }

    #[test]
    fn claude_summary_reads_content_blocks_and_truncates() {
        let long = "x".repeat(400);
        let line = format!(
            r#"{{"type":"user","cwd":"/w","message":{{"content":[{{"type":"text","text":"{long}"}}]}}}}"#
        );
        let s = parse_claude_session_summary(line.as_bytes()).unwrap();
        assert!(s.summary.ends_with('…'));
        assert!(s.summary.chars().count() <= SUMMARY_MAX);
    }

    #[test]
    fn codex_session_summary_extracts_cwd_and_user_message() {
        let lines = [
            r#"{"timestamp":"t","type":"session_meta","payload":{"cwd":"/srv/app","id":"x"}}"#,
            r#"{"timestamp":"t","type":"event_msg","payload":{"type":"user_message","message":"Add a --json flag"}}"#,
        ];
        let s = parse_codex_session_summary(lines.join("\n").as_bytes()).unwrap();
        assert_eq!(s.cwd.as_deref(), Some("/srv/app"));
        assert_eq!(s.summary, "Add a --json flag");
    }

    #[test]
    fn session_summary_parsers_degrade_on_junk() {
        // Not JSON at all → None (nothing found).
        assert!(parse_claude_session_summary(b"not json\n").is_none());
        assert!(parse_codex_session_summary(b"").is_none());
        // JSON with no cwd/user turn → Some, but empty fields.
        let s = parse_claude_session_summary(br#"{"type":"system"}"#).unwrap();
        assert!(s.cwd.is_none() && s.summary.is_empty());
    }

    #[test]
    fn one_line_collapses_and_truncates() {
        assert_eq!(one_line("  hello   world \n", 40), "hello world");
        assert_eq!(one_line("a\nb\nc", 40), "a b c");
        assert_eq!(one_line(&"ab".repeat(100), 5), "abab…");
        assert_eq!(one_line("", 5), "");
    }

    // --- per-impl usage parity (delegation is behaviour-identical) ---------

    #[test]
    fn codex_usage_delegates_to_the_pure_parser() {
        let line = r#"{"payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":42.0,"window_minutes":300}}}}"#;
        let via_seam = harness("codex").unwrap().parse_usage(line.as_bytes(), 1000);
        let direct = crate::usage::parse_codex_rollup(line.as_bytes(), 1000);
        assert_eq!(via_seam, direct);
        assert!(via_seam.is_some(), "the fixture has rate-limit data");
    }

    #[test]
    fn claude_usage_delegates_to_the_pure_parser() {
        let body = r#"{"limits":[{"kind":"session","percent":80.0}]}"#;
        let via_seam = harness("claude").unwrap().parse_usage(body.as_bytes(), 0);
        let direct = crate::usage::parse_claude_usage(body.as_bytes(), None);
        assert_eq!(via_seam, direct);
        assert!(via_seam.is_some());
    }

    #[test]
    fn antigravity_usage_delegates_to_the_pure_parser() {
        let body = r#"{"quotas":[{"name":"daily","usedPercent":10.0}]}"#;
        let via_seam = harness("antigravity")
            .unwrap()
            .parse_usage(body.as_bytes(), 5);
        let direct = crate::usage::parse_antigravity_quota(body.as_bytes(), 5);
        assert_eq!(via_seam, direct);
        assert!(via_seam.is_some());
    }

    #[test]
    fn claude_fold_transcript_delegates_to_the_pure_fold() {
        let line = r#"{"requestId":"r1","message":{"id":"m1","model":"claude","usage":{"input_tokens":10,"output_tokens":2}}}"#;
        let mut seen_a = HashSet::new();
        let mut acc_a = TokenRollup::default();
        harness("claude").unwrap().fold_transcript(
            line.as_bytes(),
            "proj",
            &mut seen_a,
            &mut acc_a,
        );
        let mut seen_b = HashSet::new();
        let mut acc_b = TokenRollup::default();
        crate::usage_tokens::fold_transcript(line.as_bytes(), "proj", &mut seen_b, &mut acc_b);
        assert_eq!(acc_a, acc_b);
        assert_eq!(acc_a.total.input, 10);
    }

    #[test]
    fn session_record_serde_round_trips() {
        let rec = SessionRecord {
            harness: "claude".into(),
            id: "abc".into(),
            worktree: Some("/w".into()),
            mtime: 123,
            summary: "hi".into(),
            unlinked: true,
        };
        let j = serde_json::to_string(&rec).unwrap();
        let back: SessionRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(back, rec);
    }
}
