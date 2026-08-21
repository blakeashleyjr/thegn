//! The unified Shift+Alt+↑/↓ navigation ring, extracted from the god-file
//! `run.rs` and re-exported from it so call sites read unchanged.
//!
//! Shift+Alt+↑/↓ (`prev-workspace` / `next-workspace`) walks ONE ring made of
//! every visible sidebar workspace row followed by every terminal host, in
//! display order — the two sidebar sections read as one, and stepping wraps
//! across their boundary.
//!
//! Each stop carries whether its group is **collapsed**, and by default
//! ([`thegn_core::config::UiConfig::sidebar_nav_skips_collapsed`])
//! [`ring_step`] walks past collapsed stops: a folded group is one the user is
//! not working in, so navigation neither lands on it nor un-collapses it. The
//! whole thing is pure over the row slice + DB terminal list, so it is
//! unit-testable straight from `build_rows` output.

/// A stop in the unified Shift+Alt+↑/↓ ring: every visible workspace (repo)
/// followed by every terminal host, in sidebar display order. Stepping wraps
/// across the workspaces↔terminals boundary so the two sidebar sections read as
/// one ring. Unlike `sidebar_workspace_order` this KEEPS live-fallback
/// workspaces (they carry a real slug even without a DB `repo_path`), so the
/// current position is always locatable and the motion never silently no-ops.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RingStop {
    /// A workspace header. `repo_path` is `None` for a live fallback (the
    /// currently-resident workspace with no DB row yet), which is never a
    /// `switch_workspace` target — landing on it just leaves the terminals
    /// region for a worktree.
    Workspace {
        slug: String,
        repo_path: Option<String>,
        /// Whether the workspace's subtree is folded in the sidebar.
        collapsed: bool,
    },
    /// A terminal host (its collapse key), e.g. `local` / `prod`.
    TerminalHost {
        key: String,
        /// Whether the host's terminals are folded in the sidebar.
        collapsed: bool,
    },
}

impl RingStop {
    /// Whether this stop's group is folded in the sidebar — the thing
    /// [`ring_step`] skips by default.
    pub(crate) fn collapsed(&self) -> bool {
        match self {
            RingStop::Workspace { collapsed, .. } | RingStop::TerminalHost { collapsed, .. } => {
                *collapsed
            }
        }
    }
}

/// Build the unified ring in **visible sidebar order**: workspaces then terminal
/// hosts. Pure over the row slice + DB terminal list so it's unit-testable
/// straight from `build_rows` output.
///
/// Host ordering still comes from `terminal_hosts_ordered` (the shared
/// grouping), with each host's collapse state read off its `TerminalHost` row —
/// defaulting to expanded when no such row exists (e.g. the TERMINALS section
/// is hidden), which is the same "don't skip what we can't see folded" fallback
/// the workspace arm gets for free.
pub(crate) fn unified_ring(
    rows: &[crate::sidebar::SidebarRow],
    db_terminals: &[thegn_core::models::TerminalRow],
) -> Vec<RingStop> {
    let mut ring: Vec<RingStop> = rows
        .iter()
        .filter(|r| r.visible && r.kind == crate::sidebar::RowKind::Workspace)
        .map(|r| RingStop::Workspace {
            slug: r.workspace_slug.clone(),
            repo_path: r.worktree_path.clone(),
            collapsed: r.collapsed,
        })
        .collect();
    for (key, ..) in crate::sidebar::terminal_hosts_ordered(db_terminals) {
        let slug = format!("terminals/host:{key}");
        let collapsed = rows
            .iter()
            .find(|r| r.kind == crate::sidebar::RowKind::TerminalHost && r.workspace_slug == slug)
            .is_some_and(|r| r.collapsed);
        ring.push(RingStop::TerminalHost { key, collapsed });
    }
    ring
}

/// The current index in [`unified_ring`], resolved by terminal host key when the
/// active group is a terminal, else by workspace slug. Matching by slug (not by
/// raw repo-path equality against `session.id`) makes the lookup robust to
/// path-form differences and live fallbacks. `None` only when the active thing
/// isn't on screen — the caller then starts from 0 rather than no-op.
///
/// Resolved against the FULL ring (collapsed stops included), so a collapsed
/// *current* workspace still anchors the walk instead of stranding it at 0.
pub(crate) fn ring_current_index(
    ring: &[RingStop],
    active_workspace_slug: Option<&str>,
    active_host_key: Option<&str>,
) -> Option<usize> {
    if let Some(key) = active_host_key {
        return ring
            .iter()
            .position(|s| matches!(s, RingStop::TerminalHost { key: k, .. } if k == key));
    }
    let slug = active_workspace_slug?;
    ring.iter()
        .position(|s| matches!(s, RingStop::Workspace { slug: s2, .. } if s2 == slug))
}

/// The ring index a Shift+Alt+↑/↓ step should land on, or `None` when the ring
/// cannot move (fewer than two stops).
///
/// With `skip_collapsed` the walk continues past folded groups to the next
/// expanded stop, wrapping. If EVERY other stop is collapsed it falls back to
/// the immediate neighbour rather than returning `None` — a fully-folded
/// sidebar must not make the keybind dead.
pub(crate) fn ring_step(
    ring: &[RingStop],
    cur: usize,
    forward: bool,
    skip_collapsed: bool,
) -> Option<usize> {
    let n = ring.len();
    if n < 2 {
        return None;
    }
    let step = |i: usize| {
        if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }
    };
    let neighbour = step(cur.min(n - 1));
    if !skip_collapsed {
        return Some(neighbour);
    }
    let mut i = neighbour;
    for _ in 0..n - 1 {
        if !ring[i].collapsed() {
            return Some(i);
        }
        i = step(i);
    }
    // Everything else is folded: still move, so the keybind never goes dead.
    Some(neighbour)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(slug: &str, collapsed: bool) -> RingStop {
        RingStop::Workspace {
            slug: slug.into(),
            repo_path: Some(format!("/repos/{slug}")),
            collapsed,
        }
    }

    fn host(key: &str, collapsed: bool) -> RingStop {
        RingStop::TerminalHost {
            key: key.into(),
            collapsed,
        }
    }

    #[test]
    fn steps_over_a_collapsed_workspace_both_directions() {
        let ring = vec![ws("a", false), ws("b", true), ws("c", false)];
        assert_eq!(ring_step(&ring, 0, true, true), Some(2));
        assert_eq!(ring_step(&ring, 2, false, true), Some(0));
        // …and wraps past it too.
        assert_eq!(ring_step(&ring, 2, true, true), Some(0));
        assert_eq!(ring_step(&ring, 0, false, true), Some(2));
    }

    #[test]
    fn steps_over_a_collapsed_terminal_host() {
        // The workspaces↔terminals boundary is invisible to the walk: one ring.
        let ring = vec![ws("a", false), host("local", true), host("prod", false)];
        assert_eq!(ring_step(&ring, 0, true, true), Some(2));
        assert_eq!(ring_step(&ring, 2, true, true), Some(0));
    }

    #[test]
    fn a_collapsed_current_stop_still_anchors_the_walk() {
        // The user folded the workspace they are standing in; stepping must
        // move one expanded stop on, not strand or no-op.
        let ring = vec![ws("a", false), ws("b", true), ws("c", false)];
        assert_eq!(ring_step(&ring, 1, true, true), Some(2));
        assert_eq!(ring_step(&ring, 1, false, true), Some(0));
    }

    #[test]
    fn all_others_collapsed_falls_back_to_the_neighbour() {
        let ring = vec![ws("a", false), ws("b", true), ws("c", true)];
        assert_eq!(ring_step(&ring, 0, true, true), Some(1));
        assert_eq!(ring_step(&ring, 0, false, true), Some(2));
        // Even a wholly folded sidebar keeps moving.
        let ring = vec![ws("a", true), ws("b", true)];
        assert_eq!(ring_step(&ring, 0, true, true), Some(1));
    }

    #[test]
    fn skip_disabled_reproduces_plain_modular_stepping() {
        let ring = vec![ws("a", false), ws("b", true), ws("c", false)];
        for cur in 0..ring.len() {
            assert_eq!(ring_step(&ring, cur, true, false), Some((cur + 1) % 3));
            assert_eq!(ring_step(&ring, cur, false, false), Some((cur + 2) % 3));
        }
    }

    #[test]
    fn a_ring_that_cannot_move_returns_none() {
        assert_eq!(ring_step(&[], 0, true, true), None);
        assert_eq!(ring_step(&[ws("a", false)], 0, true, true), None);
        assert_eq!(ring_step(&[ws("a", true)], 0, true, false), None);
    }

    #[test]
    fn ring_current_index_matches_by_slug_or_host_key() {
        let ring = vec![ws("a", true), host("prod", false)];
        // A collapsed workspace is still locatable.
        assert_eq!(ring_current_index(&ring, Some("a"), None), Some(0));
        assert_eq!(ring_current_index(&ring, None, Some("prod")), Some(1));
        assert_eq!(ring_current_index(&ring, Some("ghost"), None), None);
    }
}
