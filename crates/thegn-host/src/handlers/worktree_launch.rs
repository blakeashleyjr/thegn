//! The relaunch half of session resurrection (THE-84): a worktree tab whose
//! remembered agent is gone after a restart brings the AGENT back, not a
//! blank shell.
//!
//! After a daemon/UI restart the spec-resolving workers (materialize +
//! prewarm) resolve every missing leaf as a plain `"shell"` — so a worktree
//! that ran an agent reopens as an idle login shell and the tab reads blank
//! (`zsh -l`, no prompt). THE-85 fixed the record half (the shell resolutions
//! no longer clobber `worktrees.agent`) and the attach half (a LIVE daemon
//! session wins); this module is the relaunch half, which the `agent` spec
//! (`openspec/specs/agent/spec.md`, "The worktree remembers its agent" →
//! "session resurrection relaunches the remembered agent") already contracts.
//!
//! [`remembered_agent_relaunch`] is the whole decision: a fail-open ladder
//! over the remembered `worktrees.agent` row that composes the agent's spec
//! through the SAME machinery an interactive launch uses
//! ([`crate::direnv_warm::launch_spec_synced_with`] — sandbox, credentials,
//! bundles, the CPU cap and daemon-persistence by construction), optionally
//! resumed through the harness's resume form when the `[[agents]]` entry
//! opted in. [`apply_relaunch`] is the call-site fold the three spec workers
//! apply to their resolved batch.
//!
//! Blocking (SQLite, an optional bounded session-store walk, sandbox
//! preparation): **off-loop callers only** — the workers already run in
//! `spawn_blocking`.
//!
//! Invariants:
//! - **A live session always wins.** Callers gate on the THE-85 attach probe
//!   being empty, so a running agent is attached, never doubled.
//! - **A split/add stays a shell.** The materialize caller gates on `!quiet`
//!   (a split into a tab with a live pane is a shell gesture); prewarm is
//!   never a split.
//! - **`worktrees.agent` is never rewritten.** The relaunch passes
//!   `suppress_agent_record: true` — resurrection is not a choice event; the
//!   record already holds this agent, and a relaunch must never be able to
//!   change it.
//! - **The relaunched session is worktree-tagged end-to-end** (spec cwd →
//!   `LazyDaemonSource.worktree` → `SessionMeta.worktree`), so the NEXT open
//!   ATTACHes to it via THE-85 instead of relaunching again. No second dedup
//!   mechanism is added here.

use std::collections::{HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};

use thegn_core::calendar::display::{DisplayText, Field};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::log_redact::redact_text_line;
use thegn_core::store::WorkspaceStore;

use crate::agent::LaunchSpec;
use crate::handlers::provision::SpecError;

const MAX_REFUSAL_REASON_CHARS: usize = 512;
const MAX_REPORTED_REFUSALS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RelaunchRefusalKind {
    ProviderOwnership,
    Sandbox,
    ProviderLaunch,
}

impl RelaunchRefusalKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::ProviderOwnership => "provider-ownership",
            Self::Sandbox => "sandbox",
            Self::ProviderLaunch => "provider-launch",
        }
    }
}

#[derive(Debug)]
pub(crate) struct RelaunchRefusal {
    agent: String,
    source: anyhow::Error,
}

#[derive(Debug, Default)]
struct RefusalRegistry {
    reported: HashSet<(String, String, RelaunchRefusalKind)>,
    order: VecDeque<(String, String, RelaunchRefusalKind)>,
}

impl RefusalRegistry {
    fn admit(&mut self, key: (String, String, RelaunchRefusalKind)) -> bool {
        if self.reported.contains(&key) {
            return false;
        }
        // This is process-lifetime diagnostic state, not a ledger. Keep only a
        // small FIFO window so long-lived hosts do not retain every worktree;
        // an evicted refusal may be reported again, which is acceptable here.
        if self.order.len() == MAX_REPORTED_REFUSALS
            && let Some(evicted) = self.order.pop_front()
        {
            self.reported.remove(&evicted);
        }
        self.reported.insert(key.clone());
        self.order.push_back(key);
        true
    }
}

static REPORTED_REFUSALS: LazyLock<Mutex<RefusalRegistry>> =
    LazyLock::new(|| Mutex::new(RefusalRegistry::default()));

/// The relaunch fold's internal status. A refusal intentionally leaves the
/// already-resolved shell in the successful batch, so the fail-open launch
/// contract is unchanged while callers can distinguish refusal from success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelaunchOutcome {
    NotAttempted,
    Relaunched,
    Refused,
}

fn refusal_kind(error: &anyhow::Error) -> RelaunchRefusalKind {
    if error.chain().any(|source| {
        source
            .downcast_ref::<crate::agent::DevcontainerLaunchRefused>()
            .is_some()
    }) {
        RelaunchRefusalKind::ProviderOwnership
    } else if error
        .chain()
        .any(|source| source.downcast_ref::<crate::agent::SandboxHalt>().is_some())
    {
        RelaunchRefusalKind::Sandbox
    } else {
        // Keep one generic bucket for provider/launch failures so future typed
        // refusal categories can be added without changing this call site.
        RelaunchRefusalKind::ProviderLaunch
    }
}

fn bounded_reason(error: &anyhow::Error) -> String {
    let redacted: String = redact_text_line(&error.to_string())
        .chars()
        .take(MAX_REFUSAL_REASON_CHARS)
        .collect();
    let reason = DisplayText::new(&redacted, Field::Reminder).into_string();
    if reason.is_empty() {
        "no safe detail available".to_string()
    } else {
        reason
    }
}

fn admit_refusal(worktree: &str, agent: &str, kind: RelaunchRefusalKind) -> bool {
    let key = (worktree.to_string(), agent.to_string(), kind);
    match REPORTED_REFUSALS.lock() {
        Ok(mut reported) => reported.admit(key),
        Err(poisoned) => poisoned.into_inner().admit(key),
    }
}

fn report_relaunch_refusal(refusal: RelaunchRefusal, worktree: &str) {
    let kind = refusal_kind(&refusal.source);
    if !admit_refusal(worktree, &refusal.agent, kind) {
        return;
    }

    let reason = bounded_reason(&refusal.source);
    let worktree = DisplayText::new(worktree, Field::Location);
    let agent = DisplayText::new(&refusal.agent, Field::Title);
    // WARN is intentional: with THEGN_LOG unset, only the always-on WARN+
    // diagnostics ring remains available to the debug bundle and doctor.
    tracing::warn!(
        target: "thegn::agent",
        worktree = %worktree.as_str(),
        agent = %agent.as_str(),
        refusal_kind = kind.as_str(),
        reason = %reason,
        "remembered agent refused; shell fallback retained; fix provider/account/sandbox configuration and reopen or retry"
    );
}

/// When the THE-85 attach probe found NO live daemon session for `worktree`,
/// a fresh (re)bring-up relaunches the worktree's remembered agent as
/// `leaf`'s process — resuming its last harness session when the
/// `[[agents]]` entry opted in (`resume = true`) and the harness advertises
/// the RESUME capability. `Ok(None)` ⇒ keep the resolved shell spec; `Err` is
/// only the final launch-spec refusal that the caller reports before keeping
/// that same shell.
///
/// Every lookup gate fails open to `Ok(None)` (today's shell): an unremembered
/// / `"shell"` / `"clean-shell"` / tool-drawer record, or an entry that has
/// left the config, all remain silent and degrade honestly.
pub(crate) fn remembered_agent_relaunch(
    cfg: &Config,
    worktree: &str,
    leaf: u32,
) -> Result<Option<(u32, LaunchSpec)>, RelaunchRefusal> {
    let db = match Db::open() {
        Ok(db) => db,
        Err(_) => return Ok(None),
    };
    let name = match db.worktree_agent(worktree) {
        Ok(Some(name)) => name,
        Ok(None) | Err(_) => return Ok(None),
    };
    // The native-exec path's exclusions (`panes.rs`): a plain-shell or
    // transient `clean-shell` watchdog record — or a tool drawer
    // (yazi/lazygit/…) — is not the worktree's agent; relaunching one would
    // resurrect an overlay, not the agent the user picked.
    if name == "shell" || name == "clean-shell" || cfg.tool_command(&name).is_some() {
        return Ok(None);
    }
    // Entry no longer configured (config churn): leave the stale record alone
    // — the sidebar keeps attributing until the agent is re-added — but a
    // shell pane is still the honest spawn.
    if cfg.agent_command(&name).is_none() {
        return Ok(None);
    }
    // Resolve the resume form FIRST (cheap config read): the session-store
    // walk must never run for a non-opted entry.
    let cmd_override = resume_command_override(cfg, &name, worktree, &db);
    crate::direnv_warm::launch_spec_synced_with(
        cfg,
        worktree,
        None,
        &name,
        crate::agent::LaunchExtras {
            cmd_override: cmd_override.as_deref(),
            prompt: None,
            // Resurrection is not a choice event: the record already holds
            // this agent, and a relaunch must never be able to change it.
            suppress_agent_record: true,
            stage: None,
        },
    )
    // Fail open: an unresolvable agent spec (provider down, sandbox error)
    // degrades to the already-resolved shell, never to a failed tab. Preserve
    // this error for the diagnostic seam while leaving every earlier
    // not-applicable outcome as a silent `Ok(None)`.
    .map(|spec| Some((leaf, spec)))
    .map_err(|source| RelaunchRefusal {
        agent: name,
        source,
    })
}

/// The resume-form command for a remembered agent, when it applies: the
/// entry opted in (`resume = true`), a session was discovered whose recorded
/// cwd is this worktree (newest first, bounded walk), the harness advertises
/// RESUME, and the id passes the shape check. Any miss ⇒ `None` — a cold
/// launch of the entry's interactive command, exactly as before.
fn resume_command_override(cfg: &Config, name: &str, worktree: &str, db: &Db) -> Option<String> {
    // Cheap opt-in check FIRST — the filesystem walk never runs for
    // non-opted entries.
    let entry = cfg.agents.iter().find(|a| a.name == name)?;
    if !entry.resume {
        return None;
    }
    // The walker sorts newest-first and bounds itself (MAX_SESSIONS reads);
    // it never errors — an unreadable store contributes nothing. `known` only
    // sets each record's unlinked flag; a DB miss is an empty set.
    let known: HashSet<String> = db
        .worktrees()
        .map(|rows| rows.into_iter().map(|r| r.worktree).collect())
        .unwrap_or_default();
    let newest = thegn_svc::sessions::discover(
        cfg,
        &thegn_svc::sessions::SessionFilter {
            worktree: Some(worktree),
            harness: None,
        },
        &known,
    )
    .into_iter()
    .next()?;
    // Re-checks the opt-in, the harness's RESUME cap, and the id shape.
    let id = thegn_core::agent_task::auto_resume_id(cfg, name, Some(&newest.id))?;
    // The same composer the daemon's `sessions.open` agent path uses:
    // id-shape-validated, refuses non-RESUME harnesses. (`continue_last` is
    // the THE-86 transport-retry form — a resurrection relaunch resumes an
    // EXPLICIT discovered session, it never continues blind.)
    crate::daemon::agent_open::command_for(cfg, name, "", false, Some(&id), false, None)
        .ok()
        .map(|cmd| cmd.to_string())
}

/// Fold the relaunch override into a resolved shell batch — the call-site
/// shape all three spec workers share: replace the FIRST missing leaf's
/// resolved shell with the worktree's remembered agent when the THE-85 probe
/// found no live session and this is not a quiet split/add.
///
/// `first_leaf` is `missing[0]` in tree order (the primary leaf — the shape
/// the original spawn had, the agent in the center pane), `None`-safe by
/// construction. Callers compute the gates: `attach_is_empty` (a live
/// session still wins and the agent is never doubled), `quiet_split` (`true`
/// for a split/add into a tab that already has a live pane) — and, at the
/// prewarm site, wrap the call in `!is_terminal` (terminal groups host no
/// agent sessions). The returned outcome records a refusal without changing
/// the successful shell batch; callers that only need fail-open behavior may
/// intentionally discard it.
pub(crate) fn apply_relaunch(
    specs: &mut Result<Vec<(u32, LaunchSpec)>, SpecError>,
    cfg: &Config,
    worktree: &str,
    first_leaf: Option<u32>,
    attach_is_empty: bool,
    quiet_split: bool,
) -> RelaunchOutcome {
    apply_relaunch_with(
        specs,
        cfg,
        worktree,
        first_leaf,
        attach_is_empty,
        quiet_split,
        remembered_agent_relaunch,
    );
}

fn apply_relaunch_with(
    specs: &mut Result<Vec<(u32, LaunchSpec)>, SpecError>,
    cfg: &Config,
    worktree: &str,
    first_leaf: Option<u32>,
    attach_is_empty: bool,
    quiet_split: bool,
    relaunch: impl FnOnce(&Config, &str, u32) -> Result<Option<(u32, LaunchSpec)>, RelaunchRefusal>,
) -> RelaunchOutcome {
    if attach_is_empty
        && !quiet_split
        && let Some(first_leaf) = first_leaf
        && let Ok(resolved) = specs.as_mut()
    {
        match relaunch(cfg, worktree, first_leaf) {
            Ok(Some((leaf, spec))) => {
                if let Some(slot) = resolved.iter_mut().find(|(id, _)| *id == leaf) {
                    slot.1 = spec;
                    return RelaunchOutcome::Relaunched;
                }
            }
            Ok(None) => {}
            Err(refusal) => {
                report_relaunch_refusal(refusal, worktree);
                return RelaunchOutcome::Refused;
            }
        }
    }
    RelaunchOutcome::NotAttempted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvVarGuard;
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex as StdMutex};
    use thegn_core::config::{SandboxBackend, UsageConfig};

    #[derive(Clone)]
    struct CapturedEvents(Arc<StdMutex<Vec<String>>>);

    impl tracing::Subscriber for CapturedEvents {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut fields = String::new();
            event.record(&mut DisplayFields(&mut fields));
            self.0.lock().unwrap().push(format!(
                "{} {:?} {fields}",
                event.metadata().target(),
                event.metadata().level()
            ));
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    struct DisplayFields<'a>(&'a mut String);

    impl tracing::field::Visit for DisplayFields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            write!(self.0, " {}={value:?}", field.name()).expect("String write cannot fail");
        }
    }

    /// A scratch state dir + isolated env — this shell often runs inside a
    /// live thegn (CLAUDE.md), and `Db::open()` reads `XDG_STATE_HOME` per
    /// call. Serialized on the crate-wide env lock, held for the whole span.
    fn with_state<T>(tag: &str, f: impl FnOnce(&std::path::Path) -> T) -> T {
        let dir = std::env::temp_dir().join(format!("tg-wtlaunch-{tag}-{}", std::process::id()));
        // best-effort scratch-dir cleanup (test setup + teardown): a stale dir
        // is harmless, and the non-`let _` form keeps this file off the
        // ignored-result ratchet.
        std::fs::remove_dir_all(&dir).unwrap_or_default();
        std::fs::create_dir_all(&dir).unwrap();
        let _env = EnvVarGuard::set(&[("XDG_STATE_HOME", dir.to_str().unwrap())]);
        let out = f(&dir);
        std::fs::remove_dir_all(&dir).unwrap_or_default();
        out
    }

    /// A unique worktree path (need not exist — the resolver treats a
    /// non-git path as local; same shape as agent_tests).
    fn worktree_path(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("tg-wtlaunch-{tag}-wt-{}", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    /// Host backend (resolves without a sandbox runtime) + the named
    /// `[[agents]]` entries, each named after its command.
    fn cfg_with(agents: &[(&str, bool)]) -> Config {
        let mut c = Config::default();
        for (name, resume) in agents {
            c.agents.push(thegn_core::config::NamedCommand {
                name: (*name).into(),
                command: (*name).into(),
                hints: Vec::new(),
                provider: None,
                harness: None,
                model: None,
                env: Default::default(),
                permissions: Vec::new(),
                resume: *resume,
                route_via_proxy: false,
                drawer_scope: None,
                drawer_cwd: None,
            });
        }
        c.sandbox.backend = SandboxBackend::Auto;
        c.sandbox.backend_chain = vec!["host".to_string()];
        c
    }

    /// Register the worktree row (the wizard's shape) and remember `agent`
    /// for it.
    fn register(wt: &str, agent: &str) {
        let db = Db::open().unwrap();
        db.put_worktree("app/wt", "/x/app", wt, "tg/wt", None, None)
            .unwrap();
        db.set_worktree_agent(wt, agent).unwrap();
    }

    fn remembered(wt: &str) -> Option<String> {
        Db::open().unwrap().worktree_agent(wt).unwrap()
    }

    /// Point the (hermetic) session discovery at `home`: a configured account
    /// home means default homes are skipped (`usage::candidate_homes`:
    /// explicit beats implicit), so the real `~/.claude` never leaks in.
    fn with_usage(c: &mut Config, home: &std::path::Path) {
        c.usage = UsageConfig {
            enabled: true,
            allow_network: false,
            discover_profiles: false,
            profile_roots: Vec::new(),
            providers: vec!["claude".to_string()],
            accounts: vec![thegn_core::usage::UsageAccount {
                name: "t".into(),
                provider: "claude".into(),
                dir: home.display().to_string(),
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        };
    }

    /// Seed one claude transcript whose recorded cwd is `wt`.
    fn seed_session(home: &std::path::Path, wt: &str, id: &str) {
        let proj = home.join("projects").join("seeded");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join(format!("{id}.jsonl")),
            format!(
                r#"{{"type":"user","cwd":"{wt}","message":{{"role":"user","content":"Fix the bug"}}}}
{{"type":"assistant","message":{{"content":"ok"}}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn shell_record_never_relaunches() {
        with_state("shell-record", |_| {
            let cfg = cfg_with(&[("claude", false)]);
            let wt = worktree_path("shell-record");
            // No row at all, an empty record, and a plain-shell record all
            // stay a shell.
            assert!(remembered_agent_relaunch(&cfg, &wt, 3).unwrap().is_none());
            Db::open()
                .unwrap()
                .put_worktree("app/wt", "/x/app", &wt, "tg/wt", None, None)
                .unwrap();
            assert!(remembered_agent_relaunch(&cfg, &wt, 3).unwrap().is_none());
            register(&wt, "shell");
            assert!(remembered_agent_relaunch(&cfg, &wt, 3).unwrap().is_none());
        });
    }

    #[test]
    fn tool_drawer_record_never_relaunches() {
        with_state("tool-record", |_| {
            let mut cfg = cfg_with(&[("claude", false)]);
            cfg.tools.push(thegn_core::config::NamedCommand {
                name: "yazi".into(),
                command: "yazi".into(),
                hints: Vec::new(),
                provider: None,
                harness: None,
                model: None,
                env: Default::default(),
                permissions: Vec::new(),
                resume: false,
                route_via_proxy: false,
                drawer_scope: None,
                drawer_cwd: None,
            });
            let wt = worktree_path("tool-record");
            register(&wt, "yazi");
            assert!(
                remembered_agent_relaunch(&cfg, &wt, 3).unwrap().is_none(),
                "a remembered tool drawer is an overlay, not the worktree's agent"
            );
        });
    }

    #[test]
    fn unconfigured_agent_falls_back_to_shell() {
        with_state("unconfigured", |_| {
            let cfg = cfg_with(&[("claude", false)]);
            let wt = worktree_path("unconfigured");
            register(&wt, "vanished-agent");
            assert!(
                remembered_agent_relaunch(&cfg, &wt, 3).unwrap().is_none(),
                "a record naming an agent absent from config stays a shell"
            );
            // The stale record is left alone for the sidebar's attribution.
            assert_eq!(remembered(&wt).as_deref(), Some("vanished-agent"));
        });
    }

    #[test]
    fn remembered_agent_relanches_cold_by_default() {
        with_state("cold-default", |state| {
            let mut cfg = cfg_with(&[("claude", false)]);
            let home = state.join("claude-home");
            with_usage(&mut cfg, &home);
            let wt = worktree_path("cold-default");
            register(&wt, "claude");
            // A resumable session exists — if the resume path ran despite
            // `resume = false` this would show in the argv.
            seed_session(&home, &wt, "0c1f-uuid");

            let (leaf, spec) = remembered_agent_relaunch(&cfg, &wt, 3)
                .unwrap()
                .expect("relaunches");
            assert_eq!(leaf, 3, "the relaunch pins the leaf it was asked for");
            let argv = spec.argv.join(" ");
            assert!(argv.contains("claude"), "the entry's command: {argv}");
            assert!(
                !argv.contains("--resume"),
                "`resume = false` launches cold by default: {argv}"
            );
        });
    }

    #[test]
    fn resume_composes_the_harness_resume_form() {
        with_state("resume-form", |state| {
            let mut cfg = cfg_with(&[("claude", true)]);
            let home = state.join("claude-home");
            with_usage(&mut cfg, &home);
            let wt = worktree_path("resume-form");
            register(&wt, "claude");
            seed_session(&home, &wt, "0c1f-uuid");

            let (_, spec) = remembered_agent_relaunch(&cfg, &wt, 3)
                .unwrap()
                .expect("relaunches");
            let argv = spec.argv.join(" ");
            assert!(
                argv.contains("--resume 0c1f-uuid"),
                "the newest session's id, shape-validated, in the harness resume form: {argv}"
            );
        });
    }

    #[test]
    fn resume_without_a_session_launches_cold() {
        with_state("resume-empty", |state| {
            let mut cfg = cfg_with(&[("claude", true)]);
            // Opted in, but the session store does not exist.
            let home = state.join("absent-home");
            with_usage(&mut cfg, &home);
            let wt = worktree_path("resume-empty");
            register(&wt, "claude");

            let (_, spec) = remembered_agent_relaunch(&cfg, &wt, 3)
                .unwrap()
                .expect("relaunches");
            let argv = spec.argv.join(" ");
            assert!(argv.contains("claude"), "cold launch: {argv}");
            assert!(!argv.contains("--resume"), "no session to resume: {argv}");
        });
    }

    #[test]
    fn the_record_is_never_written_by_a_relaunch() {
        with_state("record-untouched", |_| {
            let cfg = cfg_with(&[("claude", false)]);
            let wt = worktree_path("record-untouched");
            register(&wt, "claude");
            let before = remembered(&wt);

            let (_, spec) = remembered_agent_relaunch(&cfg, &wt, 3)
                .unwrap()
                .expect("relaunches");
            assert!(spec.argv.join(" ").contains("claude"));

            // Mirror of agent_tests' suppression test: the relaunch is not a
            // choice event and must not be able to change `worktrees.agent`.
            assert_eq!(
                remembered(&wt),
                before,
                "a relaunch must leave the remembered agent byte-identical"
            );
        });
    }

    /// Worker-level contract (materialize + prewarm share the fold): the
    /// first missing leaf carries the agent argv, the remaining leaves keep
    /// the resolved shell argv; a non-empty attach list (a LIVE session) or a
    /// quiet split leaves the whole batch untouched.
    #[test]
    fn relaunch_pins_the_first_leaf_and_a_live_attach_wins() {
        with_state("batch", |_| {
            let cfg = cfg_with(&[("claude", false)]);
            let wt = worktree_path("batch");
            register(&wt, "claude");

            let shell = || LaunchSpec {
                argv: vec![
                    "/bin/sh".into(),
                    "-lc".into(),
                    "${SHELL:-/bin/sh} -l".into(),
                ],
                cwd: None,
                env: Vec::new(),
                backend: "host".into(),
                warnings: Vec::new(),
                degraded: false,
                remote: false,
            };
            let mut batch: Result<Vec<(u32, LaunchSpec)>, SpecError> =
                Ok(vec![(3, shell()), (5, shell())]);

            // Probe empty, not a quiet split ⇒ leaf 3 becomes the agent.
            assert_eq!(
                apply_relaunch(&mut batch, &cfg, &wt, Some(3), true, false),
                RelaunchOutcome::Relaunched
            );
            let Ok(resolved) = batch.as_ref() else {
                panic!("the batch is always Ok in this test");
            };
            assert!(
                resolved[0].1.argv.join(" ").contains("claude"),
                "the first leaf carries the agent argv: {:?}",
                resolved[0].1.argv
            );
            assert_eq!(
                resolved[1].1.argv[2], "${SHELL:-/bin/sh} -l",
                "the remaining leaves keep the resolved shell argv"
            );

            // A live session wins: the batch is untouched.
            let mut batch: Result<Vec<(u32, LaunchSpec)>, SpecError> =
                Ok(vec![(3, shell()), (5, shell())]);
            assert_eq!(
                apply_relaunch(&mut batch, &cfg, &wt, Some(3), false, false),
                RelaunchOutcome::NotAttempted
            );
            let Ok(resolved) = batch.as_ref() else {
                panic!("the batch is always Ok in this test");
            };
            assert!(
                resolved
                    .iter()
                    .all(|(_, s)| !s.argv.join(" ").contains("claude")),
                "a live attach suppresses the relaunch entirely"
            );

            // A quiet split/add is a shell gesture: the batch is untouched.
            let mut batch: Result<Vec<(u32, LaunchSpec)>, SpecError> =
                Ok(vec![(3, shell()), (5, shell())]);
            assert_eq!(
                apply_relaunch(&mut batch, &cfg, &wt, Some(3), true, true),
                RelaunchOutcome::NotAttempted
            );
            let Ok(resolved) = batch.as_ref() else {
                panic!("the batch is always Ok in this test");
            };
            assert!(
                resolved
                    .iter()
                    .all(|(_, s)| !s.argv.join(" ").contains("claude")),
                "a quiet split stays a shell"
            );
        });
    }

    fn shell_spec() -> LaunchSpec {
        LaunchSpec {
            argv: vec![
                "/bin/sh".into(),
                "-lc".into(),
                "printf shell-fallback".into(),
            ],
            cwd: None,
            env: Vec::new(),
            backend: "host".into(),
            warnings: Vec::new(),
            degraded: false,
            remote: false,
        }
    }

    #[test]
    fn typed_refusal_keeps_shell_and_is_deduped() {
        with_state("typed-refusal", |_| {
            let cfg = cfg_with(&[("codex", false)]);
            let worktree = worktree_path("typed-refusal");
            register(&worktree, "codex");
            let refusal = || {
                Err(RelaunchRefusal {
                    agent: "codex".into(),
                    source: anyhow::Error::new(crate::agent::DevcontainerLaunchRefused {
                        reason: "trusted provider origin was rejected".into(),
                    }),
                })
            };
            let events = Arc::new(StdMutex::new(Vec::new()));
            let subscriber = CapturedEvents(events.clone());
            let mut specs = Ok(vec![(3, shell_spec())]);
            let first_outcome = tracing::subscriber::with_default(subscriber, || {
                let first =
                    apply_relaunch_with(&mut specs, &cfg, &worktree, Some(3), true, false, refusal);
                let mut retry = Ok(vec![(3, shell_spec())]);
                let second =
                    apply_relaunch_with(&mut retry, &cfg, &worktree, Some(3), true, false, refusal);
                (first, second, retry)
            });
            assert_eq!(first_outcome.0, RelaunchOutcome::Refused);
            assert_eq!(first_outcome.1, RelaunchOutcome::Refused);
            assert_eq!(
                specs.as_ref().unwrap()[0].1.argv[2],
                "printf shell-fallback"
            );
            assert_eq!(
                first_outcome.2.as_ref().unwrap()[0].1.argv[2],
                "printf shell-fallback"
            );
            let events = events.lock().unwrap();
            assert_eq!(events.len(), 1, "same typed refusal must emit one WARN");
            assert!(events[0].contains("thegn::agent WARN"));
            assert!(events[0].contains("provider-ownership"));
            assert!(events[0].contains("trusted provider origin was rejected"));
            assert!(events[0].contains("shell fallback retained"));
            assert_eq!(remembered(&worktree).as_deref(), Some("codex"));

            let error = anyhow::Error::new(crate::agent::DevcontainerLaunchRefused {
                reason: "trusted provider origin was rejected".into(),
            });
            assert_eq!(refusal_kind(&error), RelaunchRefusalKind::ProviderOwnership);
            assert!(bounded_reason(&error).contains("trusted provider origin"));
            assert!(!admit_refusal(
                &worktree,
                "codex",
                RelaunchRefusalKind::ProviderOwnership
            ));
            assert!(admit_refusal(
                &worktree,
                "codex",
                RelaunchRefusalKind::Sandbox
            ));
        });
    }

    #[test]
    fn ordinary_launch_error_from_resolver_keeps_shell_and_has_a_generic_bucket() {
        with_state("ordinary-refusal", |_| {
            let mut cfg = cfg_with(&[("codex", false)]);
            // Codex has no command-scoped permission mechanism. This reaches
            // the real remembered-agent resolver and deterministically returns
            // its ordinary launch-policy error without running a command.
            cfg.agents[0].permissions = vec!["Read".into()];
            let worktree = worktree_path("ordinary-refusal");
            register(&worktree, "codex");

            let refusal = remembered_agent_relaunch(&cfg, &worktree, 3)
                .expect_err("the resolver must preserve the ordinary launch error");
            assert_eq!(
                refusal_kind(&refusal.source),
                RelaunchRefusalKind::ProviderLaunch
            );
            assert!(bounded_reason(&refusal.source).contains("permission policy hold"));

            let mut specs = Ok(vec![(3, shell_spec())]);
            let events = Arc::new(StdMutex::new(Vec::new()));
            let subscriber = CapturedEvents(events.clone());
            let outcome = tracing::subscriber::with_default(subscriber, || {
                apply_relaunch(&mut specs, &cfg, &worktree, Some(3), true, false)
            });
            assert_eq!(outcome, RelaunchOutcome::Refused);
            assert_eq!(
                specs.as_ref().unwrap()[0].1.argv[2],
                "printf shell-fallback"
            );
            let events = events.lock().unwrap();
            assert_eq!(events.len(), 1, "ordinary refusal must emit one WARN");
            assert!(events[0].contains("thegn::agent WARN"));
            assert!(events[0].contains("provider-launch"));
            assert!(events[0].contains("permission policy hold"));
            assert!(events[0].contains("shell fallback retained"));
            assert!(!admit_refusal(
                &worktree,
                "codex",
                RelaunchRefusalKind::ProviderLaunch
            ));
            assert!(admit_refusal(
                &worktree,
                "codex",
                RelaunchRefusalKind::Sandbox
            ));
        });

        let error = anyhow::anyhow!("provider unavailable offline");
        assert_eq!(refusal_kind(&error), RelaunchRefusalKind::ProviderLaunch);
        assert_eq!(bounded_reason(&error), "provider unavailable offline");
    }

    #[test]
    fn emitted_refusal_report_is_actionable_and_deduped() {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let subscriber = CapturedEvents(events.clone());
        let worktree = worktree_path("observable-report");
        let refusal = || RelaunchRefusal {
            agent: "codex".into(),
            source: anyhow::Error::new(crate::agent::DevcontainerLaunchRefused {
                reason: "trusted provider origin was rejected".into(),
            }),
        };

        tracing::subscriber::with_default(subscriber, || {
            report_relaunch_refusal(refusal(), &worktree);
            report_relaunch_refusal(refusal(), &worktree);
        });

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "same refusal must emit one report");
        assert!(events[0].contains("thegn::agent WARN"));
        assert!(events[0].contains("provider-ownership"));
        assert!(events[0].contains("trusted provider origin was rejected"));
        assert!(events[0].contains("shell fallback retained"));
    }

    #[test]
    fn concurrent_same_refusal_is_reported_once() {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let worktree = worktree_path("concurrent-report");

        std::thread::scope(|scope| {
            for _ in 0..8 {
                let events = Arc::clone(&events);
                let worktree = &worktree;
                scope.spawn(move || {
                    let subscriber = CapturedEvents(events);
                    tracing::subscriber::with_default(subscriber, || {
                        report_relaunch_refusal(
                            RelaunchRefusal {
                                agent: "codex".into(),
                                source: anyhow::Error::new(
                                    crate::agent::DevcontainerLaunchRefused {
                                        reason: "trusted provider origin was rejected".into(),
                                    },
                                ),
                            },
                            worktree.as_str(),
                        );
                    });
                });
            }
        });

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "concurrent reports must dedupe atomically");
    }

    #[test]
    fn refusal_detail_is_redacted_bounded_and_control_free() {
        let error = anyhow::anyhow!(
            "provider token=secret\n{} --api-key live-secret",
            "x".repeat(MAX_REFUSAL_REASON_CHARS * 2)
        );
        let reason = bounded_reason(&error);
        assert!(reason.chars().count() <= MAX_REFUSAL_REASON_CHARS);
        assert!(!reason.chars().any(char::is_control));
        assert!(reason.contains("token=***redacted***"));
        assert!(!reason.contains("live-secret"));
    }

    #[test]
    fn refusal_registry_evicts_oldest_entry_at_its_cap() {
        let mut registry = RefusalRegistry::default();
        for i in 0..MAX_REPORTED_REFUSALS {
            assert!(registry.admit((
                format!("wt-{i}"),
                "codex".into(),
                RelaunchRefusalKind::ProviderLaunch
            )));
        }
        assert_eq!(registry.reported.len(), MAX_REPORTED_REFUSALS);
        assert!(!registry.admit((
            "wt-0".into(),
            "codex".into(),
            RelaunchRefusalKind::ProviderLaunch
        )));
        assert!(registry.admit((
            "wt-new".into(),
            "codex".into(),
            RelaunchRefusalKind::ProviderLaunch
        )));
        assert_eq!(registry.reported.len(), MAX_REPORTED_REFUSALS);
        assert!(registry.admit((
            "wt-0".into(),
            "codex".into(),
            RelaunchRefusalKind::ProviderLaunch
        )));
    }
}
