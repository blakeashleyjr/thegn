//! `thegn doctor`: Lead readiness (THE-2213). Reads config and the DB
//! locally; never writes. Every check is a pure fn over an input struct (so
//! tests build inputs without touching the host); [`inspect`] gathers them.

use std::path::{Path, PathBuf};

use thegn_core::config::Config;
use thegn_core::outln;

/// Most registered repos the worker check probes.
const REPO_CAP: usize = 8;
/// The one line printed when nothing Lead-related is configured.
pub(crate) const NOT_CONFIGURED: &str = "Lead: not configured (see `thegn help lead`)";

/// Every Lead readiness check.
pub(crate) struct LeadReadiness {
    pub checks: Vec<Check>,
    /// True when nothing is configured: print [`NOT_CONFIGURED`] only.
    pub unconfigured: bool,
}

/// One readiness check.
pub(crate) struct Check {
    pub id: &'static str,
    pub level: Level,
    pub text: String,
    pub fix: Option<String>,
}

/// A check's severity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Level {
    Ok,
    Warn,
    Fail,
    Info,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
            Level::Info => "info",
        }
    }
}

fn check(id: &'static str, level: Level, text: impl Into<String>, fix: Option<&str>) -> Check {
    Check {
        id,
        level,
        text: text.into(),
        fix: fix.map(str::to_owned),
    }
}

fn pass_or(
    id: &'static str,
    good: bool,
    miss: Level,
    ok: &str,
    bad: &str,
    fix: Option<&str>,
) -> Check {
    if good {
        check(id, Level::Ok, ok, None)
    } else {
        check(id, miss, bad, fix)
    }
}

// ---- pure checks -------------------------------------------------------

pub(crate) fn check_channel(clamped: bool) -> Check {
    pass_or(
        "channel",
        !clamped,
        Level::Fail,
        "release channel allows [lead]",
        "[lead] is configured but this release channel clamps it off",
        Some(
            "press Alt i to set the Lead up (it runs as a preview on stable), or use THEGN_CHANNEL=dev",
        ),
    )
}

/// `harness`: `Some(id)` when `[lead] agent` names an `[[agents]]` entry.
pub(crate) fn check_agent(name: &str, harness: Option<&str>) -> Check {
    match harness {
        Some("claude") => check("agent", Level::Ok, format!("agent {name:?} (claude)"), None),
        Some(other) => check(
            "agent",
            Level::Fail,
            format!("agent {name:?} uses harness {other:?}; the Lead needs claude"),
            None,
        ),
        None => check(
            "agent",
            Level::Fail,
            format!("[lead] agent {name:?} names no [[agents]] entry"),
            None,
        ),
    }
}

pub(crate) fn check_account(account: Option<&str>) -> Check {
    pass_or(
        "account",
        account.is_none(),
        Level::Fail,
        "no [lead] account (private login)",
        "[lead] account is set",
        Some("remove it; use `thegn lead login`"),
    )
}

pub(crate) fn check_bwrap(available: bool) -> Check {
    pass_or(
        "bwrap",
        available,
        Level::Fail,
        "bubblewrap available",
        "bubblewrap (bwrap) is not available",
        Some("install bubblewrap"),
    )
}

pub(crate) fn check_pasta(isolated: bool, available: bool) -> Check {
    if !isolated {
        return check(
            "pasta",
            Level::Fail,
            "network = \"host\" is refused",
            Some("set [lead] network = \"isolated\""),
        );
    }
    pass_or(
        "pasta",
        available,
        Level::Fail,
        "pasta available (isolated network)",
        "pasta is not available for the isolated network",
        Some("install passt (required for the isolated Lead network)"),
    )
}

pub(crate) struct DaemonFacts {
    pub enabled: bool,
    pub answers: bool,
    pub channel: Option<String>,
    /// `[lead] preview = true` (written by the in-app setup): a stable daemon
    /// keeps `[lead]` too.
    pub preview: bool,
}

pub(crate) fn check_daemon(f: &DaemonFacts) -> Check {
    if !f.enabled {
        return check(
            "daemon",
            Level::Fail,
            "[daemon] enabled is false",
            Some("set [daemon] enabled = true"),
        );
    }
    if f.answers
        && !f.preview
        && let Some(c) = f.channel.as_deref().filter(|c| *c != "dev")
    {
        return check(
            "daemon",
            Level::Fail,
            format!("the daemon runs on the {c} channel, where [lead] is off"),
            Some(
                "set the Lead up again from Alt i (it marks [lead] as a preview), or use THEGN_CHANNEL=dev",
            ),
        );
    }
    pass_or(
        "daemon",
        f.answers,
        Level::Fail,
        "daemon answers on its socket",
        "the daemon socket does not answer",
        Some("open thegn (it starts the daemon)"),
    )
}

/// `addr`: the result of reading and parsing `control-addr.json`.
pub(crate) fn check_listener(addr: Result<std::net::SocketAddr, String>) -> Check {
    match addr {
        Ok(a) => check("listener", Level::Ok, format!("Lead listener at {a}"), None),
        Err(e) => check(
            "listener",
            Level::Fail,
            format!("Lead listener address unusable: {e}"),
            Some("restart the daemon from the Lead setup (Alt i), then reopen thegn"),
        ),
    }
}

pub(crate) fn check_login(has_credentials: bool) -> Check {
    pass_or(
        "login",
        has_credentials,
        Level::Warn,
        "the Lead's private harness home has credentials",
        "no credentials in the Lead's private harness home",
        Some("`thegn lead login`"),
    )
}

/// `offenders`: the paths (agent binary, current exe) found under a mask.
pub(crate) fn check_masks(offenders: &[String]) -> Check {
    pass_or(
        "masks",
        offenders.is_empty(),
        Level::Fail,
        "agent binary and thegn are outside the sandbox masks",
        &format!("under a masked root: {}", offenders.join(", ")),
        Some("install elsewhere"),
    )
}

pub(crate) struct McpFacts {
    pub profile: String,
    /// `None` = no such profile; else each member and its state.
    pub members: Option<Vec<McpMember>>,
}

pub(crate) struct McpMember {
    pub name: String,
    pub exposed: bool,
    pub needs_dir: bool,
}

pub(crate) fn check_mcp(f: &McpFacts) -> Vec<Check> {
    let Some(members) = &f.members else {
        return vec![check(
            "mcp",
            Level::Warn,
            format!("[mcp_proxy] profiles has no {:?} profile", f.profile),
            None,
        )];
    };
    let mut out = Vec::new();
    let unexposed: Vec<&str> = members
        .iter()
        .filter(|m| !m.exposed)
        .map(|m| m.name.as_str())
        .collect();
    if unexposed.is_empty() {
        out.push(check(
            "mcp",
            Level::Ok,
            format!(
                "mcp profile {:?}: {} server(s) proxy-exposed",
                f.profile,
                members.len()
            ),
            None,
        ));
    } else {
        out.push(check(
            "mcp",
            Level::Warn,
            format!("mcp servers not proxy-exposed: {}", unexposed.join(", ")),
            None,
        ));
    }
    for m in members.iter().filter(|m| m.needs_dir) {
        out.push(check(
            "mcp",
            Level::Info,
            format!(
                "mcp server {:?} runs through npx/uvx/mcp-remote: if it needs OAuth, run `thegn lead login --mcp {}`",
                m.name, m.name
            ),
            None,
        ));
    }
    out
}

pub(crate) struct WorkerFacts {
    pub repo: String,
    pub isolated: bool,
    pub backend: String,
    pub network: String,
}

pub(crate) fn check_workers(w: &WorkerFacts) -> Check {
    if w.isolated {
        check(
            "workers",
            Level::Ok,
            format!("{}: sealed ({}, network {})", w.repo, w.backend, w.network),
            None,
        )
    } else {
        check(
            "workers",
            Level::Warn,
            format!(
                "{}: no sealed sandbox for unattended workers ({}, network {})",
                w.repo, w.backend, w.network
            ),
            Some("name a sealed profile; sealed has no network"),
        )
    }
}

pub(crate) struct SupervisorFacts {
    pub enabled: bool,
    pub stages: usize,
    pub missing_tasks: Vec<String>,
    pub test_timeout_secs: u64,
}

pub(crate) fn check_supervisor(f: &SupervisorFacts) -> Check {
    if !f.enabled {
        return check(
            "supervisor",
            Level::Info,
            "[pipeline.supervisor] is off",
            None,
        );
    }
    let mut problems = Vec::new();
    if f.stages == 0 {
        problems.push("no [[pipeline.stages]]".to_owned());
    }
    if !f.missing_tasks.is_empty() {
        problems.push(format!(
            "validate tasks missing: {}",
            f.missing_tasks.join(", ")
        ));
    }
    if f.test_timeout_secs == 0 {
        problems.push("[limits] test_timeout_secs is 0".to_owned());
    }
    if problems.is_empty() {
        check(
            "supervisor",
            Level::Ok,
            format!("supervisor on, {} stage(s)", f.stages),
            None,
        )
    } else {
        check(
            "supervisor",
            Level::Warn,
            format!("supervisor: {}", problems.join("; ")),
            None,
        )
    }
}

pub(crate) struct MergeFacts {
    pub gate_on: bool,
    pub gate_command_set: bool,
    pub gate_timeout_secs: u64,
    pub gate_sandbox: String,
    pub sealed: bool,
}

pub(crate) fn merge_reason(f: &MergeFacts) -> Option<&'static str> {
    use thegn_core::stage_land as sl;
    if !f.gate_on {
        Some(sl::GATE_OFF)
    } else if !f.gate_command_set {
        Some(sl::GATE_COMMAND_EMPTY)
    } else if f.gate_timeout_secs == 0 {
        Some(sl::GATE_UNBOUNDED)
    } else if !f.sealed {
        Some(sl::GATE_NOT_ISOLATED)
    } else {
        None
    }
}

pub(crate) fn check_merge(f: &MergeFacts) -> Check {
    let autonomy = match merge_reason(f) {
        None => "merge autonomy: can land unattended (the sealed gate sandbox is resolved for each repo at land time; any failure asks)".to_owned(),
        Some(r) => format!("merge autonomy: asks because {r}"),
    };
    check(
        "merge",
        Level::Info,
        format!(
            "gate_on={} gate_command={} gate_timeout_secs={} gate_sandbox={}; {autonomy}",
            f.gate_on,
            if f.gate_command_set { "set" } else { "empty" },
            f.gate_timeout_secs,
            f.gate_sandbox
        ),
        None,
    )
}

pub(crate) struct AutomationFacts {
    pub enabled: bool,
    pub config_rules: usize,
    pub lead_rules: usize,
    pub max_concurrent_lanes: u32,
    pub max_spawns_per_day: u32,
    pub paused: bool,
}

pub(crate) fn check_automations(f: &AutomationFacts) -> Check {
    let text = format!(
        "automations {}: {} config rule(s), {} Lead rule(s); budgets lanes={} spawns/day={}{}",
        if f.enabled { "enabled" } else { "disabled" },
        f.config_rules,
        f.lead_rules,
        f.max_concurrent_lanes,
        f.max_spawns_per_day,
        if f.paused { "; PAUSED" } else { "" }
    );
    let level =
        if f.paused || (f.enabled && (f.max_concurrent_lanes == 0 || f.max_spawns_per_day == 0)) {
            Level::Warn
        } else {
            Level::Info
        };
    check("automations", level, text, None)
}

pub(crate) struct PushFacts {
    pub topic_set: bool,
    pub token_is_secret_ref: bool,
    pub inbox_enabled: bool,
    pub inbox_secret_set: bool,
    pub redeem_allowed: bool,
}

pub(crate) fn check_push(f: &PushFacts) -> Check {
    if !f.topic_set {
        return check("push", Level::Warn, "push: no topic configured", None);
    }
    let mut problems = Vec::new();
    if !f.token_is_secret_ref {
        problems.push("a push topic's token is not a secret ref (env:/file:)");
    }
    if !f.inbox_enabled {
        problems.push("inbox disabled");
    } else {
        if !f.inbox_secret_set {
            problems.push("inbox_secret unset");
        }
        if !f.redeem_allowed {
            problems.push("approvals.redeem not in inbox allow");
        }
    }
    if problems.is_empty() {
        check("push", Level::Ok, "push topic, token and inbox ready", None)
    } else {
        check(
            "push",
            Level::Warn,
            format!("push: {}", problems.join("; ")),
            None,
        )
    }
}

pub(crate) struct ProducerFacts {
    pub headless: bool,
    /// `(kind, armed, lease holder)` per kind.
    pub kinds: Vec<(String, bool, Option<String>)>,
    /// Recomputed from config because no daemon answered.
    pub from_config: bool,
}

impl ProducerFacts {
    /// The daemon's own `producers.status` answer.
    pub(crate) fn from_reply(r: &thegn_core::producer_status::ProducersStatusReply) -> Self {
        ProducerFacts {
            headless: r.headless,
            kinds: r
                .kinds
                .iter()
                .map(|k| (k.kind.clone(), k.armed, k.holder.clone()))
                .collect(),
            from_config: false,
        }
    }
}

pub(crate) fn check_producers(f: &ProducerFacts) -> Check {
    let armed: Vec<String> = f
        .kinds
        .iter()
        .filter(|(_, armed, _)| *armed)
        .map(|(k, _, holder)| format!("{k} (lease: {})", holder.as_deref().unwrap_or("none")))
        .collect();
    let text = format!(
        "headless_producers={}; armed: {}",
        f.headless,
        if armed.is_empty() {
            "none".to_owned()
        } else {
            armed.join(", ")
        }
    );
    let text = if f.from_config {
        format!("{text} (computed from config; daemon not reachable)")
    } else {
        text
    };
    check("producers", Level::Info, text, None)
}

pub(crate) struct InboxFacts {
    /// `[inbox] enabled`.
    pub enabled: bool,
    /// `[daemon] headless_producers`.
    pub headless: bool,
    /// Enabled accounts of a reserved kind.
    pub reserved: Vec<String>,
    /// Enabled implemented accounts, and (daemon answering) their state:
    /// `Some(None)` = not polled yet, `Some(Some(err))` = last error
    /// (empty = ok), `None` = daemon not answering.
    pub accounts: Vec<(String, Option<Option<String>>)>,
}

/// The `inbox` row; `None` when `[inbox]` is not enabled.
pub(crate) fn check_inbox(f: &InboxFacts) -> Option<Check> {
    if !f.enabled {
        return None;
    }
    let mut problems: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    if !f.headless {
        problems.push("headless_producers is off; the inbox is never polled".to_owned());
    }
    for name in &f.reserved {
        problems.push(format!("account {name}: reserved kind, not implemented"));
    }
    for (name, state) in &f.accounts {
        match state {
            Some(Some(err)) if !err.is_empty() => problems.push(format!("account {name}: {err}")),
            Some(Some(_)) => notes.push(format!("{name}: ok")),
            Some(None) | None => notes.push(format!("{name}: not polled yet")),
        }
    }
    if problems.is_empty() {
        let text = if notes.is_empty() {
            "inbox: enabled, no accounts".to_owned()
        } else {
            format!("inbox: {}", notes.join("; "))
        };
        Some(check("inbox", Level::Info, text, None))
    } else {
        let fix = (!f.headless).then_some("set [daemon] headless_producers = true");
        let all: Vec<String> = problems.into_iter().chain(notes).collect();
        Some(check(
            "inbox",
            Level::Warn,
            format!("inbox: {}", all.join("; ")),
            fix,
        ))
    }
}

// ---- gathering ---------------------------------------------------------

fn lead_unconfigured(cfg: &Config, lead_rules: usize) -> bool {
    cfg.lead.is_none()
        && !thegn_host_clamped()
        && cfg.effective_automations().rules.is_empty()
        && lead_rules == 0
        && !cfg.pipeline.supervisor.enabled
}

fn thegn_host_clamped() -> bool {
    crate::channel_state::clamped(thegn_core::channel::Feature::Lead)
}

fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// Binaries under a masked root, unless they sit inside a re-exposed
/// (registered repo/worktree) root.
pub(crate) fn offending(bins: &[PathBuf], masks: &[PathBuf], reexposed: &[PathBuf]) -> Vec<String> {
    let mut out = Vec::new();
    for b in bins {
        if !under_any(b, masks) {
            continue;
        }
        let canon = std::fs::canonicalize(b).unwrap_or_else(|_| b.clone());
        if reexposed
            .iter()
            .any(|r| canon.starts_with(r) || b.starts_with(r))
        {
            continue;
        }
        out.push(b.display().to_string());
    }
    out
}

fn under_any(path: &Path, roots: &[PathBuf]) -> bool {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    roots
        .iter()
        .any(|r| canon.starts_with(r) || path.starts_with(r))
}

/// Inspect the Lead's readiness.
pub(crate) fn inspect(cfg: &Config) -> LeadReadiness {
    use thegn_core::store::AutomationRuleStore;
    let db = thegn_core::db::Db::open_read_only_current().ok().flatten();
    let lead_rules = db
        .as_ref()
        .and_then(|d| d.lead_rules().ok())
        .unwrap_or_default();
    if lead_unconfigured(cfg, lead_rules.len()) {
        return LeadReadiness {
            checks: Vec::new(),
            unconfigured: true,
        };
    }
    let mut checks = Vec::new();
    let clamped = thegn_host_clamped();
    checks.push(check_channel(clamped));
    if let Some(lead) = &cfg.lead {
        let harness = thegn_core::agent_task::effective_agent(cfg, &lead.agent, None)
            .ok()
            .map(|e| e.harness);
        checks.push(check_agent(&lead.agent, harness.as_deref()));
        checks.push(check_account(
            lead.account.as_deref().filter(|a| !a.trim().is_empty()),
        ));
        checks.push(check_bwrap(thegn_core::lead_sandbox::bwrap_available()));
        let isolated = lead.network == thegn_core::config_lead::LeadNetwork::Isolated;
        checks.push(check_pasta(
            isolated,
            thegn_core::lead_net::pasta_available(),
        ));
        let probe = if cfg.daemon.enabled {
            super::doctor::probe_daemon_channel(cfg)
        } else {
            None
        };
        checks.push(check_daemon(&DaemonFacts {
            enabled: cfg.daemon.enabled,
            answers: probe.is_some(),
            channel: probe.flatten(),
            preview: cfg.lead.as_ref().is_some_and(|l| l.preview),
        }));
        let state = thegn_core::util::xdg_state_home();
        let addr_path = state
            .join(crate::lead::mcp::CONTROL_DIR_REL)
            .join(crate::lead::mcp::CONTROL_ADDR_FILE);
        checks.push(check_listener(
            std::fs::read(&addr_path)
                .map_err(|e| format!("{}: {e}", addr_path.display()))
                .and_then(|b| {
                    crate::lead::mcp::parse_control_addr(&b).map_err(|e| format!("{e:#}"))
                }),
        ));
        let harness_id = harness.as_deref().unwrap_or("claude");
        let creds = state
            .join("thegn")
            .join(thegn_core::lead_sandbox::HARNESS_HOME_DIR)
            .join(harness_id)
            .join(".credentials.json");
        checks.push(check_login(creds.is_file()));
        let mut offenders = Vec::new();
        if let Ok(roots) = crate::lead::launch::masked_roots(cfg) {
            let mut bins = Vec::new();
            if let Some(cmd) = cfg.agents.iter().find(|a| a.name == lead.agent)
                && let Some(prog) = cmd.command.split_whitespace().next()
                && let Some(p) = which(prog)
            {
                bins.push(p);
            }
            if let Ok(exe) = std::env::current_exe() {
                bins.push(exe);
            }
            let mut reexposed: Vec<PathBuf> = Vec::new();
            if let Some(d) = &db {
                use thegn_core::store::WorkspaceStore;
                let mut raw: Vec<PathBuf> = d
                    .workspaces()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|w| PathBuf::from(w.repo_path))
                    .collect();
                raw.extend(
                    d.worktrees()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|w| w.location.trim().is_empty())
                        .flat_map(|w| [PathBuf::from(w.worktree), PathBuf::from(w.repo_root)]),
                );
                reexposed = raw
                    .into_iter()
                    .filter_map(|p| std::fs::canonicalize(p).ok())
                    .collect();
            }
            offenders = offending(&bins, &roots, &reexposed);
        }
        checks.push(check_masks(&offenders));
        let members = cfg.mcp_proxy.profiles.get(&lead.mcp_profile).map(|names| {
            names
                .iter()
                .map(|n| {
                    let server = cfg.mcp_servers.get(n);
                    let needs_dir = server.is_some_and(|s| {
                        s.command.first().is_some_and(|c| {
                            let base = c.rsplit('/').next().unwrap_or(c);
                            matches!(base, "npx" | "uvx" | "mcp-remote")
                        })
                    });
                    McpMember {
                        name: n.clone(),
                        exposed: server.is_some_and(|s| s.is_proxy_exposed()),
                        needs_dir,
                    }
                })
                .collect()
        });
        checks.extend(check_mcp(&McpFacts {
            profile: lead.mcp_profile.clone(),
            members,
        }));
    }
    if let Some(db) = &db {
        use thegn_core::store::WorkspaceStore;
        let roots: Vec<String> = db
            .workspaces()
            .unwrap_or_default()
            .into_iter()
            .filter(|w| w.kind == "repo")
            .map(|w| w.repo_path)
            .take(REPO_CAP)
            .collect();
        for root in roots {
            checks.push(check_workers(&worker_facts(cfg, &root)));
        }
    }
    let missing_tasks: Vec<String> = cfg
        .pipeline
        .stages
        .iter()
        .flat_map(|s| s.validate_tasks())
        .filter(|t| !cfg.tasks.iter().any(|task| task.name == *t))
        .map(str::to_owned)
        .collect();
    checks.push(check_supervisor(&SupervisorFacts {
        enabled: cfg.pipeline.supervisor.enabled,
        stages: cfg.pipeline.stages.len(),
        missing_tasks,
        test_timeout_secs: cfg.limits.test_timeout_secs,
    }));
    let mq = &cfg.merge_queue;
    checks.push(check_merge(&MergeFacts {
        gate_on: mq.gate_on,
        gate_command_set: !mq.gate_command.trim().is_empty(),
        gate_timeout_secs: mq.gate_timeout_secs,
        gate_sandbox: if mq.gate_sandbox == thegn_core::config::GateSandbox::Sealed {
            "sealed"
        } else {
            "off"
        }
        .to_owned(),
        sealed: mq.gate_sandbox == thegn_core::config::GateSandbox::Sealed,
    }));
    let auto = cfg.effective_automations();
    let paused = db
        .as_ref()
        .is_some_and(|d| thegn_core::automation_control::paused(d));
    checks.push(check_automations(&AutomationFacts {
        enabled: auto.enabled,
        config_rules: auto.rules.len(),
        lead_rules: lead_rules.len(),
        max_concurrent_lanes: auto.max_concurrent_lanes,
        max_spawns_per_day: auto.max_spawns_per_day,
        paused,
    }));
    let push = &cfg.notifications.push;
    let is_ref = |t: &str| {
        let t = t.trim();
        t.starts_with("env:") || t.starts_with("file:")
    };
    checks.push(check_push(&PushFacts {
        topic_set: !push.topic.trim().is_empty(),
        token_is_secret_ref: is_ref(&push.token)
            && push
                .sinks
                .iter()
                .all(|s| s.topic.trim().is_empty() || is_ref(&s.token)),
        inbox_enabled: push.inbox.enabled,
        inbox_secret_set: !push.inbox.inbox_secret.trim().is_empty(),
        redeem_allowed: push.inbox.allow.iter().any(|a| a == "approvals.redeem"),
    }));
    let reply = if cfg.daemon.enabled {
        super::inbox::daemon_producers_status(cfg)
    } else {
        None
    };
    checks.push(check_producers(&match &reply {
        Some(r) => ProducerFacts::from_reply(r),
        None => producer_facts(cfg, db.as_ref()),
    }));
    checks.extend(check_inbox(&inbox_facts(cfg, reply.as_ref())));
    LeadReadiness {
        checks,
        unconfigured: false,
    }
}

fn worker_facts(cfg: &Config, root: &str) -> WorkerFacts {
    use crate::agent_run::AgentDispatch;
    let gate = crate::agent_run::agent_floor_gate(
        cfg,
        root,
        true,
        thegn_core::config::IsolationFloor::SharedKernel,
        thegn_core::config::OnFloorMiss::Fail,
    );
    match gate {
        AgentDispatch::Run(Some(spec)) | AgentDispatch::RunDegraded(Some(spec), _) => WorkerFacts {
            repo: root.to_owned(),
            isolated: crate::daemon::stage_executor::host_validation_isolated(&spec),
            backend: spec.backend.label().to_owned(),
            network: format!("{:?}", spec.network).to_lowercase(),
        },
        _ => WorkerFacts {
            repo: root.to_owned(),
            isolated: false,
            backend: "none".to_owned(),
            network: "host".to_owned(),
        },
    }
}

fn inbox_facts(
    cfg: &Config,
    reply: Option<&thegn_core::producer_status::ProducersStatusReply>,
) -> InboxFacts {
    use thegn_core::seam::Kind;
    let enabled_accounts = cfg.inbox.accounts.iter().filter(|a| a.enabled);
    InboxFacts {
        enabled: cfg.inbox.enabled,
        headless: cfg.daemon.headless_producers,
        reserved: enabled_accounts
            .clone()
            .filter(|a| a.kind.is_reserved())
            .map(|a| a.name.clone())
            .collect(),
        accounts: enabled_accounts
            .filter(|a| !a.kind.is_reserved())
            .map(|a| {
                let state = reply.map(|r| {
                    r.inbox
                        .iter()
                        .find(|s| s.account == a.name)
                        .map(|s| s.last_error.clone())
                });
                (a.name.clone(), state)
            })
            .collect(),
    }
}

fn producer_facts(cfg: &Config, db: Option<&thegn_core::db::Db>) -> ProducerFacts {
    let mut rules = cfg
        .effective_automations()
        .compiled_rules()
        .unwrap_or_default();
    if let Some(db) = db {
        rules = crate::automation_runtime::with_lead_rules(db, rules);
    }
    let push = &cfg.notifications.push;
    let inputs = thegn_core::producer_arm::ArmInputs {
        daemon_enabled: cfg.daemon.enabled,
        headless: cfg.daemon.headless_producers,
        rules: &rules,
        autopilot_any: cfg.autopilot.enabled,
        pr_queue_enabled: cfg.pr_queue.enabled,
        github_mentions: cfg.notifications.github_mentions,
        push_configured: !push.topic.trim().is_empty(),
        inbox_any: thegn_core::producer_arm::inbox_armable(cfg),
    };
    let armed = thegn_core::producer_arm::armed_kinds(&inputs);
    let leases = db
        .and_then(|d| d.producer_leases().ok())
        .unwrap_or_default();
    let kinds = thegn_core::producer_lease::ProducerKind::ALL
        .into_iter()
        .map(|k| {
            let holder = leases
                .iter()
                .find(|l| l.kind == k)
                .map(|l| l.holder.clone());
            (k.as_str().to_owned(), armed.contains(&k), holder)
        })
        .collect();
    ProducerFacts {
        headless: cfg.daemon.headless_producers,
        kinds,
        from_config: true,
    }
}

// ---- output ------------------------------------------------------------

/// The text lines for a report.
pub(crate) fn lines(r: &LeadReadiness) -> Vec<String> {
    if r.unconfigured {
        return vec![NOT_CONFIGURED.to_owned()];
    }
    let mut out = vec!["Lead readiness".to_owned()];
    for c in &r.checks {
        out.push(format!("{}  {}", c.level.label(), c.text));
        if let Some(fix) = &c.fix {
            out.push(format!("      fix: {fix}"));
        }
    }
    out
}

/// Print the text report.
pub(crate) fn print(r: &LeadReadiness) {
    outln!("");
    for line in lines(r) {
        outln!("{line}");
    }
}

/// The JSON report.
pub(crate) fn json(r: &LeadReadiness) -> serde_json::Value {
    serde_json::json!({
        "checks": r.checks.iter().map(|c| serde_json::json!({
            "id": c.id,
            "level": c.level.label(),
            "text": c.text,
            "fix": c.fix,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lv(c: &Check) -> Level {
        c.level
    }

    #[test]
    fn channel_ok_and_miss() {
        assert_eq!(lv(&check_channel(false)), Level::Ok);
        let c = check_channel(true);
        assert_eq!(c.level, Level::Fail);
        assert!(c.fix.unwrap().contains("preview on stable"));
    }

    #[test]
    fn agent_ok_and_miss() {
        assert_eq!(lv(&check_agent("a", Some("claude"))), Level::Ok);
        assert_eq!(lv(&check_agent("a", Some("codex"))), Level::Fail);
        assert_eq!(lv(&check_agent("a", None)), Level::Fail);
    }

    #[test]
    fn account_ok_and_miss() {
        assert_eq!(lv(&check_account(None)), Level::Ok);
        let c = check_account(Some("x"));
        assert_eq!(c.level, Level::Fail);
        assert!(c.fix.unwrap().contains("thegn lead login"));
    }

    #[test]
    fn bwrap_ok_and_miss() {
        assert_eq!(lv(&check_bwrap(true)), Level::Ok);
        assert_eq!(lv(&check_bwrap(false)), Level::Fail);
    }

    #[test]
    fn pasta_ok_miss_and_host() {
        assert_eq!(lv(&check_pasta(true, true)), Level::Ok);
        assert_eq!(lv(&check_pasta(true, false)), Level::Fail);
        assert_eq!(lv(&check_pasta(false, false)), Level::Fail);
    }

    #[test]
    fn daemon_ok_and_miss() {
        assert_eq!(
            lv(&check_daemon(&DaemonFacts {
                enabled: true,
                answers: true,
                channel: Some("dev".into()),
                preview: false,
            })),
            Level::Ok
        );
        assert_eq!(
            lv(&check_daemon(&DaemonFacts {
                enabled: true,
                answers: true,
                channel: Some("stable".into()),
                preview: false,
            })),
            Level::Fail
        );
        assert_eq!(
            lv(&check_daemon(&DaemonFacts {
                enabled: true,
                answers: true,
                channel: None,
                preview: false,
            })),
            Level::Ok
        );
        assert_eq!(
            lv(&check_daemon(&DaemonFacts {
                enabled: false,
                answers: false,
                channel: None,
                preview: false,
            })),
            Level::Fail
        );
        assert_eq!(
            lv(&check_daemon(&DaemonFacts {
                enabled: true,
                answers: false,
                channel: None,
                preview: false,
            })),
            Level::Fail
        );
    }

    #[test]
    fn a_stable_daemon_is_fine_for_a_preview_lead() {
        let facts = |preview| DaemonFacts {
            enabled: true,
            answers: true,
            channel: Some("stable".into()),
            preview,
        };
        assert_eq!(lv(&check_daemon(&facts(true))), Level::Ok);
        assert_eq!(lv(&check_daemon(&facts(false))), Level::Fail);
    }

    #[test]
    fn offending_skips_reexposed() {
        let masks = vec![PathBuf::from("/m")];
        let bins = vec![PathBuf::from("/m/wt/bin")];
        assert_eq!(offending(&bins, &masks, &[]).len(), 1);
        assert!(offending(&bins, &masks, &[PathBuf::from("/m/wt")]).is_empty());
    }

    #[test]
    fn listener_ok_and_miss() {
        assert_eq!(
            lv(&check_listener(Ok("127.0.0.1:9".parse().unwrap()))),
            Level::Ok
        );
        assert_eq!(lv(&check_listener(Err("gone".into()))), Level::Fail);
    }

    #[test]
    fn login_ok_and_miss() {
        assert_eq!(lv(&check_login(true)), Level::Ok);
        assert_eq!(lv(&check_login(false)), Level::Warn);
    }

    #[test]
    fn masks_ok_and_miss() {
        assert_eq!(lv(&check_masks(&[])), Level::Ok);
        assert_eq!(lv(&check_masks(&["/x/claude".into()])), Level::Fail);
    }

    #[test]
    fn mcp_ok_miss_and_needs_dir() {
        let m = |name: &str, exposed, needs_dir| McpMember {
            name: name.into(),
            exposed,
            needs_dir,
        };
        let ok = check_mcp(&McpFacts {
            profile: "lead".into(),
            members: Some(vec![m("a", true, false)]),
        });
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].level, Level::Ok);
        let none = check_mcp(&McpFacts {
            profile: "lead".into(),
            members: None,
        });
        assert_eq!(none[0].level, Level::Warn);
        let bad = check_mcp(&McpFacts {
            profile: "lead".into(),
            members: Some(vec![m("a", false, true)]),
        });
        assert_eq!(bad[0].level, Level::Warn);
        assert_eq!(bad[1].level, Level::Info);
        assert!(bad[1].text.contains("thegn lead login --mcp a"));
    }

    #[test]
    fn workers_ok_and_miss() {
        let w = |isolated| WorkerFacts {
            repo: "/r".into(),
            isolated,
            backend: "bwrap".into(),
            network: "none".into(),
        };
        assert_eq!(lv(&check_workers(&w(true))), Level::Ok);
        let c = check_workers(&w(false));
        assert_eq!(c.level, Level::Warn);
        assert!(c.fix.unwrap().contains("sealed has no network"));
    }

    #[test]
    fn supervisor_ok_and_miss() {
        let f = |enabled, stages, missing: Vec<String>, t| SupervisorFacts {
            enabled,
            stages,
            missing_tasks: missing,
            test_timeout_secs: t,
        };
        assert_eq!(lv(&check_supervisor(&f(false, 0, vec![], 0))), Level::Info);
        assert_eq!(lv(&check_supervisor(&f(true, 2, vec![], 60))), Level::Ok);
        assert_eq!(lv(&check_supervisor(&f(true, 0, vec![], 60))), Level::Warn);
        assert_eq!(
            lv(&check_supervisor(&f(true, 2, vec!["t".into()], 60))),
            Level::Warn
        );
        assert_eq!(lv(&check_supervisor(&f(true, 2, vec![], 0))), Level::Warn);
    }

    #[test]
    fn merge_unattended_and_asks() {
        let mut f = MergeFacts {
            gate_on: true,
            gate_command_set: true,
            gate_timeout_secs: 60,
            gate_sandbox: "sealed".into(),
            sealed: true,
        };
        let c = check_merge(&f);
        assert_eq!(c.level, Level::Info);
        assert!(c.text.contains("can land unattended"));
        f.sealed = false;
        assert!(check_merge(&f).text.contains("asks because"));
        f.gate_on = false;
        assert!(
            check_merge(&f)
                .text
                .contains(thegn_core::stage_land::GATE_OFF)
        );
    }

    #[test]
    fn automations_info_and_paused_warn() {
        let mut f = AutomationFacts {
            enabled: true,
            config_rules: 1,
            lead_rules: 2,
            max_concurrent_lanes: 2,
            max_spawns_per_day: 5,
            paused: false,
        };
        assert_eq!(lv(&check_automations(&f)), Level::Info);
        f.paused = true;
        assert_eq!(lv(&check_automations(&f)), Level::Warn);
    }

    #[test]
    fn push_ok_and_miss() {
        let good = PushFacts {
            topic_set: true,
            token_is_secret_ref: true,
            inbox_enabled: true,
            inbox_secret_set: true,
            redeem_allowed: true,
        };
        assert_eq!(lv(&check_push(&good)), Level::Ok);
        let bad = PushFacts {
            token_is_secret_ref: false,
            redeem_allowed: false,
            ..good
        };
        let c = check_push(&bad);
        assert_eq!(c.level, Level::Warn);
        assert!(c.text.contains("secret ref") && c.text.contains("approvals.redeem"));
    }

    #[test]
    fn producers_lists_armed_kinds_with_holders() {
        let c = check_producers(&ProducerFacts {
            headless: true,
            kinds: vec![
                ("tracker".into(), true, Some("daemon".into())),
                ("pr_poll".into(), false, None),
            ],
            from_config: false,
        });
        assert_eq!(c.level, Level::Info);
        assert!(c.text.contains("tracker (lease: daemon)"));
        assert!(!c.text.contains("pr_poll"));
    }

    #[test]
    fn producers_reads_the_daemon_reply_or_notes_the_fallback() {
        use thegn_core::producer_status::{ProducerKindStatus, ProducersStatusReply};
        let reply = ProducersStatusReply {
            installed: true,
            headless: true,
            kinds: vec![ProducerKindStatus {
                kind: "inbox".into(),
                armed: true,
                holder: Some("daemon".into()),
                ..Default::default()
            }],
            inbox: vec![],
        };
        let c = check_producers(&ProducerFacts::from_reply(&reply));
        assert!(c.text.contains("inbox (lease: daemon)"));
        assert!(!c.text.contains("computed from config"));
        let mut f = ProducerFacts::from_reply(&reply);
        f.from_config = true;
        assert!(
            check_producers(&f)
                .text
                .contains("(computed from config; daemon not reachable)")
        );
    }

    fn inbox_facts_for_test(headless: bool) -> InboxFacts {
        InboxFacts {
            enabled: true,
            headless,
            reserved: vec![],
            accounts: vec![("work".into(), Some(None))],
        }
    }

    #[test]
    fn inbox_row_cases() {
        let off = InboxFacts {
            enabled: false,
            ..inbox_facts_for_test(true)
        };
        assert!(check_inbox(&off).is_none());
        let c = check_inbox(&inbox_facts_for_test(false)).unwrap();
        assert_eq!(lv(&c), Level::Warn);
        assert_eq!(
            c.fix.as_deref(),
            Some("set [daemon] headless_producers = true")
        );
        let c = check_inbox(&inbox_facts_for_test(true)).unwrap();
        assert_eq!(lv(&c), Level::Info);
        assert!(c.text.contains("work: not polled yet"));
        let mut f = inbox_facts_for_test(true);
        f.accounts = vec![("work".into(), Some(Some("auth failed".into())))];
        let c = check_inbox(&f).unwrap();
        assert_eq!(lv(&c), Level::Warn);
        assert!(c.text.contains("account work: auth failed"));
        let mut f = inbox_facts_for_test(true);
        f.accounts = vec![("work".into(), Some(Some(String::new())))];
        assert!(check_inbox(&f).unwrap().text.contains("work: ok"));
        let mut f = inbox_facts_for_test(true);
        f.reserved = vec!["chat".into()];
        assert!(check_inbox(&f).unwrap().text.contains("reserved kind"));
    }

    #[test]
    fn unconfigured_prints_exactly_one_line() {
        let r = LeadReadiness {
            checks: Vec::new(),
            unconfigured: true,
        };
        assert_eq!(
            lines(&r),
            vec!["Lead: not configured (see `thegn help lead`)".to_owned()]
        );
    }

    #[test]
    fn text_and_json_shape() {
        let r = LeadReadiness {
            checks: vec![check_bwrap(false)],
            unconfigured: false,
        };
        assert_eq!(
            lines(&r),
            vec![
                "Lead readiness",
                "fail  bubblewrap (bwrap) is not available",
                "      fix: install bubblewrap"
            ]
        );
        let j = json(&r);
        let c = &j["checks"][0];
        assert_eq!(c["id"], "bwrap");
        assert_eq!(c["level"], "fail");
        assert_eq!(c["fix"], "install bubblewrap");
        assert!(c["text"].is_string());
    }
}
