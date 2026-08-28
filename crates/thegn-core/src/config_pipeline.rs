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
    /// How many workers of this stage the Lead may run at once. Advisory: an
    /// **agent-side** budget, counted by the Lead from the roster's active rows,
    /// never enforced by thegn. `0` is a config error (a stage that can never
    /// run is a typo, not a way to disable one).
    pub concurrency: u32,
    /// How long the Lead should wait on this stage's session before treating it
    /// as blocked, in seconds. **Advisory — thegn never fires this timer**; the
    /// Lead passes it to `thegn session wait --timeout` (milliseconds), which is
    /// the only watchdog that exists.
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
    /// `permissions` when non-empty. At launch thegn writes the effective
    /// list into the harness's per-worktree settings file and does NOT
    /// interpret the strings — they are the harness's own vocabulary
    /// (`Bash(git status:*)`, `Read`, `mcp__srv__tool`). Empty = the entry's
    /// list, if any.
    pub permissions: Vec<String>,
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
        }
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

    fn stage(name: &str, next: Option<&str>) -> PipelineStage {
        PipelineStage {
            name: name.into(),
            agent: "worker".into(),
            next: next.map(str::to_string),
            ..Default::default()
        }
    }

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
}
