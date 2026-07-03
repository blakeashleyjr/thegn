//! Persist-time capture of live pane state into the session model.
//!
//! Just before [`crate::session::Session::persist`] writes the layout, these
//! helpers copy each live leaf pane's out-of-band state — working directory,
//! foreground command, provider exec session, and a bounded scrollback tail —
//! into its `Tab` so the next resurrect can respawn faithfully. Only
//! materialized (live) leaves are touched; tabs that were never opened keep the
//! hints they resurrected with. All four are cheap `/proc` reads / in-memory
//! copies run only at persist time, never on the render loop.

use crate::panes::Panes;
use crate::session::Session;

/// Capture every live pane's cwd, foreground command, provider session, and
/// scrollback tail into the session model, ready for [`Session::persist`].
pub(crate) fn capture_pane_state(session: &mut Session, panes: &Panes) {
    capture_pane_cwds(session, panes);
    capture_pane_cmds(session, panes);
    capture_pane_sessions(session, panes);
    // The scrollback cap is a `[session]` config knob; loading it here keeps
    // `persist_session_layout`'s signature (and its ~18 call sites) untouched.
    let max_lines = superzej_core::config::Config::try_load_layered(
        &superzej_core::config::ProcessEnv,
        &[],
        None,
    )
    .map(|c| c.session.scrollback_lines as usize)
    .unwrap_or_else(|_| superzej_core::config::SessionConfig::default().scrollback_lines as usize);
    capture_pane_scrollback(session, panes, max_lines);
}

/// Capture each live pane's current working directory into its tab's
/// `pane_cwds` so the next resurrect respawns panes where they were. Cheap: a
/// `/proc/<pid>/cwd` readlink per live pane.
fn capture_pane_cwds(session: &mut Session, panes: &Panes) {
    for g in &mut session.worktrees {
        for tab in &mut g.tabs {
            for id in tab.center.pane_ids() {
                if let Some(p) = panes.table.get(&id)
                    && let Some(cwd) = p.cwd()
                {
                    tab.pane_cwds.insert(id, cwd.to_string_lossy().into_owned());
                }
            }
        }
    }
}

/// Capture each live pane's foreground command into its tab's `pane_cmds` so a
/// resurrected or crashed pane can offer to relaunch it. An idle shell prompt
/// clears any stale entry, so the hint always reflects what was last running.
fn capture_pane_cmds(session: &mut Session, panes: &Panes) {
    for g in &mut session.worktrees {
        for tab in &mut g.tabs {
            for id in tab.center.pane_ids() {
                let Some(p) = panes.table.get(&id) else {
                    continue;
                };
                match p.foreground_command() {
                    Some(cmd) => {
                        tab.pane_cmds.insert(id, cmd);
                    }
                    None => {
                        tab.pane_cmds.remove(&id);
                    }
                }
            }
        }
    }
}

/// Capture each live `Stream` pane's provider session into its tab's
/// `pane_sessions` so a restart reattaches the live remote session (replaying
/// scrollback) instead of opening a fresh shell. A pane that isn't a native-exec
/// stream — or whose session id hasn't been announced — clears any stale entry.
fn capture_pane_sessions(session: &mut Session, panes: &Panes) {
    for g in &mut session.worktrees {
        for tab in &mut g.tabs {
            for id in tab.center.pane_ids() {
                let Some(p) = panes.table.get(&id) else {
                    continue;
                };
                match p.provider_session() {
                    Some(ps) => {
                        tab.pane_sessions.insert(id, ps);
                    }
                    None => {
                        tab.pane_sessions.remove(&id);
                    }
                }
            }
        }
    }
}

/// Capture a bounded plain-text scrollback tail of each live **host PTY** pane
/// into its tab's `pane_scrollback`, so a resurrected pane repaints its recent
/// history instead of a blank screen. Native-exec streams replay scrollback
/// server-side on reattach, so their tail is not stored (any stale entry is
/// cleared). `max_lines == 0` disables capture (blank restore, as before).
fn capture_pane_scrollback(session: &mut Session, panes: &Panes, max_lines: usize) {
    for g in &mut session.worktrees {
        for tab in &mut g.tabs {
            for id in tab.center.pane_ids() {
                let Some(p) = panes.table.get(&id) else {
                    continue;
                };
                // A stream pane replays its own scrollback on reattach — don't
                // double-store the host-side mirror.
                let tail = if max_lines == 0 || p.provider_session().is_some() {
                    String::new()
                } else {
                    p.history_tail(max_lines)
                };
                if tail.is_empty() {
                    tab.pane_scrollback.remove(&id);
                } else {
                    tab.pane_scrollback.insert(id, tail);
                }
            }
        }
    }
}
