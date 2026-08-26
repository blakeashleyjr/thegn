//! Publishes per-worktree "recent unsolicited agent-pane output" stamps for the
//! activity FSM's second busy signal (`thegn_core::activity`).
//!
//! The CPU-jiffies scan alone flips an agent's dot to `waiting` (unread) mid-turn:
//! an agent blocked on an API response uses ~0% CPU for far longer than the quiet
//! grace. But a *working* agent keeps redrawing its spinner — continuous PTY
//! output with no user keystrokes — while a finished (or permission-stuck) agent
//! emits nothing. The run loop calls [`publish`] just before each model
//! hydration; the hydration thread reads [`snapshot`] and feeds it to
//! `poll_and_save_inputs` as `output_hints`.
//!
//! What counts, per pane of a worktree that has a real (non-tool) agent:
//! - output is **unsolicited**: nothing typed into the pane for
//!   `[activity] unsolicited_gap_secs` before it (keystroke echo in a shell must not
//!   register). Host-generated protocol replies use `write_reply` and never
//!   stamp input.
//! - the pane is **established** (`[activity] spawn_grace_secs`): shell banners, prompt
//!   paint and a Stream reattach's server-side scrollback replay all land right
//!   after spawn. (Host-pane resurrect repaint bypasses `feed` entirely.)
//! - the pane is plausibly the agent's. Either its **spawn program** is neither
//!   an interactive shell nor a configured tool drawer (yazi/lazygit/…) — thegn
//!   launched it — or, for a shell pane, its **live foreground program** is a
//!   recognized agent CLI. That second test is what catches an agent started by
//!   hand: typing `claude` at a prompt leaves the spawn argv saying `zsh`, so
//!   such a worktree used to fall back to the CPU signal alone, where an agent
//!   waiting on a model response is indistinguishable from an idle one — the
//!   "dot goes red while the agent is still working" bug.
//!
//! Granularity note: any *other* qualifying pane in an agent-bearing worktree
//! also counts, since the DB `agent` field is per-worktree. That matches the CPU
//! signal, which already sums every process under the worktree.
//!
//! Pane→worktree attribution is learned from the live session on each publish
//! and retained (pane ids are never reused), so agents in backgrounded
//! workspaces keep reporting; entries are pruned once their pane is gone.
//!
//! The same pass also publishes [`snapshot_live_agents`] — the worktrees seen
//! running an agent *right now*. The activity FSM needs that as well as the
//! stamps: its "only an agent-bearing worktree may go red" rule reads the DB
//! `agent` column, which for a hand-started agent says `"shell"`, and would
//! otherwise swallow the finished alert for exactly the agents this module
//! works hardest to see.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

struct Registry {
    /// pane id → worktree path (learned from the session, pruned on pane close).
    pane_wt: HashMap<u32, String>,
    /// worktree path → unix secs of the last unsolicited agent-pane output.
    hints: BTreeMap<String, f64>,
    /// pane id → the agent CLI name running in it (the worktree's bound agent).
    /// This is the per-pane attribution the CPU/output signals lack: a pane
    /// qualifies when it is a non-shell/non-tool pane in an agent-bearing
    /// worktree. Lets a client report per-pane state, not just per-worktree.
    // Read by `snapshot_pane_agents`, which the per-pane `agent_states` control
    // endpoint (B‑3 exposure) will call once the compositor→daemon state-push
    // channel lands with the runtime split; staged here so the attribution is
    // computed and tested now.
    #[allow(dead_code)]
    pane_agent: HashMap<u32, String>,
    /// Worktrees observed running an agent *right now*, by live foreground
    /// probe rather than by the DB `agent` column.
    ///
    /// The DB says what thegn launched; this says what is actually there. They
    /// disagree for a hand-started agent — you type `claude` in a shell pane of
    /// a worktree whose bound agent is the `"shell"` placeholder. Without this,
    /// such a worktree is classified as agent-less, and the activity FSM's
    /// "red requires an agent" rule would suppress the very "your agent
    /// finished" alert the user wants.
    live_agent_wts: std::collections::BTreeSet<String>,
}

/// Process-global cell, mirroring the sibling `hydrate::glyph_cache` pattern:
/// written by the run loop ([`publish`]), read by the hydration thread
/// ([`snapshot`]) — no threading through the ~dozen hydration spawn sites.
fn cell() -> &'static Mutex<Registry> {
    static CELL: OnceLock<Mutex<Registry>> = OnceLock::new();
    CELL.get_or_init(|| {
        Mutex::new(Registry {
            pane_wt: HashMap::new(),
            hints: BTreeMap::new(),
            pane_agent: HashMap::new(),
            live_agent_wts: std::collections::BTreeSet::new(),
        })
    })
}

/// Whether a pane's spawn program can be the worktree's working agent for the
/// output signal. Interactive shells are out (typing echo, manually-started dev
/// servers); configured tool drawers (yazi/lazygit/…) are out. Wrappers we
/// can't see past (bwrap/podman/ssh/stream shims) stay in — the worktree-level
/// agent gate already vouches for the worktree.
fn counts_as_agent_pane(program: &str, is_tool: bool) -> bool {
    !is_tool && !crate::pane::is_interactive_shell(program)
}

/// Whether `program` is a recognized agent CLI: a configured `[[agents]]`
/// command, the worktree's own bound agent, or one of
/// `[activity] agent_programs`.
///
/// This is the *positive* test that lets a hand-started agent count. A pane
/// whose spawn argv is a shell fails [`counts_as_agent_pane`] no matter what is
/// running inside it, so `claude` typed at a prompt used to contribute nothing
/// and the worktree fell back to CPU alone — where an agent waiting on a model
/// response looks exactly like an idle one.
fn is_known_agent_program(
    program: &str,
    bound_agent: &str,
    cfg: &thegn_core::config::Config,
) -> bool {
    if program.is_empty() {
        return false;
    }
    program.eq_ignore_ascii_case(bound_agent)
        || cfg.activity.is_agent_program(program)
        || cfg
            .agents
            .iter()
            .any(|a| crate::pane::agent_program_name(&a.command, &a.name) == program)
}

/// The age of the pane's last *unsolicited* output, or `None` when there is no
/// output, the output is echo (input within the configured gap before it), or
/// the pane is still inside the spawn grace.
///
/// A `Duration`-shaped adapter over [`thegn_core::session_activity::unsolicited_age`],
/// which is where the rule itself lives — the pane daemon's per-session observer
/// applies the identical test, and a second copy here is exactly how the two
/// would drift. Moving it also made the `[activity] spawn_grace_secs` /
/// `unsolicited_gap_secs` knobs real: this used to hardcode them while the
/// config reference advertised them as tunable.
fn unsolicited_age(
    out_age: Option<Duration>,
    in_age: Option<Duration>,
    pane_age: Option<Duration>,
    cfg: &thegn_core::config_activity::ActivityConfig,
) -> Option<Duration> {
    thegn_core::session_activity::unsolicited_age(
        out_age.map(|d| d.as_secs_f64()),
        in_age.map(|d| d.as_secs_f64()),
        pane_age.map(|d| d.as_secs_f64()),
        cfg,
    )
    .map(Duration::from_secs_f64)
}

/// Refresh the pane→worktree registry from the live session and publish fresh
/// per-worktree output stamps. Run-loop side, O(panes).
///
/// Not quite free: each *shell* pane gets a live foreground probe
/// ([`crate::pane::PtyPane::foreground_program`]) — a handful of small `/proc`
/// reads, bounded by the shell-pane count, once per publish (~5s), and the same
/// call shape `capture_pane_cmds` already makes on the loop at persist time.
/// Panes thegn launched the agent in skip it entirely (their spawn argv already
/// answers the question), as do tool drawers and other named programs.
pub(crate) fn publish(
    session: &crate::session::Session,
    panes: &crate::panes::Panes,
    agent_by_wt: &BTreeMap<String, String>,
    cfg: &thegn_core::config::Config,
) {
    let Ok(mut reg) = cell().lock() else {
        return;
    };
    for (gi, _ti, tab) in session.iter_tabs() {
        let Some(path) = session.worktrees.get(gi).map(|g| g.path.as_str()) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        for id in tab.center.pane_ids() {
            reg.pane_wt.insert(id, path.to_string());
        }
    }
    reg.pane_wt.retain(|id, _| panes.table.contains_key(id));

    let unix_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut hints: BTreeMap<String, f64> = BTreeMap::new();
    let mut pane_agent: HashMap<u32, String> = HashMap::new();
    let mut live_agent_wts: std::collections::BTreeSet<String> = Default::default();
    for (id, wt) in &reg.pane_wt {
        let Some(pane) = panes.table.get(id) else {
            continue;
        };
        // The worktree's DB-bound agent, if it names a real one rather than the
        // `"shell"`/`"local"` placeholders a plain terminal worktree carries.
        let bound = agent_by_wt
            .get(wt)
            .map(String::as_str)
            .filter(|a| thegn_core::activity::is_real_agent(a))
            .unwrap_or_default();
        let program = pane.program();

        // Which agent, if any, is this pane running? Two routes:
        //
        // 1. thegn launched it — the worktree has a bound agent and the pane's
        //    spawn program is neither a shell nor a tool drawer. (Wrappers stay
        //    in: the worktree-level binding vouches for them.)
        // 2. the user started it by hand — the spawn argv says `zsh`, so only a
        //    live foreground probe can see the `claude` actually running there.
        //    This route does NOT require a bound agent, because the whole point
        //    is that the DB doesn't know about it.
        let agent = if !bound.is_empty()
            && counts_as_agent_pane(program, cfg.tool_command(program).is_some())
        {
            Some(bound.to_string())
        } else if crate::pane::is_interactive_shell(program) {
            // An idle prompt resolves to `None` — nothing running, nothing to
            // report — and a shell running `cargo` resolves to a non-agent.
            pane.foreground_program()
                .filter(|fg| is_known_agent_program(fg, bound, cfg))
        } else {
            None
        };
        let Some(agent) = agent else { continue };

        // Evidence for the FSM's agent gate that beats the DB column: this
        // worktree really is running an agent right now, whatever the row says.
        live_agent_wts.insert(wt.clone());
        // Per-pane attribution: this pane is running that agent.
        pane_agent.insert(*id, agent);
        let (out_at, in_at) = pane.output_stamps();
        let Some(age) = unsolicited_age(
            out_at.map(|t| t.elapsed()),
            in_at.map(|t| t.elapsed()),
            panes.pane_age(*id),
            &cfg.activity,
        ) else {
            continue;
        };
        let stamp = unix_now - age.as_secs_f64();
        // Multiple qualifying panes: the freshest one keeps the worktree busy.
        let e = hints.entry(wt.clone()).or_insert(stamp);
        *e = e.max(stamp);
    }
    reg.hints = hints;
    reg.pane_agent = pane_agent;
    reg.live_agent_wts = live_agent_wts;
}

/// The last published stamps (`worktree path → unix secs`), for the hydration
/// thread to pass into `activity::poll_and_save_inputs`.
pub(crate) fn snapshot() -> BTreeMap<String, f64> {
    cell()
        .lock()
        .map(|reg| reg.hints.clone())
        .unwrap_or_default()
}

/// Worktrees seen running an agent by live probe, for the hydration thread to
/// union into the FSM's agent gate. Without it, an agent you started by hand
/// lives in a worktree the DB calls a plain shell, so "red requires an agent"
/// would swallow its finished alert.
pub(crate) fn snapshot_live_agents() -> std::collections::BTreeSet<String> {
    cell()
        .lock()
        .map(|reg| reg.live_agent_wts.clone())
        .unwrap_or_default()
}

/// The current per-pane agent attribution (`pane id → agent CLI name`), for
/// reporting per-pane semantic state. Empty until the next [`publish`].
// Consumed by the forthcoming `agent_states` control endpoint (B‑3 exposure);
// see the `pane_agent` field note.
#[allow(dead_code)]
pub(crate) fn snapshot_pane_agents() -> HashMap<u32, String> {
    cell()
        .lock()
        .map(|reg| reg.pane_agent.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(f64) -> Duration = Duration::from_secs_f64;

    /// The stock `[activity]` thresholds the rule used to hardcode.
    fn acfg() -> thegn_core::config_activity::ActivityConfig {
        thegn_core::config_activity::ActivityConfig::default()
    }

    #[test]
    fn unsolicited_counts_without_input() {
        // Established pane, output, never any input → the output counts.
        assert_eq!(
            unsolicited_age(Some(S(2.0)), None, Some(S(60.0)), &acfg()),
            Some(S(2.0))
        );
    }

    #[test]
    fn echo_window_suppresses() {
        // Output 0.5s after input (input age 2.5, output age 2.0) → echo.
        assert_eq!(
            unsolicited_age(Some(S(2.0)), Some(S(2.5)), Some(S(60.0)), &acfg()),
            None
        );
        // Output 2s after input → unsolicited.
        assert_eq!(
            unsolicited_age(Some(S(2.0)), Some(S(4.0)), Some(S(60.0)), &acfg()),
            Some(S(2.0))
        );
        // Input AFTER the last output (user just typed, no response yet) → no
        // unsolicited output to report.
        assert_eq!(
            unsolicited_age(Some(S(5.0)), Some(S(1.0)), Some(S(60.0)), &acfg()),
            None
        );
    }

    #[test]
    fn spawn_grace_and_missing_output() {
        // Pane younger than the grace (banners/replay) → nothing.
        assert_eq!(
            unsolicited_age(Some(S(0.1)), None, Some(S(2.0)), &acfg()),
            None
        );
        // Unknown pane age → conservative nothing.
        assert_eq!(unsolicited_age(Some(S(0.1)), None, None, &acfg()), None);
        // No output at all → nothing.
        assert_eq!(unsolicited_age(None, None, Some(S(60.0)), &acfg()), None);
    }

    /// The positive test that makes a hand-started agent count. `bound` is the
    /// worktree's DB agent; `cfg` carries `[[agents]]` + `[activity]`.
    #[test]
    fn known_agent_program_matches_bound_config_and_defaults() {
        let cfg = thegn_core::config::Config::default();
        // The worktree's own bound agent, whatever it is called.
        assert!(is_known_agent_program("my-bot", "my-bot", &cfg));
        assert!(is_known_agent_program("MY-BOT", "my-bot", &cfg));
        // The `[activity] agent_programs` default list, independent of binding.
        assert!(is_known_agent_program("claude", "", &cfg));
        assert!(is_known_agent_program("codex", "", &cfg));
        // Everything else is not an agent — this is the whole point of making
        // the filter positive: `htop`/`watch`/a dev server must not qualify.
        assert!(!is_known_agent_program("htop", "", &cfg));
        assert!(!is_known_agent_program("watch", "", &cfg));
        assert!(!is_known_agent_program("cargo", "", &cfg));
        // An unresolved program name never qualifies — including against an
        // empty bound agent, which would otherwise compare equal.
        assert!(!is_known_agent_program("", "", &cfg));
        assert!(!is_known_agent_program("", "claude", &cfg));
    }

    #[test]
    fn known_agent_program_matches_a_configured_agent_command() {
        let cfg = thegn_core::config::Config {
            agents: vec![thegn_core::config::NamedCommand {
                name: "helper".into(),
                command: "/opt/bin/my-helper --acp".into(),
                hints: Vec::new(),
                provider: None,
                route_via_proxy: false,
            }],
            ..Default::default()
        };
        // Matched on the command's program stem, not the display name.
        assert!(is_known_agent_program("my-helper", "", &cfg));
        assert!(!is_known_agent_program("helper", "", &cfg));
    }

    #[test]
    fn pane_predicate_excludes_shells_and_tools() {
        assert!(!counts_as_agent_pane("zsh", false));
        assert!(!counts_as_agent_pane("bash", false));
        assert!(!counts_as_agent_pane("yazi", true));
        assert!(counts_as_agent_pane("claude", false));
        // Wrappers we can't see past stay in — the worktree agent gate vouches.
        assert!(counts_as_agent_pane("bwrap", false));
        assert!(counts_as_agent_pane("ssh", false));
    }

    #[test]
    fn publish_and_snapshot_roundtrip_via_registry() {
        // The full publish() path needs live panes; the registry/hints cell is
        // exercised by writing hints through the same lock and reading back.
        {
            let mut reg = cell().lock().unwrap();
            reg.hints = BTreeMap::from([("/wt/a".to_string(), 123.0)]);
        }
        assert_eq!(snapshot().get("/wt/a"), Some(&123.0));
        {
            let mut reg = cell().lock().unwrap();
            reg.hints.clear();
        }
        assert!(snapshot().is_empty());
    }

    #[test]
    fn live_agent_worktrees_roundtrip_via_registry() {
        {
            let mut reg = cell().lock().unwrap();
            reg.live_agent_wts = std::collections::BTreeSet::from(["/wt/hand".to_string()]);
        }
        assert!(snapshot_live_agents().contains("/wt/hand"));
        {
            let mut reg = cell().lock().unwrap();
            reg.live_agent_wts.clear();
        }
        assert!(snapshot_live_agents().is_empty());
    }

    #[test]
    fn pane_agent_attribution_roundtrips_via_registry() {
        {
            let mut reg = cell().lock().unwrap();
            reg.pane_agent = HashMap::from([(7u32, "claude".to_string())]);
        }
        assert_eq!(snapshot_pane_agents().get(&7), Some(&"claude".to_string()));
        {
            let mut reg = cell().lock().unwrap();
            reg.pane_agent.clear();
        }
        assert!(snapshot_pane_agents().is_empty());
    }
}
