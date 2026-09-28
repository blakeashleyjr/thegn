//! The `[[pipeline.stages]]` config family — the **org chart** for a multi-stage
//! agent pipeline (architect → code → review → land).
//!
//! # Structure, not judgment
//!
//! This table is **declarative data a supervising agent reads**, never a
//! scheduler thegn runs. No code path in thegn advances `next`, enforces
//! `concurrency`, or fires `timeout_secs`: the Lead agent reads the whole
//! structure with `thegn config get pipeline --json` and executes it with its
//! own judgment, exactly as `add-agent-orchestration-surface` decided when it
//! **rejected a native drain driver** ("every driver feature hard-codes
//! judgement the prompt should own"). thegn's two jobs here are
//! (a) **validate** the structure at `config validate` time and (b) **display**
//! it (stage grouping/labels on the dispatch board). Delete the section and
//! nothing thegn *does* changes — only what it can check and show.
//!
//! Everything here is **pure** (no I/O, no host types): the shape, the two
//! validation channels, and the reachability fold. Resolution of a stage's
//! `agent` reuses the `[[presets]]` classifier
//! ([`crate::config_presets::classify_command`]) so a pipeline never introduces
//! a second program registry, plus the same bare-harness carve-out the agent
//! launch path applies (`daemon/agent_open::bare_provider`).

use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use crate::config::{Config, NamedCommand, config_enum, config_warn};

config_enum! {
    /// What the Lead does with a stage row that raised its hand (the worker
    /// asked a question, or its `timeout_secs` budget elapsed). Advisory: the
    /// supervising agent reads it and acts; thegn never takes the action.
    pub enum OnBlocked: "pipeline on_blocked" {
        Park = "park" | "waiting_human" | "wait",
        Escalate = "escalate" | "notify",
        Abandon = "abandon" | "drop",
    } default = Park;
}

/// One `[[pipeline.stages]]` entry — a step in the org chart.
///
/// Carries no provider/sandbox semantics of its own: `agent` names an
/// `[[agents]]`/`[[tools]]` entry (or a bare harness id), and *that* entry
/// brings the command, provisioning and proxy routing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PipelineStage {
    /// Stable stage name — what a roster row's `stage` column records and what
    /// another stage's `next` points at. Unique; required.
    pub name: String,
    /// The `[[agents]]`/`[[tools]]` entry that runs this stage (a bare harness
    /// id such as `claude` also resolves, matching the agent-launch path).
    pub agent: String,
    /// The prompt template handed to this stage's worker. Placeholders are
    /// checked at `config validate` time against
    /// [`crate::agent_task::STAGE_VARS`]; **rendering is the Lead's job** —
    /// thegn never expands this itself.
    pub prompt: String,
    /// How many workers of this stage may run at once. The Lead still decides
    /// what work to dispatch, while the atomic claim and `session open --stage`
    /// enforce this ceiling from the roster's active rows. `0` is a config error
    /// (a stage that can never run is a typo, not a way to disable one).
    pub concurrency: u32,
    /// How long the Lead should wait on this stage's session before treating it
    /// as blocked, in seconds. **Advisory — thegn never fires this timer**; the
    /// Lead passes it to `thegn session wait --timeout` (milliseconds), which is
    /// the only watchdog that exists.
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
    pub timeout_secs: u64,
    /// The stage the Lead advances to when this one finishes. Unset = terminal.
    /// Advisory: no thegn code path follows this edge.
    pub next: Option<String>,
    /// What the Lead does with a blocked or timed-out row of this stage.
    pub on_blocked: OnBlocked,
    /// Per-stage harness override (`claude` | `codex` | `pi` | `aider`): this
    /// stage launches that harness's own command instead of the entry's, with
    /// the stage's (or entry's) `model` rendered through *its* flag. A stage
    /// is a generic role — this is how one chart mixes harnesses per stage.
    pub harness: Option<String>,
    /// Per-stage model override, rendered through the agent's harness model
    /// flag (`[[agents]].model` is the default). Lets one entry run a cheap
    /// tier for coders and a strong one for reviewers.
    pub model: Option<String>,
    /// Per-stage environment overlay, layered key-by-key over the agent
    /// entry's `env` (same `env:`/`file:` secret expansion).
    pub env: BTreeMap<String, String>,
    /// Per-stage headless tool allow-list; replaces the agent entry's
    /// `permissions` when non-empty. The effective list rides the launch
    /// command through the harness's command-scoped grant (claude:
    /// `--settings`) — nothing is written into the worktree — and a harness
    /// without one refuses the dispatch before a roster row exists. thegn does
    /// NOT interpret the strings — they are the harness's own vocabulary
    /// (`Bash(git status:*)`, `Read`, `mcp__srv__tool`). Empty = the entry's
    /// list, if any.
    pub permissions: Vec<String>,
    /// `[[tasks]]` entry names to run against this stage's lane once its worker
    /// exits — the compile/test/lint pass the worker could not run itself (the
    /// pipeline sandbox mounts the Nix store read-only, so `nix develop` cannot
    /// materialise a shell inside it, and a worker's "implementation-ready"
    /// therefore means *source-reviewed, never built*).
    ///
    /// Names entries, **not shell commands** — the same closed-registry rule
    /// [`PipelineStage::agent`] follows, and for the same reason: a stage must
    /// not be able to introduce arbitrary command execution from config. The
    /// named task owns its own scoping, caps and output matcher.
    ///
    /// Empty = this stage's output is not machine-checkable, which is the right
    /// answer for a planning stage that produces prose.
    pub validate: Vec<String>,
    /// What must already be a **recorded fact** before the supervisor may
    /// dispatch INTO this stage. A closed vocabulary — see [`Requirement`] —
    /// checked at config-validate time rather than discovered at dispatch.
    ///
    /// This is where a review gate is expressed. `requires = ["approval"]` says
    /// "a person reads the parent's output before this stage starts", and
    /// because it is data, a chart documents its own gates instead of relying on
    /// a supervisor's restraint.
    ///
    /// Empty = the supervisor may advance into this stage as soon as its parent
    /// row is closed.
    pub requires: Vec<String>,
}

/// A precondition a stage transition must satisfy before the supervisor may
/// dispatch it.
///
/// Closed on purpose. Each variant names a fact that is **recorded** somewhere
/// checkable — git, the roster, the validation table, the approval table — so
/// that deciding whether a transition may happen is a lookup rather than a
/// judgement. A requirement the supervisor could satisfy only by forming an
/// opinion does not belong in this enum, and that line is what the whole
/// supervisor design rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    /// The parent row's handoff artifact exists, is committed in `HEAD`, and is
    /// unchanged at that path — what `dispatch verify` reports.
    ParentArtifact,
    /// The parent row carries a worker report — what the done-gate checks.
    ParentReport,
    /// Every `[[tasks]]` entry in the parent stage's `validate` recorded
    /// [`crate::pipeline_validate::ValidationClass::Green`] for the parent's
    /// current tip. An environment error does **not** satisfy this: "the
    /// command never ran" is not "the code passed".
    ValidationGreen,
    /// A live, commit-bound approval exists for the parent stage's output at the
    /// lane's current tip (see [`crate::pipeline_approval`]).
    Approval,
}

impl Requirement {
    /// Every requirement, in the order they are reported.
    pub const ALL: [Requirement; 4] = [
        Self::ParentArtifact,
        Self::ParentReport,
        Self::ValidationGreen,
        Self::Approval,
    ];

    /// The stable config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ParentArtifact => "parent_artifact",
            Self::ParentReport => "parent_report",
            Self::ValidationGreen => "validation:green",
            Self::Approval => "approval",
        }
    }

    /// Parse a configured requirement. `None` for anything outside the set — a
    /// typo must be an error naming the known values, never a requirement that
    /// silently never applies, which would read as a gate present in the file
    /// and absent in effect.
    pub fn parse(s: &str) -> Option<Requirement> {
        let s = s.trim();
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }

    /// The known values, comma-separated, for an error message.
    pub fn known_values() -> String {
        Self::ALL
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl Default for PipelineStage {
    fn default() -> Self {
        Self {
            name: String::new(),
            agent: String::new(),
            prompt: String::new(),
            concurrency: default_concurrency(),
            timeout_secs: default_timeout_secs(),
            next: None,
            on_blocked: OnBlocked::default(),
            harness: None,
            model: None,
            env: BTreeMap::new(),
            permissions: Vec::new(),
            validate: Vec::new(),
            requires: Vec::new(),
        }
    }
}

impl PipelineStage {
    /// This stage's parsed requirements, dropping anything unrecognised.
    /// Callers that must *report* a bad value run [`Requirement::parse`] over
    /// the raw strings instead: validation names the typo, execution ignores it
    /// — having already refused to run a config that carries one.
    pub fn parsed_requires(&self) -> Vec<Requirement> {
        self.requires
            .iter()
            .filter_map(|r| Requirement::parse(r))
            .collect()
    }

    /// The `[[tasks]]` names this stage validates with, trimmed and non-empty.
    pub fn validate_tasks(&self) -> Vec<&str> {
        self.validate
            .iter()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .collect()
    }
}

/// One worker at a time — the conservative default for a stage that forgets to
/// say (a fan-out is opt-in, never inherited).
const fn default_concurrency() -> u32 {
    1
}

/// One hour. Long enough for a real implementation turn, short enough that a
/// wedged worker surfaces the same day. Advisory only.
const fn default_timeout_secs() -> u64 {
    3600
}

/// `[pipeline]` — the declarative stage list. Empty by default: with no stages
/// configured every surface that reads this is simply inert, and the AI-free
/// shell behaves exactly as before.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Pipeline {
    /// The stages, in declaration order. The **first** entry is the entry point
    /// (the stage a new issue starts at); order is otherwise only a display
    /// convention — the edges are `next`.
    pub stages: Vec<PipelineStage>,
    /// `[pipeline.transport_retry]` — whether the daemon may relaunch a headless
    /// stage worker that died of a transport failure (THE-86). Structure, not
    /// judgment like the rest of the section: the daemon writes only
    /// `waiting_human` + a `note` (it can park a row, never finish one), and
    /// the retry budget/backoff/lists are data an operator tunes.
    pub transport_retry: TransportRetry,
    /// `[pipeline.supervisor]` — the mechanical half of the chart, run
    /// durably. Off by default; see [`Supervisor`].
    pub supervisor: Supervisor,
}

/// `[pipeline.supervisor]` — whether thegn itself performs the parts of a
/// pipeline that are **rules rather than judgement**, and which of them.
///
/// # Why this does not contradict "structure, not judgment"
///
/// This module's header states that no thegn code path advances `next`, and
/// that was a deliberate rejection of a native drain driver ("every driver
/// feature hard-codes judgement the prompt should own"). The supervisor does
/// not weaken that rule; it applies the exception the daemon's reaper already
/// carved, which is that a transition may be applied when *"applying it is
/// arithmetic on recorded facts, not a judgement about whether the work was any
/// good"* (`daemon/pipeline_reaper.rs`).
///
/// Everything the supervisor may do is gated on facts that are already written
/// down: an artifact committed in git, a report on the roster, a recorded
/// validation class, a commit-bound approval. The judgement — *should this
/// advance?* — is expressed by the operator in each stage's
/// [`PipelineStage::requires`], in the config file, in advance. The supervisor
/// never forms one, and in particular has no path that can create an approval:
/// granting is an operator-scoped write on a surface the daemon does not hold.
///
/// With `enabled = false` (the default) nothing here runs and the pipeline
/// behaves exactly as it did before the section existed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Supervisor {
    /// Master switch. `false` (the default) = the supervisor never acts;
    /// `thegn supervise plan` still reports what it *would* do, which is how an
    /// operator reads the chart's behaviour before trusting it with it.
    pub enabled: bool,
    /// Run each stage's `validate` tasks when its worker exits, and record what
    /// came back. The one capability that is pure observation — it writes no
    /// roster status and dispatches nothing.
    pub validate_on_exit: bool,
    /// Dispatch a stage's `next` once every requirement that stage declares is
    /// a recorded fact. `false` = validation still runs and is recorded, but
    /// every transition stays an explicit act by a person or an agent.
    pub advance: bool,
    /// Enqueue a terminal stage's lane onto the merge queue once its
    /// requirements are met. The supervisor never lands anything itself: the
    /// queue owns serialization and the fold gate, so this is a hand-off, not a
    /// second landing path.
    pub land: bool,
    /// How long a recorded approval stays valid, in seconds. `0` = no age
    /// bound. An approval is always **also** invalidated by a new commit on the
    /// lane, which is the bound that actually matters; this one only stops a
    /// long-forgotten approval from authorizing work after the context around
    /// it has moved on.
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
    pub approval_ttl_secs: u64,
    /// How many validation runs may be in flight at once, across every lane.
    /// `0` = follow `[limits] test_max_parallel`.
    ///
    /// The default of one is not timidity: a full-workspace compile is the most
    /// expensive thing this tool does, several at once is what pins every core
    /// and drives the box into swap, and — worse — a gate run under that
    /// contention goes red for reasons that have nothing to do with the code,
    /// which is precisely the false verdict [`crate::pipeline_validate`] exists
    /// to avoid emitting.
    pub max_validations: u32,
    /// What must be a recorded fact about a **terminal** stage's row before its
    /// lane may be handed to the merge queue.
    ///
    /// Landing is one act at the end of the chart rather than a stage, so it
    /// carries its own gate instead of borrowing a stage's `requires`. Same
    /// closed vocabulary ([`Requirement`]), evaluated against the terminal row
    /// itself — the row whose output is being consumed, which for a land is the
    /// lane's final commit.
    ///
    /// Defaults to requiring an approval, and that default is the point:
    /// switching the supervisor on must not, by itself, start landing code
    /// nobody has read.
    pub land_requires: Vec<String>,
}

impl Supervisor {
    /// The parsed `land_requires`, dropping anything unrecognised — validation
    /// names the typo, execution ignores it, having already refused to run a
    /// config that carries one.
    pub fn parsed_land_requires(&self) -> Vec<Requirement> {
        self.land_requires
            .iter()
            .filter_map(|r| Requirement::parse(r))
            .collect()
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self {
            enabled: false,
            validate_on_exit: true,
            advance: true,
            land: true,
            approval_ttl_secs: default_approval_ttl_secs(),
            max_validations: 1,
            land_requires: vec![Requirement::Approval.as_str().to_string()],
        }
    }
}

/// One day. Long enough that an approval survives an overnight queue, short
/// enough that a forgotten one does not authorize work a week later.
const fn default_approval_ttl_secs() -> u64 {
    86_400
}

impl Supervisor {
    /// The effective validation concurrency, resolving `0` against the
    /// machine-wide `[limits] test_max_parallel` and never returning zero (a
    /// ceiling of zero would mean nothing ever validates, which is a stall, not
    /// a setting).
    pub fn effective_max_validations(&self, limits_test_max_parallel: usize) -> usize {
        if self.max_validations > 0 {
            self.max_validations as usize
        } else {
            limits_test_max_parallel.max(1)
        }
    }

    /// Whether any capability is switched on. A section that is `enabled` with
    /// every capability off does nothing, which validation reports.
    pub fn any_capability(&self) -> bool {
        self.validate_on_exit || self.advance || self.land
    }
}

/// `[pipeline.transport_retry]` — the daemon-side auto-retry for headless
/// dispatch workers killed by a transport failure (a connection error, a
/// provider outage). Every outcome parks the row `waiting_human` with a `note`
/// and never writes `done`/`failed`; a usage **limit** parks immediately at
/// any attempt — spending money is never the daemon's call.
///
/// Nested table ⇒ depth 2 ⇒ deliberately out of env-overlay scope, and the
/// hm module does not render `[pipeline]`, so there is no nix mirror to keep
/// in step.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct TransportRetry {
    /// Master switch. `false` restores today's behavior: a dead headless
    /// worker stays whatever the supervisor wrote, and nothing auto-relaunches.
    pub enabled: bool,
    /// Bounded relaunches per roster row. A transport failure beyond this
    /// parks the row with an `exhausted` note instead of retrying again.
    pub max_attempts: u32,
    /// Base backoff in ms; doubles per attempt (`base * 2^(n-1)`), capped at
    /// 60 s by [`crate::pipeline_exit::MAX_BACKOFF_MS`].
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_MILLIS"))]
    pub backoff_ms: u64,
    /// Substrings (case-insensitive) that classify a dead worker's final
    /// screen as a retryable transport failure. **Replaces** the default list
    /// — it does not extend it.
    pub transport_signatures: Vec<String>,
    /// Substrings that classify the screen as a usage limit (park, never
    /// retry). Replaces the default list, like `transport_signatures`.
    pub limit_signatures: Vec<String>,
}

impl Default for TransportRetry {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: 3,
            backoff_ms: 2_000,
            transport_signatures: crate::pipeline_exit::DEFAULT_TRANSPORT_SIGNATURES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            limit_signatures: crate::pipeline_exit::DEFAULT_LIMIT_SIGNATURES
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl PipelineStage {
    /// The trimmed stage name, or `None` when unset.
    pub fn stage_name(&self) -> Option<&str> {
        let n = self.name.trim();
        (!n.is_empty()).then_some(n)
    }

    /// The trimmed `next` target, or `None` when this stage is terminal.
    pub fn next_name(&self) -> Option<&str> {
        self.next
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
    }
}

impl Pipeline {
    /// Look up a stage by name (first wins — duplicates are a hard validation
    /// error, so this only differs from "the" stage in an invalid config).
    pub fn stage(&self, name: &str) -> Option<&PipelineStage> {
        let n = name.trim();
        self.stages.iter().find(|s| s.name.trim() == n)
    }

    /// The configured stage names, in declaration order (board column order,
    /// candidate lists).
    pub fn stage_names(&self) -> Vec<String> {
        self.stages
            .iter()
            .filter_map(|s| s.stage_name().map(str::to_string))
            .collect()
    }

    /// The entry stage — the first named one. `None` for an empty pipeline.
    pub fn entry(&self) -> Option<&PipelineStage> {
        self.stages.iter().find(|s| s.stage_name().is_some())
    }
}

/// Does this stage's `agent` name something launchable?
///
/// Two tiers, matching what the agent-launch path actually accepts:
///  1. an exact `[[agents]]`/`[[tools]]` entry — the picker's resolution, via
///     [`crate::config_presets::classify_command`] (a `PresetCommand::Named`);
///  2. a **bare harness id** (`claude`, `codex`, …) — the same closed-registry
///     carve-out `daemon/agent_open::bare_provider` applies, so a Lead that
///     says `claude` on a machine whose config calls that entry something else
///     still resolves. The filter is copied from that function verbatim: a
///     harness qualifies only if it is launchable (has a headless form or a
///     home layout).
///
/// A raw shell command is deliberately NOT accepted: a stage worker is
/// supervised through `session open --agent`, which takes a registry name.
pub fn stage_agent_resolves(agent: &str, agents: &[NamedCommand], tools: &[NamedCommand]) -> bool {
    use crate::config_presets::{PresetCommand, classify_command};
    let a = agent.trim();
    if a.is_empty() {
        return false;
    }
    if matches!(classify_command(a, agents, tools), PresetCommand::Named(_)) {
        return true;
    }
    crate::harness::harness(a)
        .filter(|h| h.headless_template().is_some() || h.home().is_some())
        .is_some()
}

/// The indexed error label a stage's problems are reported under, so a message
/// points at a line in the file rather than at "a stage".
fn label(i: usize, s: &PipelineStage) -> String {
    match s.stage_name() {
        Some(n) => format!("pipeline.stages[{i}] ({n:?})"),
        None => format!("pipeline.stages[{i}]"),
    }
}

/// The first index carrying `name` (duplicates are an error, so this is *the*
/// index in any valid config).
fn index_of(stages: &[PipelineStage], name: &str) -> Option<usize> {
    stages.iter().position(|s| s.name.trim() == name)
}

/// The substring a stage prompt must contain to be teaching its worker the
/// report half of the handoff. Matched literally (not as a placeholder) because
/// what matters is that the prompt *names the command* — a worker that is never
/// told to run it never files one.
const REPORT_VERB: &str = "dispatch report";

/// Why a stage prompt cannot produce a closable row.
///
/// # The failure this exists to prevent
///
/// `pipeline_run::verify` — the gate behind `dispatch set-status … done` —
/// requires BOTH a git-tracked artifact and a worker report. A stage whose
/// prompt never tells the worker its row id, or never tells it to run
/// `thegn dispatch report`, therefore produces rows that **can never close
/// without `--force`**: the worker does the work, commits the artifact, exits
/// 0, and the row stays `running` forever. A supervisor that reads liveness
/// from sessions rather than rows then sees a free slot and dispatches again,
/// and the roster grows without bound.
///
/// That is not hypothetical: it is exactly how a 121-row backlog accumulated
/// on 2026-08-29 after the report requirement landed (THE-88) while the
/// deployed `[[pipeline.stages]]` prompts were left on their pre-THE-88 text.
/// The config was individually valid at every other check — which is why this
/// one exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractGap {
    /// The prompt never references `{row}`, so the worker cannot name itself to
    /// `dispatch report` even if it wanted to.
    NoRowPlaceholder,
    /// The prompt never mentions `thegn dispatch report`, so the worker is
    /// never asked for the report the done-gate requires.
    NoReportInstruction,
}

impl ContractGap {
    /// The operator-facing remedy — one actionable sentence, not a diagnosis.
    pub fn remedy(self) -> &'static str {
        match self {
            Self::NoRowPlaceholder => {
                "the prompt never references {row}, so the worker cannot know which roster row \
                 it is filing against — add `You are dispatch row {row}.` and have it run \
                 `thegn dispatch report {row} --text '…'`"
            }
            Self::NoReportInstruction => {
                "the prompt never tells the worker to run `thegn dispatch report`, but the \
                 done-gate requires a report — such a row can only ever be closed with \
                 `--force`; add the report step to the prompt"
            }
        }
    }
}

/// Which halves of the handoff contract a stage prompt is missing.
///
/// Pure and placeholder-aware: `{row}` is detected through the same parser the
/// renderer uses ([`crate::agent_task::template_vars`]), so `{ row }` counts and
/// an escaped `{{row}}` correctly does not.
pub fn stage_contract_gaps(stage: &PipelineStage) -> Vec<ContractGap> {
    let mut out = Vec::new();
    let refs_row = crate::agent_task::template_vars(&stage.prompt)
        .map(|vs| vs.iter().any(|v| v == "row"))
        .unwrap_or(false);
    if !refs_row {
        out.push(ContractGap::NoRowPlaceholder);
    }
    if !stage.prompt.contains(REPORT_VERB) {
        out.push(ContractGap::NoReportInstruction);
    }
    out
}

/// Strict validation for `thegn config validate` (errors only — these fail the
/// command): a stage must be nameable, uniquely, runnable by a resolvable
/// agent, with a concurrency budget above zero and a `next` edge that names a
/// real stage and does not close a loop. Soft observations (an unreachable
/// stage) are a separate channel: [`pipeline_warnings`].
///
/// Prompt-template placeholders are checked separately, in
/// `config_validate::check_templates`, against [`crate::agent_task::STAGE_VARS`]
/// (that pass owns every template in the file).
pub fn validate_pipeline(cfg: &Config) -> Vec<String> {
    let stages = &cfg.pipeline.stages;
    let mut out = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (i, s) in stages.iter().enumerate() {
        let label = label(i, s);
        match s.stage_name() {
            None => out.push(format!(
                "pipeline.stages[{i}].name: required (a stage is referenced by name — \
                 roster rows record it and `next` points at it)"
            )),
            Some(n) => {
                if seen.contains(&n) {
                    out.push(format!(
                        "{label}: duplicate stage name — every stage name must be unique"
                    ));
                } else {
                    seen.push(n);
                }
            }
        }
        if s.agent.trim().is_empty() {
            out.push(format!(
                "{label}.agent: required (the [[agents]]/[[tools]] entry that runs this stage)"
            ));
        } else if !stage_agent_resolves(&s.agent, &cfg.agents, &cfg.tools) {
            out.push(format!(
                "{label}.agent: {:?} names no [[agents]]/[[tools]] entry and is not a known \
                 harness id — a stage agent is launched by name, not as a shell command",
                s.agent.trim()
            ));
        }
        if s.concurrency == 0 {
            out.push(format!(
                "{label}.concurrency: must be at least 1 (a stage that can never run is a \
                 typo — delete the stage to remove it)"
            ));
        }
        if let Some(nx) = s.next_name()
            && index_of(stages, nx).is_none()
        {
            out.push(format!("{label}.next: {nx:?} names no configured stage"));
        }
        // Permission patterns are seeded verbatim into the harness's own
        // settings file, so a pattern that names nothing (or carries a control
        // character that would corrupt the JSON line) is refused here rather
        // than written there.
        for (j, p) in s.permissions.iter().enumerate() {
            if p.trim().is_empty() {
                out.push(format!(
                    "{label}.permissions[{j}]: empty (a permission pattern must name something)"
                ));
            } else if p.chars().any(char::is_control) {
                out.push(format!(
                    "{label}.permissions[{j}]: contains a control character"
                ));
            } else if let Some(k) = s.permissions[..j].iter().position(|q| q == p) {
                out.push(format!(
                    "{label}.permissions[{j}]: duplicate of permissions[{k}]"
                ));
            }
        }
    }
    out.extend(cycle_errors(stages));
    out.extend(validate_transport_retry(&cfg.pipeline.transport_retry));
    out.extend(validate_supervisor(cfg));
    out
}

/// `[pipeline.supervisor]` + the per-stage `validate`/`requires` it acts on.
///
/// Every rule here exists to make an **unsatisfiable** configuration a loud
/// error at validate time rather than a silent stall at run time. A stage whose
/// requirement can never be met does not "hold work back safely" — it holds it
/// back invisibly, and the operator finds out when nothing has moved for a day.
/// That is the same reasoning `concurrency = 0` is refused under.
fn validate_supervisor(cfg: &Config) -> Vec<String> {
    let stages = &cfg.pipeline.stages;
    let sup = &cfg.pipeline.supervisor;
    let mut out = Vec::new();

    if sup.enabled && !sup.any_capability() {
        out.push(
            "pipeline.supervisor: enabled with validate_on_exit, advance and land all false \
             — that configuration does nothing; set enabled = false instead"
                .to_string(),
        );
    }

    // `land_requires` shares `requires`' closed vocabulary. An unrecognised
    // entry here is worse than elsewhere: it reads as a landing gate in the
    // file and is no gate at all in effect.
    let mut seen_land: Vec<Requirement> = Vec::new();
    for (j, r) in sup.land_requires.iter().enumerate() {
        match Requirement::parse(r) {
            None => out.push(format!(
                "pipeline.supervisor.land_requires[{j}]: {:?} is not a known requirement — \
                 known values: {}",
                r.trim(),
                Requirement::known_values()
            )),
            Some(req) if seen_land.contains(&req) => out.push(format!(
                "pipeline.supervisor.land_requires[{j}]: duplicate requirement {:?}",
                req.as_str()
            )),
            Some(req) => seen_land.push(req),
        }
    }
    // A terminal stage must be able to satisfy a `validation:green` landing
    // gate, or every lane parks at the end of the chart.
    if seen_land.contains(&Requirement::ValidationGreen) {
        for (i, s) in stages
            .iter()
            .enumerate()
            .filter(|(_, s)| s.next_name().is_none() && s.validate_tasks().is_empty())
        {
            out.push(format!(
                "pipeline.supervisor.land_requires: {:?} requires every terminal stage to \
                 declare `validate`, but {} declares none — nothing would ever record a green \
                 result, so its lanes could never land",
                Requirement::ValidationGreen.as_str(),
                label(i, s)
            ));
        }
    }

    for (i, s) in stages.iter().enumerate() {
        let label = label(i, s);

        // `validate` names [[tasks]] entries, never shell commands.
        for (j, t) in s.validate.iter().enumerate() {
            let name = t.trim();
            if name.is_empty() {
                out.push(format!(
                    "{label}.validate[{j}]: empty (a validation step names a [[tasks]] entry)"
                ));
            } else if !cfg.tasks.iter().any(|task| task.name.trim() == name) {
                out.push(format!(
                    "{label}.validate[{j}]: {name:?} names no [[tasks]] entry — a validation \
                     step is run by name, not as a shell command (add a [[tasks]] entry called \
                     {name:?})"
                ));
            } else if let Some(k) = s.validate[..j].iter().position(|q| q.trim() == name) {
                out.push(format!("{label}.validate[{j}]: duplicate of validate[{k}]"));
            }
        }

        // `requires` is a closed vocabulary.
        let mut parsed: Vec<Requirement> = Vec::new();
        for (j, r) in s.requires.iter().enumerate() {
            match Requirement::parse(r) {
                None => out.push(format!(
                    "{label}.requires[{j}]: {:?} is not a known requirement — known values: {}",
                    r.trim(),
                    Requirement::known_values()
                )),
                Some(req) if parsed.contains(&req) => out.push(format!(
                    "{label}.requires[{j}]: duplicate requirement {:?}",
                    req.as_str()
                )),
                Some(req) => parsed.push(req),
            }
        }
        if parsed.is_empty() {
            continue;
        }

        // Every requirement is a statement about the PARENT's output, so a
        // stage nothing advances into can never satisfy one. Reported per
        // stage, naming the remedy, because the usual cause is a requirement
        // written on the entry stage by analogy with the others.
        let parents: Vec<&PipelineStage> = stages
            .iter()
            .filter(|p| p.next_name() == s.stage_name())
            .collect();
        if parents.is_empty() {
            out.push(format!(
                "{label}.requires: no configured stage has `next` pointing here, so this stage \
                 has no parent and none of its requirements ({}) can ever be satisfied — \
                 remove them, or give the stage a parent",
                parsed
                    .iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            continue;
        }

        // `validation:green` is only meaningful when the parent actually
        // declares something to validate. Otherwise the requirement is
        // vacuously unmeetable and the lane parks forever.
        if parsed.contains(&Requirement::ValidationGreen) {
            for p in parents.iter().filter(|p| p.validate_tasks().is_empty()) {
                out.push(format!(
                    "{label}.requires: {:?} requires the parent stage {:?} to declare `validate`, \
                     but it declares none — nothing would ever record a green result, so this \
                     stage could never start",
                    Requirement::ValidationGreen.as_str(),
                    p.stage_name().unwrap_or("<unnamed>")
                ));
            }
        }
    }
    out
}

/// The handoff-contract pass: every configured stage must teach its worker the
/// two halves the done-gate checks. Separate from [`validate_pipeline`] so the
/// message can name the gate it is protecting, and so a caller that only wants
/// the structural checks (the display path) need not pay for it.
///
/// Errors, not warnings: a stage that fails this cannot produce a closable row,
/// and the failure is invisible until a backlog has already built up.
pub fn validate_stage_contracts(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    for (i, s) in cfg.pipeline.stages.iter().enumerate() {
        let label = label(i, s);
        for gap in stage_contract_gaps(s) {
            out.push(format!("{label}.prompt: {}", gap.remedy()));
        }
    }
    out
}

/// `[pipeline.transport_retry]` validation: a signature must name something
/// (it is matched against the worker's final screen — an empty one would match
/// every dead worker, including clean-looking crashes the supervisor should
/// see), and an enabled section must actually retry at least once.
fn validate_transport_retry(t: &TransportRetry) -> Vec<String> {
    let mut out = Vec::new();
    if t.enabled && t.max_attempts == 0 {
        out.push(
            "pipeline.transport_retry.max_attempts: must be at least 1 when enabled \
             (zero attempts can never retry — set enabled = false instead)"
                .to_string(),
        );
    }
    for (i, s) in t.transport_signatures.iter().enumerate() {
        if s.trim().is_empty() {
            out.push(format!(
                "pipeline.transport_retry.transport_signatures[{i}]: empty (a signature \
                 must name something)"
            ));
        }
    }
    for (i, s) in t.limit_signatures.iter().enumerate() {
        if s.trim().is_empty() {
            out.push(format!(
                "pipeline.transport_retry.limit_signatures[{i}]: empty (a signature \
                 must name something)"
            ));
        }
    }
    out
}

/// Every `next` cycle, reported once — from its lowest-indexed member, so one
/// loop yields one error however many stages it runs through.
fn cycle_errors(stages: &[PipelineStage]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, s) in stages.iter().enumerate() {
        let Some(start) = s.stage_name() else {
            continue;
        };
        let mut path: Vec<&str> = vec![start];
        let mut cur = s;
        while let Some(nx) = cur.next_name() {
            let Some(j) = index_of(stages, nx) else { break };
            if nx == start {
                // Report from the lowest-indexed member only.
                if path.iter().all(|n| index_of(stages, n).unwrap_or(i) >= i) {
                    out.push(format!(
                        "{}.next: forms a cycle ({} -> {start}) — a pipeline is a DAG",
                        label(i, s),
                        path.join(" -> ")
                    ));
                }
                break;
            }
            if path.contains(&nx) {
                // A loop that does not contain `start`; its own members report it.
                break;
            }
            path.push(nx);
            cur = &stages[j];
        }
    }
    out
}

/// Soft, best-effort warnings surfaced at config load (never block anything): a
/// stage no `next` edge reaches and which is not the entry stage. It is
/// reachable only if the Lead dispatches it by hand — usually a renamed `next`
/// that was not updated.
pub fn pipeline_warnings(cfg: &Config) -> Vec<String> {
    let stages = &cfg.pipeline.stages;
    let entry = stages.iter().position(|s| s.stage_name().is_some());
    let mut out = Vec::new();
    for (i, s) in stages.iter().enumerate() {
        let Some(n) = s.stage_name() else { continue };
        if Some(i) == entry {
            continue;
        }
        let reached = stages
            .iter()
            .enumerate()
            .any(|(j, other)| j != i && other.next_name() == Some(n));
        if !reached {
            out.push(format!(
                "pipeline stage {n:?} is not the first stage and no stage's `next` reaches it \
                 — it will only run if the supervisor dispatches it explicitly"
            ));
        }
    }
    // Surface the handoff-contract gaps at load too, not only under
    // `config validate`: the operator who most needs this is the one whose
    // pipeline is already running against prompts that cannot close a row.
    for (i, s) in stages.iter().enumerate() {
        let label = label(i, s);
        for gap in stage_contract_gaps(s) {
            out.push(format!("{label}.prompt: {}", gap.remedy()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> NamedCommand {
        NamedCommand {
            name: name.to_string(),
            command: format!("{name} --run"),
            hints: Vec::new(),
            provider: None,
            harness: None,
            resume: false,
            route_via_proxy: false,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
            drawer_scope: None,
            drawer_cwd: None,
        }
    }

    /// A config with one `[[agents]]` entry named `worker`, plus `stages`.
    fn cfg_with(stages: Vec<PipelineStage>) -> Config {
        let mut cfg = Config::default();
        cfg.agents.push(named("worker"));
        cfg.tools.push(named("reviewer-tool"));
        cfg.pipeline.stages = stages;
        cfg
    }

    /// A stage whose prompt satisfies the handoff contract, so tests about the
    /// org chart (names, edges, cycles) are not also asserting on
    /// [`stage_contract_gaps`]. The contract itself is tested directly below.
    fn stage(name: &str, next: Option<&str>) -> PipelineStage {
        PipelineStage {
            name: name.into(),
            agent: "worker".into(),
            next: next.map(str::to_string),
            prompt: COMPLIANT_PROMPT.into(),
            ..Default::default()
        }
    }

    /// The minimum a stage prompt must say: which row the worker is, and that
    /// it must file the report the done-gate requires.
    const COMPLIANT_PROMPT: &str = "You are dispatch row {row}. Do the work, commit {artifact}, then run \
         `thegn dispatch report {row} --text '…'`.";

    #[test]
    fn defaults_are_one_worker_and_an_hour_parked() {
        let s = PipelineStage::default();
        assert_eq!(s.concurrency, 1);
        assert_eq!(s.timeout_secs, 3600);
        assert_eq!(s.on_blocked, OnBlocked::Park);
        assert_eq!(s.next_name(), None);
        assert_eq!(s.stage_name(), None);
        assert!(Pipeline::default().stages.is_empty());
        assert!(Pipeline::default().entry().is_none());
    }

    // --- [pipeline.transport_retry] (THE-86) ----------------------------------

    #[test]
    fn transport_retry_defaults_are_enabled_bounded_and_load_the_core_signature_lists() {
        let t = TransportRetry::default();
        assert!(t.enabled);
        assert_eq!(t.max_attempts, 3);
        assert_eq!(t.backoff_ms, 2_000);
        assert_eq!(
            t.transport_signatures,
            crate::pipeline_exit::DEFAULT_TRANSPORT_SIGNATURES
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            t.limit_signatures,
            crate::pipeline_exit::DEFAULT_LIMIT_SIGNATURES
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        // An absent section parses to the defaults, and validates clean.
        let cfg: Config = toml::from_str("[pipeline]\n").unwrap();
        assert_eq!(cfg.pipeline.transport_retry, TransportRetry::default());
        assert!(validate_pipeline(&cfg).is_empty());
    }

    #[test]
    fn transport_retry_overrides_parse_and_replace_the_default_lists() {
        let cfg: Config = toml::from_str(
            r#"
            [pipeline.transport_retry]
            enabled = true
            max_attempts = 5
            backoff_ms = 500
            transport_signatures = ["overloaded_error", "socket hang up"]
            limit_signatures = ["weekly limit"]
            "#,
        )
        .unwrap();
        let t = &cfg.pipeline.transport_retry;
        assert_eq!(t.max_attempts, 5);
        assert_eq!(
            t.transport_signatures,
            vec!["overloaded_error".to_string(), "socket hang up".to_string()],
            "an override REPLACES the default list, it does not extend it"
        );
        assert!(validate_pipeline(&cfg).is_empty());
    }

    #[test]
    fn transport_retry_rejects_an_empty_signature_and_a_zero_attempt_budget() {
        let mut cfg = Config::default();
        cfg.pipeline.transport_retry.max_attempts = 0;
        let errs = validate_pipeline(&cfg);
        assert!(
            errs.iter().any(|e| e.contains("max_attempts")),
            "0 attempts with enabled = true is a config error: {errs:?}"
        );
        cfg.pipeline.transport_retry.max_attempts = 3;
        cfg.pipeline
            .transport_retry
            .transport_signatures
            .push("   ".into());
        let errs = validate_pipeline(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains("transport_signatures") && e.contains(": empty")),
            "a whitespace signature matches everything: {errs:?}"
        );
        cfg.pipeline.transport_retry.transport_signatures.pop();
        cfg.pipeline
            .transport_retry
            .limit_signatures
            .push("\t".into());
        let errs = validate_pipeline(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains("limit_signatures") && e.contains(": empty")),
            "a whitespace limit signature matches everything: {errs:?}"
        );
        // `max_attempts = 0` is legal exactly when the section is disabled.
        cfg.pipeline.transport_retry.enabled = false;
        cfg.pipeline.transport_retry.limit_signatures.pop();
        assert!(validate_pipeline(&cfg).is_empty());
    }

    #[test]
    fn on_blocked_parses_canon_and_aliases_defaults_park() {
        assert_eq!(OnBlocked::from_str_validated("park"), Ok(OnBlocked::Park));
        assert_eq!(
            OnBlocked::from_str_validated("waiting_human"),
            Ok(OnBlocked::Park)
        );
        assert_eq!(OnBlocked::from_str_validated("WAIT"), Ok(OnBlocked::Park));
        assert_eq!(
            OnBlocked::from_str_validated("escalate"),
            Ok(OnBlocked::Escalate)
        );
        assert_eq!(
            OnBlocked::from_str_validated("notify"),
            Ok(OnBlocked::Escalate)
        );
        assert_eq!(
            OnBlocked::from_str_validated("abandon"),
            Ok(OnBlocked::Abandon)
        );
        assert_eq!(
            OnBlocked::from_str_validated("drop"),
            Ok(OnBlocked::Abandon)
        );
        assert!(OnBlocked::from_str_validated("retry").is_err());
        assert_eq!(OnBlocked::default(), OnBlocked::Park);
        assert_eq!(OnBlocked::Park.as_str(), "park");
        assert_eq!(OnBlocked::Escalate.to_string(), "escalate");
    }

    #[test]
    fn toml_round_trips_with_defaults_for_every_omitted_key() {
        let body = r#"
[[pipeline.stages]]
name = "architect"
agent = "worker"
prompt = "Design {issue_title} into chunks under {artifact}."
next = "code"

[[pipeline.stages]]
name = "code"
agent = "worker"
concurrency = 3
timeout_secs = 900
on_blocked = "escalate"
permissions = ["Read", "Edit", "Bash(git:*)"]
"#;
        let cfg: Config = toml::from_str(body).expect("parses");
        let p = &cfg.pipeline;
        assert_eq!(p.stage_names(), vec!["architect", "code"]);
        assert_eq!(p.entry().unwrap().name, "architect");

        let arch = p.stage("architect").unwrap();
        assert_eq!(arch.concurrency, 1, "omitted concurrency defaults to 1");
        assert_eq!(
            arch.timeout_secs, 3600,
            "omitted timeout defaults to an hour"
        );
        assert_eq!(arch.on_blocked, OnBlocked::Park);
        assert_eq!(arch.next_name(), Some("code"));

        let code = p.stage("code").unwrap();
        assert_eq!(code.concurrency, 3);
        assert_eq!(code.timeout_secs, 900);
        assert_eq!(code.on_blocked, OnBlocked::Escalate);
        assert_eq!(code.next_name(), None, "the last stage is terminal");
        assert_eq!(
            code.permissions,
            vec![
                "Read".to_string(),
                "Edit".to_string(),
                "Bash(git:*)".to_string()
            ],
            "a permissions list parses"
        );
        assert!(
            arch.permissions.is_empty(),
            "omitted permissions default to none"
        );
        assert!(p.stage("nope").is_none());

        // Serialize → parse: the shape survives a round trip.
        let out = toml::to_string(&cfg.pipeline).expect("serializes");
        let back: Pipeline = toml::from_str(&out).expect("re-parses");
        assert_eq!(back, cfg.pipeline);
    }

    #[test]
    fn an_absent_section_is_an_empty_inert_pipeline() {
        let cfg: Config = toml::from_str("base_branch = \"main\"\n").unwrap();
        assert!(cfg.pipeline.stages.is_empty());
        assert!(validate_pipeline(&cfg).is_empty());
        assert!(pipeline_warnings(&cfg).is_empty());
    }

    #[test]
    fn stage_agent_resolves_registry_names_and_bare_harness_ids() {
        let agents = [named("worker")];
        let tools = [named("reviewer-tool")];
        assert!(stage_agent_resolves("worker", &agents, &tools));
        assert!(stage_agent_resolves("  worker  ", &agents, &tools));
        assert!(stage_agent_resolves("reviewer-tool", &agents, &tools));
        // Bare harness ids (the `bare_provider` carve-out) resolve with no entry.
        for id in ["claude", "codex", "aider"] {
            assert!(
                stage_agent_resolves(id, &[], &[]),
                "bare harness id {id} should resolve"
            );
        }
        // A raw shell command is not a stage agent.
        assert!(!stage_agent_resolves("just dev", &agents, &tools));
        assert!(!stage_agent_resolves("", &agents, &tools));
        assert!(!stage_agent_resolves("   ", &agents, &tools));
        assert!(!stage_agent_resolves("nosuchagent", &agents, &tools));
        // `shell` classifies as the login shell, not a named entry.
        assert!(!stage_agent_resolves("shell", &agents, &tools));
    }

    #[test]
    fn contract_gaps_flag_a_prompt_that_cannot_close_its_row() {
        // The exact shape of the 2026-08-29 incident: a perfectly valid org
        // chart whose prompts predate the report requirement. Every other
        // check passes, and every row it dispatches is unclosable.
        let mut s = stage("code", None);
        s.prompt = "Implement the chunk at {parent_artifact}; write {artifact}.".into();
        assert_eq!(
            stage_contract_gaps(&s),
            vec![
                ContractGap::NoRowPlaceholder,
                ContractGap::NoReportInstruction
            ]
        );
        let cfg = cfg_with(vec![s]);
        // The org-chart pass is silent — which is precisely why the contract
        // pass has to exist as its own channel.
        assert!(validate_pipeline(&cfg).is_empty());
        let errs = validate_stage_contracts(&cfg);
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("{row}")), "{errs:?}");
        assert!(
            errs.iter().any(|e| e.contains("dispatch report")),
            "{errs:?}"
        );
        // And it reaches the operator at load, not only under `config validate`.
        assert!(
            pipeline_warnings(&cfg)
                .iter()
                .any(|w| w.contains("dispatch report"))
        );
    }

    #[test]
    fn contract_gaps_are_placeholder_aware_not_substring_matching() {
        // `{ row }` renders, so it counts...
        let mut spaced = stage("code", None);
        spaced.prompt = "row { row }: run `thegn dispatch report`".into();
        assert!(stage_contract_gaps(&spaced).is_empty());
        // ...but an escaped `{{row}}` is a literal brace pair the renderer
        // never substitutes, so it must NOT count as naming the row.
        let mut escaped = stage("code", None);
        escaped.prompt = "literally {{row}} — run `thegn dispatch report`".into();
        assert_eq!(
            stage_contract_gaps(&escaped),
            vec![ContractGap::NoRowPlaceholder]
        );
    }

    #[test]
    fn contract_gap_reports_only_the_half_that_is_missing() {
        let mut no_report = stage("code", None);
        no_report.prompt = "You are row {row}. Commit {artifact}.".into();
        assert_eq!(
            stage_contract_gaps(&no_report),
            vec![ContractGap::NoReportInstruction]
        );
        let mut no_row = stage("code", None);
        no_row.prompt = "Commit it, then run `thegn dispatch report <id>`.".into();
        assert_eq!(
            stage_contract_gaps(&no_row),
            vec![ContractGap::NoRowPlaceholder]
        );
    }

    #[test]
    fn a_compliant_prompt_has_no_contract_gaps() {
        let cfg = cfg_with(vec![stage("architect", None)]);
        assert!(validate_stage_contracts(&cfg).is_empty());
        assert!(stage_contract_gaps(&cfg.pipeline.stages[0]).is_empty());
    }

    #[test]
    fn validate_accepts_a_well_formed_chain() {
        let cfg = cfg_with(vec![
            stage("architect", Some("code")),
            stage("code", Some("review")),
            stage("review", None),
        ]);
        assert!(validate_pipeline(&cfg).is_empty());
        assert!(pipeline_warnings(&cfg).is_empty());
    }

    #[test]
    fn validate_rejects_a_missing_name() {
        let cfg = cfg_with(vec![stage("", None)]);
        let errs = validate_pipeline(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains("pipeline.stages[0].name: required")),
            "{errs:?}"
        );
    }

    #[test]
    fn validate_rejects_a_duplicate_name() {
        let cfg = cfg_with(vec![stage("code", None), stage("code", None)]);
        let errs = validate_pipeline(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains("pipeline.stages[1] (\"code\")") && e.contains("duplicate")),
            "{errs:?}"
        );
    }

    #[test]
    fn validate_rejects_an_empty_agent() {
        let mut s = stage("code", None);
        s.agent = "  ".into();
        let cfg = cfg_with(vec![s]);
        let errs = validate_pipeline(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains("pipeline.stages[0] (\"code\").agent: required")),
            "{errs:?}"
        );
    }

    #[test]
    fn validate_rejects_an_unresolvable_agent() {
        let mut s = stage("code", None);
        s.agent = "just dev".into();
        let cfg = cfg_with(vec![s]);
        let errs = validate_pipeline(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains(".agent:") && e.contains("names no [[agents]]/[[tools]] entry")),
            "{errs:?}"
        );
    }

    #[test]
    fn validate_rejects_zero_concurrency() {
        let mut s = stage("code", None);
        s.concurrency = 0;
        let cfg = cfg_with(vec![s]);
        let errs = validate_pipeline(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains(".concurrency: must be at least 1")),
            "{errs:?}"
        );
    }

    #[test]
    fn validate_rejects_an_unknown_next() {
        let cfg = cfg_with(vec![stage("code", Some("nope"))]);
        let errs = validate_pipeline(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains(".next: \"nope\" names no configured stage")),
            "{errs:?}"
        );
        // A blank `next` is simply terminal, not an unknown target.
        let mut s = stage("code", Some("   "));
        s.next = Some("   ".into());
        let cfg = cfg_with(vec![s]);
        assert!(validate_pipeline(&cfg).is_empty(), "blank next is terminal");
    }

    #[test]
    fn validate_reports_each_cycle_exactly_once() {
        // a -> b -> c -> a: one error, from the lowest-indexed member.
        let cfg = cfg_with(vec![
            stage("a", Some("b")),
            stage("b", Some("c")),
            stage("c", Some("a")),
        ]);
        let errs = validate_pipeline(&cfg);
        let cycles: Vec<&String> = errs.iter().filter(|e| e.contains("cycle")).collect();
        assert_eq!(cycles.len(), 1, "{errs:?}");
        assert!(
            cycles[0].contains("pipeline.stages[0] (\"a\")") && cycles[0].contains("a -> b -> c"),
            "{cycles:?}"
        );
    }

    #[test]
    fn validate_reports_a_self_loop_and_a_downstream_loop() {
        let cfg = cfg_with(vec![stage("solo", Some("solo"))]);
        let errs = validate_pipeline(&cfg);
        assert_eq!(
            errs.iter().filter(|e| e.contains("cycle")).count(),
            1,
            "{errs:?}"
        );

        // An entry stage that feeds a loop it is not part of: the loop still
        // reports (from its own lowest member), the entry does not.
        let cfg = cfg_with(vec![
            stage("entry", Some("b")),
            stage("b", Some("c")),
            stage("c", Some("b")),
        ]);
        let errs = validate_pipeline(&cfg);
        let cycles: Vec<&String> = errs.iter().filter(|e| e.contains("cycle")).collect();
        assert_eq!(cycles.len(), 1, "{errs:?}");
        assert!(
            cycles[0].contains("pipeline.stages[1] (\"b\")"),
            "{cycles:?}"
        );
    }

    #[test]
    fn warnings_flag_an_unreachable_stage_only() {
        let cfg = cfg_with(vec![
            stage("architect", Some("code")),
            stage("code", None),
            stage("orphan", None),
        ]);
        assert!(validate_pipeline(&cfg).is_empty(), "orphan is not an error");
        let warns = pipeline_warnings(&cfg);
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(warns[0].contains("orphan"), "{warns:?}");

        // The entry stage is the first NAMED one, so an unnamed stage above it
        // (already a hard error) must not also mint an unreachable warning.
        let cfg = cfg_with(vec![stage("", None), stage("only", None)]);
        assert!(pipeline_warnings(&cfg).is_empty());
    }

    #[test]
    fn a_stage_pointing_at_itself_is_not_counted_as_reaching_itself() {
        let cfg = cfg_with(vec![stage("entry", None), stage("loop", Some("loop"))]);
        let warns = pipeline_warnings(&cfg);
        assert!(
            warns.iter().any(|w| w.contains("loop")),
            "a self-edge must not launder a stage into reachability: {warns:?}"
        );
    }

    // --- stage.permissions ---------------------------------------------------

    fn stage_with_permissions(permissions: &[&str]) -> PipelineStage {
        PipelineStage {
            permissions: permissions.iter().map(|s| s.to_string()).collect(),
            ..stage("code", None)
        }
    }

    #[test]
    fn validate_rejects_an_empty_or_control_char_permission() {
        let errs = validate_pipeline(&cfg_with(vec![stage_with_permissions(&["Read", ""])]));
        assert!(
            errs.iter()
                .any(|e| e
                    .contains("permissions[1]: empty (a permission pattern must name something)")),
            "{errs:?}"
        );
        // Whitespace-only is empty too, and reports at the right index.
        let errs = validate_pipeline(&cfg_with(vec![stage_with_permissions(&["   "])]));
        assert!(
            errs.iter().any(|e| e.contains("permissions[0]: empty")),
            "{errs:?}"
        );
        // A control character would corrupt the seeded settings file.
        let errs = validate_pipeline(&cfg_with(vec![stage_with_permissions(&["Read\n"])]));
        assert!(
            errs.iter()
                .any(|e| e.contains("permissions[0]: contains a control character")),
            "{errs:?}"
        );
    }

    #[test]
    fn validate_rejects_a_duplicate_permission() {
        let errs = validate_pipeline(&cfg_with(vec![stage_with_permissions(&[
            "Read", "Edit", "Read",
        ])]));
        assert!(
            errs.iter()
                .any(|e| e.contains("permissions[2]: duplicate of permissions[0]")),
            "{errs:?}"
        );
        // Distinct entries are fine.
        assert!(
            validate_pipeline(&cfg_with(vec![stage_with_permissions(&["Read", "Edit"])]))
                .is_empty()
        );
    }

    #[test]
    fn a_stage_with_no_permissions_is_valid_and_seeds_nothing() {
        let cfg = cfg_with(vec![stage("code", None)]);
        assert!(validate_pipeline(&cfg).is_empty());
        assert!(cfg.pipeline.stages[0].permissions.is_empty());
        assert_eq!(PipelineStage::default().permissions, Vec::<String>::new());
    }

    // --- the supervisor: validate / requires / [pipeline.supervisor] ---------

    /// A config carrying a `nextest` task, so `validate` entries resolve.
    fn cfg_with_task(stages: Vec<PipelineStage>) -> Config {
        let mut cfg = cfg_with(stages);
        cfg.tasks.push(crate::config::Task {
            name: "nextest".into(),
            command: "cargo".into(),
            args: vec!["nextest".into(), "run".into()],
            cwd: None,
            env: Default::default(),
            kind: crate::config::TaskKind::Test,
            matcher: Some("nextest".into()),
            scope: None,
        });
        cfg
    }

    #[test]
    fn the_supervisor_defaults_to_off_and_to_requiring_an_approval_to_land() {
        // The two defaults that matter: nothing runs unless asked, and asking
        // does not by itself authorize landing unreviewed code.
        let sup = Supervisor::default();
        assert!(!sup.enabled);
        assert_eq!(sup.parsed_land_requires(), vec![Requirement::Approval]);
        // And a default config validates.
        assert!(validate_pipeline(&Config::default()).is_empty());
    }

    #[test]
    fn a_validate_entry_must_name_a_task_not_a_shell_command() {
        // The closed-registry rule `agent` follows: a stage must not be able to
        // introduce arbitrary command execution from config.
        let mut s = stage("code", None);
        s.validate = vec!["cargo nextest run".into()];
        let errs = validate_pipeline(&cfg_with_task(vec![s]));
        assert!(
            errs.iter().any(|e| e.contains("names no [[tasks]] entry")),
            "{errs:?}"
        );
    }

    #[test]
    fn a_validate_entry_naming_a_real_task_is_accepted() {
        let mut s = stage("code", None);
        s.validate = vec!["nextest".into()];
        assert!(validate_pipeline(&cfg_with_task(vec![s])).is_empty());
    }

    #[test]
    fn an_empty_or_duplicate_validate_entry_is_reported() {
        let mut s = stage("code", None);
        s.validate = vec!["nextest".into(), "".into(), "nextest".into()];
        let errs = validate_pipeline(&cfg_with_task(vec![s]));
        assert!(
            errs.iter().any(|e| e.contains("validate[1]: empty")),
            "{errs:?}"
        );
        assert!(
            errs.iter().any(|e| e.contains("validate[2]: duplicate")),
            "{errs:?}"
        );
    }

    #[test]
    fn an_unknown_requirement_names_the_known_values() {
        // A typo must never become a gate that is present in the file and
        // absent in effect.
        let mut code = stage("code", Some("review"));
        code.validate = vec!["nextest".into()];
        let mut review = stage("review", None);
        review.requires = vec!["aproval".into()];
        let errs = validate_pipeline(&cfg_with_task(vec![code, review]));
        let msg = errs
            .iter()
            .find(|e| e.contains("requires[0]"))
            .unwrap_or_else(|| panic!("{errs:?}"));
        assert!(msg.contains("not a known requirement"), "{msg}");
        for known in Requirement::ALL {
            assert!(
                msg.contains(known.as_str()),
                "{msg} omits {}",
                known.as_str()
            );
        }
    }

    #[test]
    fn a_duplicate_requirement_is_reported() {
        let code = stage("code", Some("review"));
        let mut review = stage("review", None);
        review.requires = vec!["approval".into(), "approval".into()];
        let errs = validate_pipeline(&cfg_with_task(vec![code, review]));
        assert!(
            errs.iter().any(|e| e.contains("requires[1]: duplicate")),
            "{errs:?}"
        );
    }

    #[test]
    fn a_requirement_on_a_stage_with_no_parent_is_refused() {
        // Every requirement is a statement about the PARENT's output, so a
        // stage nothing advances into can never satisfy one. Refusing it here
        // is the difference between a loud error and a lane that silently never
        // moves — the same reasoning `concurrency = 0` is refused under.
        let mut entry = stage("investigate", None);
        entry.requires = vec!["approval".into()];
        let errs = validate_pipeline(&cfg_with_task(vec![entry]));
        assert!(
            errs.iter().any(|e| e.contains("has no parent")),
            "an unsatisfiable requirement was accepted: {errs:?}"
        );
    }

    #[test]
    fn validation_green_requires_the_parent_to_declare_validate() {
        // Otherwise nothing ever records a green result and the stage can never
        // start — an unmeetable gate, not a strict one.
        let code = stage("code", Some("review")); // declares no `validate`
        let mut review = stage("review", None);
        review.requires = vec!["validation:green".into()];
        let errs = validate_pipeline(&cfg_with_task(vec![code, review]));
        assert!(
            errs.iter()
                .any(|e| e.contains("requires the parent stage") && e.contains("declares none")),
            "{errs:?}"
        );

        // With the parent declaring a task, it is fine.
        let mut code = stage("code", Some("review"));
        code.validate = vec!["nextest".into()];
        let mut review = stage("review", None);
        review.requires = vec!["validation:green".into()];
        assert!(validate_pipeline(&cfg_with_task(vec![code, review])).is_empty());
    }

    #[test]
    fn a_land_gate_requiring_green_needs_every_terminal_stage_to_validate() {
        let mut cfg = cfg_with_task(vec![stage("code", None)]);
        cfg.pipeline.supervisor.land_requires = vec!["validation:green".into()];
        let errs = validate_pipeline(&cfg);
        assert!(errs.iter().any(|e| e.contains("land_requires")), "{errs:?}");

        cfg.pipeline.stages[0].validate = vec!["nextest".into()];
        assert!(validate_pipeline(&cfg).is_empty());
    }

    #[test]
    fn an_unknown_land_requirement_is_refused() {
        let mut cfg = cfg_with_task(vec![stage("code", None)]);
        cfg.pipeline.supervisor.land_requires = vec!["rubber_stamp".into()];
        let errs = validate_pipeline(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains("land_requires[0]") && e.contains("not a known requirement")),
            "{errs:?}"
        );
    }

    #[test]
    fn an_enabled_supervisor_with_every_capability_off_is_refused() {
        let mut cfg = cfg_with_task(vec![stage("code", None)]);
        cfg.pipeline.supervisor.enabled = true;
        cfg.pipeline.supervisor.validate_on_exit = false;
        cfg.pipeline.supervisor.advance = false;
        cfg.pipeline.supervisor.land = false;
        let errs = validate_pipeline(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains("that configuration does nothing")),
            "{errs:?}"
        );
    }

    #[test]
    fn every_requirement_round_trips_and_has_a_distinct_spelling() {
        let mut spellings: Vec<&str> = Requirement::ALL.iter().map(|r| r.as_str()).collect();
        let n = spellings.len();
        spellings.sort_unstable();
        spellings.dedup();
        assert_eq!(n, spellings.len(), "two requirements share a spelling");
        for r in Requirement::ALL {
            assert_eq!(Requirement::parse(r.as_str()), Some(r));
            // Tolerant of surrounding whitespace, intolerant of typos.
            assert_eq!(Requirement::parse(&format!("  {}  ", r.as_str())), Some(r));
        }
        assert_eq!(Requirement::parse("approvals"), None);
        assert_eq!(Requirement::parse(""), None);
    }

    #[test]
    fn max_validations_resolves_zero_against_the_machine_limit_and_never_returns_zero() {
        let mut sup = Supervisor::default();
        assert_eq!(sup.effective_max_validations(4), 1, "explicit value wins");
        sup.max_validations = 0;
        assert_eq!(sup.effective_max_validations(4), 4, "zero follows [limits]");
        // A ceiling of zero would mean nothing ever validates — a stall, not a
        // setting — so it is clamped from both directions.
        assert_eq!(sup.effective_max_validations(0), 1);
    }

    #[test]
    fn a_stage_helper_ignores_blank_validate_entries() {
        let mut s = stage("code", None);
        s.validate = vec!["  nextest  ".into(), "   ".into()];
        assert_eq!(s.validate_tasks(), vec!["nextest"]);
    }

    #[test]
    fn parsed_requires_drops_what_validation_already_reported() {
        let mut s = stage("code", None);
        s.requires = vec!["approval".into(), "nonsense".into()];
        assert_eq!(s.parsed_requires(), vec![Requirement::Approval]);
    }
}
