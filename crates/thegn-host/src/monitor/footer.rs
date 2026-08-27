//! The monitor's footer row: key hints, or whatever has taken the row over.
//!
//! Lifted out of `monitor.rs` (already a god-file) and given an explicit input
//! struct rather than `&MonitorOverlay`, so the builder is testable without
//! constructing an overlay and the hint set is a pure function of the tab.
//!
//! # Only the keys the tab has
//!
//! The hints used to be one generic run shown on every non-Containers,
//! non-Pipeline tab, so Processes — which emits only headings and a table —
//! advertised `[ ]` window, `g` style and `s` scale, four keys with no graph to
//! act on. [`super::MonitorTab::has_graphs`] gates those three; everything else
//! is per-tab. `spc` pause is on **every** tab, the board included: `Space`
//! freezes the board and stops its roster sample, and a footer that hides that
//! is how a supervisor ends up staring at a stale board.

use super::{Confirm, MonitorPrefs, MonitorTab};
use crate::chrome::S;
use crate::seg::{Line, Seg, Tok, seg};

/// Everything the footer reads. Borrowed, so building it costs no allocation
/// beyond the hint text itself.
pub(super) struct FooterInput<'a> {
    pub tab: MonitorTab,
    pub prefs: &'a MonitorPrefs,
    /// A pending destructive confirmation — owns the whole row while set.
    pub confirm: Option<&'a Confirm>,
    pub filtering: bool,
    pub filter: &'a str,
    /// A one-keystroke notice (a container action's outcome, a failed jump).
    pub notice: Option<&'a str>,
    /// A transient status note; takes the right slot over the close hint.
    pub status: Option<&'a str>,
    pub paused: bool,
    /// The selected Containers row is one of thegn's own (foreign rows are
    /// read-only, so they get a note instead of an action legend).
    pub container_ours: bool,
    /// Disk-tab worktree row count: no rows, nothing for `x` to clean.
    pub disk_rows: usize,
}

/// Ghost text, the footer's default voice.
fn ghost(text: impl Into<String>) -> Seg {
    seg(Tok::Slot(S::Ghost), text)
}

/// One `key + label` hint.
fn hint(k: &str, label: impl Into<String>) -> Vec<Seg> {
    vec![Seg::key(k), ghost(label)]
}

/// Build the footer row.
pub(super) fn line(input: FooterInput<'_>) -> Line {
    // A pending confirmation owns the footer: it names exactly what will
    // happen, so a pane-owned build is recognizably thegn's own.
    if let Some(c) = input.confirm {
        let msg = match c {
            Confirm::Signal { label, stage, .. } => {
                let verb = match stage {
                    crate::platform::ProcSignal::Terminate => "terminate",
                    crate::platform::ProcSignal::Kill => "KILL (no cleanup)",
                };
                format!("{verb} {label}?")
            }
            Confirm::Clean { label, .. } => format!("clean target/ in {label}?"),
        };
        return Line::split(
            vec![seg(Tok::Slot(S::Accent), msg).bold()],
            vec![Seg::key("y"), ghost(" yes  "), Seg::key("n"), ghost(" no")],
        );
    }
    // Filter input: echo the query with a cursor.
    if input.filtering {
        return Line::split(
            vec![
                Seg::key("/"),
                ghost(" filter "),
                seg(Tok::Slot(S::Accent), format!("{}\u{2502}", input.filter)),
            ],
            vec![ghost("esc clear · enter apply")],
        );
    }
    // A pending container confirm / action outcome takes over the footer
    // while set.
    if let Some(notice) = input.notice {
        return Line::split(
            vec![seg(Tok::Slot(S::Accent), notice.to_string())],
            vec![ghost("q close")],
        );
    }

    // --- Hints, in reading order: tabs, the graph toggles the tab actually
    // --- has, pause, the tab's own actions, help.
    let mut hints: Vec<Vec<Seg>> = vec![hint("tab", " tabs")];
    if input.tab.has_graphs() {
        let p = input.prefs.tab(input.tab);
        hints.push(hint("[ ]", format!(" {}", p.window.label())));
        hints.push(hint("g", format!(" {}", p.style.label())));
        hints.push(hint("s", format!(" {}", p.scale.label())));
    }
    hints.push(hint("spc", if input.paused { " resume" } else { " pause" }));
    match input.tab {
        MonitorTab::Procs => {
            hints.push(hint(
                "c/m/n",
                format!(
                    " sort {}{}",
                    input.prefs.proc_sort.label(),
                    if input.prefs.proc_desc { "↓" } else { "↑" }
                ),
            ));
            hints.push(hint("/", " find"));
            hints.push(hint(
                "t",
                if input.prefs.proc_tree {
                    " flat"
                } else {
                    " tree"
                },
            ));
            hints.push(hint("x", " signal"));
        }
        MonitorTab::Disk if input.disk_rows > 0 => hints.push(hint("x", " clean")),
        MonitorTab::Containers if input.container_ours => {
            hints.push(hint("↵", " shell"));
            hints.push(hint("o", " logs"));
            hints.push(hint("t", " stop"));
            hints.push(hint("r", " restart"));
            hints.push(hint("x", " remove"));
        }
        MonitorTab::Containers => hints.push(vec![ghost("foreign container — read-only")]),
        // The board is a read-only table: one action.
        MonitorTab::Pipeline => hints.push(hint("↵", " go to worktree")),
        _ => {}
    }
    hints.push(hint("?", " help"));

    let mut left: Vec<Seg> = Vec::new();
    for (i, h) in hints.into_iter().enumerate() {
        if i > 0 {
            left.push(ghost("  "));
        }
        left.extend(h);
    }
    // A transient status note (signal outcome, filter echo) takes the right
    // slot over the close hint — it is the thing the user just asked for.
    let right = match input.status {
        Some(s) => seg(Tok::Slot(S::Accent), s.to_string()),
        None => ghost("q close"),
    };
    Line::split(left, vec![right])
}
