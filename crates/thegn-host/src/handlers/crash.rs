//! Pane fast-crash messaging + the sole-pane respawn decision, split out of the
//! ratchet-capped `run.rs`/`pty_drain.rs`.
//!
//! When a sole shell keeps crashing on startup the loop stops respawning it. A
//! sandbox/exec failure (e.g. a broken `--userns keep-id` podman exec) writes its
//! real error to the pane before dying; [`keeps_crashing_status`] surfaces that
//! captured tail so the failure is legible instead of a pane that just vanished.
//!
//! A sole pane's crash respawn no longer spawns synchronously on the event loop
//! (which re-resolved the sandbox — DB open + container ensure — inline):
//! [`respawn_action`] decides give-up vs leave-the-dead-leaf, and
//! [`prep_leaf_for_respawn`] does the pure `session::Tab` bookkeeping so the
//! existing off-thread materialize pipeline (`maybe_materialize` → spec channel
//! → `materialize_with_specs`) performs the actual respawn.

/// What the exit handler should do about a sole worktree pane that died.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RespawnAction {
    /// Crashing on every startup — stop respawning (dormant + status), so a
    /// broken sandbox isn't a silent respawn loop.
    GiveUp,
    /// Leave the dead leaf in `tab.center`: it becomes a "missing leaf" the
    /// off-thread materialize pipeline respawns (sandbox resolution on a
    /// blocking task, never on the loop). `keep_remembered_cmd` records whether
    /// the pane's last foreground command should survive for relaunch arming
    /// (crashes keep it; a clean exit lands at a plain prompt).
    LeaveForMaterialize { keep_remembered_cmd: bool },
}

/// The sole-pane respawn decision, pure for unit testing. `crashes` is the
/// consecutive fast-crash count for this (group, tab); `failed` is the exit's
/// failure classification (non-zero code, or the fast-crash heuristic).
pub(crate) fn respawn_action(crashes: u32, failed: bool) -> RespawnAction {
    if crashes >= 3 {
        RespawnAction::GiveUp
    } else {
        RespawnAction::LeaveForMaterialize {
            keep_remembered_cmd: failed,
        }
    }
}

/// Pure `session::Tab` bookkeeping for a sole worktree pane whose process died
/// and whose leaf is being left in the tree for the off-thread materialize
/// pipeline. Runs for EVERY sole exit (active tab or not) so switch-back
/// materialize sees the same state the active-tab respawn does:
///
/// - the dead pane's daemon/provider session is forgotten — its process is
///   gone, so a warm reattach could only degrade via `SessionFallback` (an
///   extra relay round-trip) — UNLESS this was a transport-loss exit
///   (`transport_loss`, i.e. `PaneEvent::Exit(id, None)` minted after the
///   reconnect ladder exhausted), where the daemon/provider session and its
///   child may still be ALIVE, merely unreachable. There we KEEP the record so
///   switch-back materialize's warm-reattach ladder can recover the live
///   session (or cleanly degrade via `SessionFallback` if it is truly gone);
///   dropping it would orphan a still-running daemon session while a duplicate
///   fresh session spawns beside it — exactly the leak the daemon-disable
///   claim fix targets;
/// - on a clean exit the remembered foreground command is dropped so
///   materialize won't arm a stale relaunch (a failed exit keeps it — that is
///   exactly what `materialize_with_specs` reads to arm `set_pending_relaunch`)
///   — UNLESS `keep_cmd`: an attached agent's clean exit keeps the command so
///   the respawned shell still arms the Enter-to-relaunch overlay that
///   [`agent_exit_status`] promises;
/// - the scrollback snapshot is refreshed with the tail captured just before
///   the pane left the table, so the respawned pane repaints what the user
///   actually last saw instead of the last-persist snapshot (an empty tail —
///   e.g. an instant crash with no output — keeps the richer persisted entry).
///
/// Deliberately does NOT touch `tab.center`: the dead id staying in the tree is
/// what makes it a missing leaf for `panes.missing_leaves`.
pub(crate) fn prep_leaf_for_respawn(
    tab: &mut crate::session::Tab,
    id: u32,
    failed: bool,
    transport_loss: bool,
    keep_cmd: bool,
    scrollback_tail: Option<String>,
) {
    // Transport-loss (Exit code `None`): the process may still be alive but
    // unreachable — keep the session record for warm reattach. A real process
    // death (Exit code `Some(_)`) forgets it.
    if !transport_loss {
        tab.pane_sessions.remove(&id);
    }
    if !failed && !keep_cmd {
        tab.pane_cmds.remove(&id);
    }
    if let Some(tail) = scrollback_tail.filter(|t| !t.is_empty()) {
        tab.pane_scrollback.insert(id, tail);
    }
}

/// The last non-blank line of a crashed pane's output tail, trimmed and
/// length-capped — the concrete reason to show the user. `None` when the pane
/// produced no usable output (fall back to the generic hint). Input is already
/// ANSI-stripped by the pane history ring.
pub(crate) fn crash_reason(tail: &str) -> Option<String> {
    tail.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().chars().take(200).collect())
}

/// Status shown when a sole shell keeps crashing on startup: names the real error
/// when one was captured, else the generic backend/shell hint.
pub(crate) fn keeps_crashing_status(tail: &str) -> String {
    match crash_reason(tail) {
        Some(r) => {
            format!("Shell keeps crashing on startup — not respawning. Last error: {r}")
        }
        None => "Shell keeps crashing on startup — not respawning. \
                 Check your sandbox backend and shell config, \
                 then switch worktrees to retry."
            .to_string(),
    }
}

/// Status for a daemon-backed agent pane's exit (e.g. an attached worktree
/// agent finishing its run): names the program + exit code and — when a
/// relaunch is actually offerable — the choice the respawned shell's overlay
/// honors: Enter retypes the remembered command, Esc dismisses it to a plain
/// shell. An unreapable exit renders `?`. The overlay arms only when a
/// foreground command was captured for the leaf (`pane_cmds`, persist-time
/// capture, host backend implied), so callers pass `relaunch = false` rather
/// than promise keys that do nothing — the statusbar never lies. Plain
/// statusbar text (the `— … · …` hint shape the other lines use); no glyph
/// icons, so nothing here needs the caps chokepoints.
pub(crate) fn agent_exit_status(program: &str, code: Option<i32>, relaunch: bool) -> String {
    let code = code.map_or_else(|| "?".to_string(), |c| c.to_string());
    let line = format!("agent {program} exited (code {code})");
    if relaunch {
        format!("{line} — Enter: relaunch · Esc: shell")
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_is_last_non_blank_line() {
        let tail = "starting\nError: crun: readlink: No such file or directory\n\n";
        assert_eq!(
            crash_reason(tail).as_deref(),
            Some("Error: crun: readlink: No such file or directory")
        );
    }

    #[test]
    fn reason_none_for_blank_tail() {
        assert_eq!(crash_reason("   \n\n"), None);
        assert_eq!(crash_reason(""), None);
    }

    #[test]
    fn reason_is_length_capped() {
        let long = "x".repeat(500);
        assert_eq!(crash_reason(&long).unwrap().chars().count(), 200);
    }

    #[test]
    fn status_names_error_when_present() {
        let s = keeps_crashing_status("boom: exec probe failed: crun error");
        assert!(
            s.contains("Last error: boom: exec probe failed: crun error"),
            "{s}"
        );
    }

    #[test]
    fn status_falls_back_to_generic_hint() {
        let s = keeps_crashing_status("");
        assert!(s.contains("Check your sandbox backend"), "{s}");
        assert!(!s.contains("Last error:"), "{s}");
    }

    #[test]
    fn respawn_action_gives_up_at_three_fast_crashes() {
        assert_eq!(respawn_action(3, true), RespawnAction::GiveUp);
        assert_eq!(respawn_action(7, false), RespawnAction::GiveUp);
    }

    #[test]
    fn respawn_action_keeps_remembered_cmd_only_on_failure() {
        // A failed exit leaves the leaf AND keeps the last foreground command
        // for relaunch arming; a clean exit leaves the leaf but drops it.
        assert_eq!(
            respawn_action(1, true),
            RespawnAction::LeaveForMaterialize {
                keep_remembered_cmd: true
            }
        );
        assert_eq!(
            respawn_action(0, false),
            RespawnAction::LeaveForMaterialize {
                keep_remembered_cmd: false
            }
        );
    }

    fn tab_with_dead_pane(id: u32) -> crate::session::Tab {
        let mut tab = crate::session::Tab::new("1");
        tab.center = crate::center::CenterTree::Leaf(id);
        tab.focused_pane = id;
        tab.pane_sessions.insert(
            id,
            crate::session::ProviderSession {
                provider: "daemon".into(),
                id: "sb1".into(),
                session: "sess-1".into(),
            },
        );
        tab.pane_cmds.insert(
            id,
            crate::session::PaneCmd {
                argv: vec!["nvim".into(), "src/main.rs".into()],
                cwd: None,
            },
        );
        tab.pane_scrollback.insert(id, "persisted tail".into());
        tab
    }

    #[test]
    fn prep_leaf_for_respawn_failed_exit_keeps_cmd_drops_session() {
        let mut tab = tab_with_dead_pane(7);
        prep_leaf_for_respawn(&mut tab, 7, true, false, false, Some("live tail".into()));
        // The leaf stays in the tree — that's what makes it a missing leaf for
        // the off-thread materialize pipeline.
        assert_eq!(tab.center.pane_ids(), vec![7]);
        // Dead process ⇒ no daemon/provider reattach attempt on switch-back.
        assert!(!tab.pane_sessions.contains_key(&7));
        // Failed exit keeps the remembered command for relaunch arming.
        assert!(tab.pane_cmds.contains_key(&7));
        // Scrollback refreshed with what the user actually last saw.
        assert_eq!(
            tab.pane_scrollback.get(&7).map(String::as_str),
            Some("live tail")
        );
    }

    #[test]
    fn prep_leaf_for_respawn_clean_exit_drops_stale_relaunch() {
        let mut tab = tab_with_dead_pane(7);
        prep_leaf_for_respawn(&mut tab, 7, false, false, false, None);
        assert_eq!(tab.center.pane_ids(), vec![7], "leaf stays in the tree");
        assert!(!tab.pane_sessions.contains_key(&7));
        assert!(
            !tab.pane_cmds.contains_key(&7),
            "clean exit must not arm a stale relaunch"
        );
        // No/empty captured tail keeps the richer persisted snapshot.
        assert_eq!(
            tab.pane_scrollback.get(&7).map(String::as_str),
            Some("persisted tail")
        );
    }

    #[test]
    fn prep_leaf_for_respawn_keep_cmd_arms_relaunch_on_clean_agent_exit() {
        // An attached agent's CLEAN exit still keeps the remembered command:
        // the respawned shell then arms the Enter-to-relaunch overlay the
        // `agent_exit_status` line promises (a plain shell's clean exit keeps
        // dropping it — see `..._clean_exit_drops_stale_relaunch`).
        let mut tab = tab_with_dead_pane(7);
        prep_leaf_for_respawn(&mut tab, 7, false, false, true, None);
        assert_eq!(tab.center.pane_ids(), vec![7], "leaf stays in the tree");
        assert!(!tab.pane_sessions.contains_key(&7));
        assert!(
            tab.pane_cmds.contains_key(&7),
            "keep_cmd must survive a clean agent exit for the relaunch overlay"
        );
    }

    #[test]
    fn agent_exit_status_renders_both_code_arms_and_none() {
        // Relaunch offerable (a foreground command was captured for the leaf):
        assert_eq!(
            agent_exit_status("claude", Some(0), true),
            "agent claude exited (code 0) — Enter: relaunch · Esc: shell"
        );
        assert_eq!(
            agent_exit_status("pi", Some(1), true),
            "agent pi exited (code 1) — Enter: relaunch · Esc: shell"
        );
        assert_eq!(
            agent_exit_status("claude", None, true),
            "agent claude exited (code ?) — Enter: relaunch · Esc: shell"
        );
        // No captured command (no persist ran while the agent worked): the
        // bare line only — Enter/Esc would do nothing, so they are not promised.
        assert_eq!(
            agent_exit_status("claude", Some(0), false),
            "agent claude exited (code 0)"
        );
    }

    #[test]
    fn prep_leaf_for_respawn_transport_loss_keeps_session_for_reattach() {
        // Exit code `None` (relay reconnect ladder exhausted): the daemon/
        // provider session may still be alive but unreachable — the record MUST
        // survive so switch-back materialize warm-reattaches instead of
        // orphaning a live session and spawning a duplicate beside it.
        let mut tab = tab_with_dead_pane(7);
        prep_leaf_for_respawn(&mut tab, 7, true, true, false, Some("live tail".into()));
        assert!(
            tab.pane_sessions.contains_key(&7),
            "transport-loss exit must keep the session record for warm reattach"
        );
    }

    #[test]
    fn prep_leaf_for_respawn_empty_tail_keeps_persisted_snapshot() {
        let mut tab = tab_with_dead_pane(9);
        prep_leaf_for_respawn(&mut tab, 9, true, false, false, Some(String::new()));
        assert_eq!(
            tab.pane_scrollback.get(&9).map(String::as_str),
            Some("persisted tail"),
            "an instant crash with no output must not blank the snapshot"
        );
    }
}
