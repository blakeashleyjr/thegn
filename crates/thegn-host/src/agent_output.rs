//! Publishes per-worktree "recent unsolicited agent-pane output" stamps for the
//! activity FSM's second busy signal (`thegn_core::activity`).
//!
//! The CPU-jiffies scan alone flips an agent's dot to `waiting` (unread) mid-turn:
//! an agent blocked on an API response uses ~0% CPU for far longer than the quiet
//! grace. But a *working* agent keeps redrawing its spinner — continuous PTY
//! output with no user keystrokes — while a finished (or permission-stuck) agent
//! emits nothing. The run loop calls [`publish`] just before each model
//! hydration; the hydration thread reads [`snapshot`] and feeds it to
//! `poll_and_save_with` as `output_hints`.
//!
//! What counts, per pane of a worktree that has a real (non-tool) agent:
//! - output is **unsolicited**: nothing typed into the pane for
//!   [`UNSOLICITED_GAP_SECS`] before it (keystroke echo in a shell must not
//!   register). Host-generated protocol replies use `write_reply` and never
//!   stamp input.
//! - the pane is **established** ([`SPAWN_GRACE_SECS`]): shell banners, prompt
//!   paint and a Stream reattach's server-side scrollback replay all land right
//!   after spawn. (Host-pane resurrect repaint bypasses `feed` entirely.)
//! - the pane's spawn program is neither an interactive shell nor a configured
//!   tool drawer (yazi/lazygit/…). Granularity note: any other pane in an
//!   agent-bearing worktree counts — per-pane agent attribution doesn't exist
//!   (the DB `agent` field is per-worktree), and this matches the CPU signal,
//!   which already sums every process under the worktree. An agent launched by
//!   hand inside a shell pane is missed (spawn argv says the shell) and simply
//!   falls back to today's CPU-only behavior.
//!
//! Pane→worktree attribution is learned from the live session on each publish
//! and retained (pane ids are never reused), so agents in backgrounded
//! workspaces keep reporting; entries are pruned once their pane is gone.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Output within this gap after user input to the same pane is "solicited"
/// (keystroke echo / an immediate command response), not agent work.
const UNSOLICITED_GAP_SECS: f64 = 1.0;
/// Ignore output from panes younger than this: spawn-time banners, prompt
/// paint, and reattach scrollback replay are not live agent activity.
const SPAWN_GRACE_SECS: f64 = 5.0;

struct Registry {
    /// pane id → worktree path (learned from the session, pruned on pane close).
    pane_wt: HashMap<u32, String>,
    /// worktree path → unix secs of the last unsolicited agent-pane output.
    hints: BTreeMap<String, f64>,
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

/// The age of the pane's last *unsolicited* output, or `None` when there is no
/// output, the output is echo (input within [`UNSOLICITED_GAP_SECS`] before
/// it), or the pane is still inside [`SPAWN_GRACE_SECS`]. Pure — ages instead
/// of `Instant`s so it is unit-testable.
fn unsolicited_age(
    out_age: Option<Duration>,
    in_age: Option<Duration>,
    pane_age: Option<Duration>,
) -> Option<Duration> {
    let out = out_age?;
    if pane_age.is_none_or(|a| a.as_secs_f64() < SPAWN_GRACE_SECS) {
        return None;
    }
    // Timestamp form "output later than input + gap" in age space: the input
    // must be older than the output by more than the gap.
    if in_age.is_some_and(|inp| inp.as_secs_f64() - out.as_secs_f64() <= UNSOLICITED_GAP_SECS) {
        return None;
    }
    Some(out)
}

/// Refresh the pane→worktree registry from the live session and publish fresh
/// per-worktree output stamps. Run-loop side: O(panes), no I/O, no blocking.
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
    for (id, wt) in &reg.pane_wt {
        if !agent_by_wt.contains_key(wt) {
            continue;
        }
        let Some(pane) = panes.table.get(id) else {
            continue;
        };
        let program = pane.program();
        if !counts_as_agent_pane(program, cfg.tool_command(program).is_some()) {
            continue;
        }
        let (out_at, in_at) = pane.output_stamps();
        let Some(age) = unsolicited_age(
            out_at.map(|t| t.elapsed()),
            in_at.map(|t| t.elapsed()),
            panes.pane_age(*id),
        ) else {
            continue;
        };
        let stamp = unix_now - age.as_secs_f64();
        // Multiple qualifying panes: the freshest one keeps the worktree busy.
        let e = hints.entry(wt.clone()).or_insert(stamp);
        *e = e.max(stamp);
    }
    reg.hints = hints;
}

/// The last published stamps (`worktree path → unix secs`), for the hydration
/// thread to pass into `activity::poll_and_save_with`.
pub(crate) fn snapshot() -> BTreeMap<String, f64> {
    cell()
        .lock()
        .map(|reg| reg.hints.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(f64) -> Duration = Duration::from_secs_f64;

    #[test]
    fn unsolicited_counts_without_input() {
        // Established pane, output, never any input → the output counts.
        assert_eq!(
            unsolicited_age(Some(S(2.0)), None, Some(S(60.0))),
            Some(S(2.0))
        );
    }

    #[test]
    fn echo_window_suppresses() {
        // Output 0.5s after input (input age 2.5, output age 2.0) → echo.
        assert_eq!(
            unsolicited_age(Some(S(2.0)), Some(S(2.5)), Some(S(60.0))),
            None
        );
        // Output 2s after input → unsolicited.
        assert_eq!(
            unsolicited_age(Some(S(2.0)), Some(S(4.0)), Some(S(60.0))),
            Some(S(2.0))
        );
        // Input AFTER the last output (user just typed, no response yet) → no
        // unsolicited output to report.
        assert_eq!(
            unsolicited_age(Some(S(5.0)), Some(S(1.0)), Some(S(60.0))),
            None
        );
    }

    #[test]
    fn spawn_grace_and_missing_output() {
        // Pane younger than the grace (banners/replay) → nothing.
        assert_eq!(unsolicited_age(Some(S(0.1)), None, Some(S(2.0))), None);
        // Unknown pane age → conservative nothing.
        assert_eq!(unsolicited_age(Some(S(0.1)), None, None), None);
        // No output at all → nothing.
        assert_eq!(unsolicited_age(None, None, Some(S(60.0))), None);
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
}
