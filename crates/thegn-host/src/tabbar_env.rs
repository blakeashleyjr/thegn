//! The center tab bar's right-aligned **env cluster** — the sandbox backend
//! `(backend)` and remote-placement `[kind]` chips for the active worktree.
//!
//! These sit just left of the pin chips and mirror the sidebar's
//! `(backend) [placement]` reading order. Kept in a sibling module (rather than
//! in the pinned god-file `chrome.rs`) so the tab bar can surface them without
//! growing `chrome.rs`. The chips reuse `chrome`'s draw primitives; the width
//! math matches `pin_chips_start`/`strip_chip_spans` (char count), so the tab
//! boundary the three share can never drift.

use crate::chrome::{FrameModel, S, col, draw_text};
use crate::compositor::Rect;
use termwiz::surface::Surface;

/// The env-cluster chip labels for the active worktree, in reading order:
/// `(backend)` first (when sandboxed), then the terse placement `[kind]` (when
/// remote). Empty when the worktree runs locally with no sandbox.
pub(crate) fn env_chips(model: &FrameModel) -> Vec<String> {
    let backend = model.active_sandbox_backend.as_str();
    let devcontainer_status = model
        .sidebar_rows
        .iter()
        .find(|row| row.active && matches!(row.kind, crate::sidebar::RowKind::Worktree))
        .and_then(|row| row.devcontainer_status.as_deref());
    let mut chips = runtime_sandbox_chips(backend, devcontainer_status);
    // Remote placement kind (ssh/mosh/k8s/<provider>); `None` when local. The
    // full detail (ssh:host, sprite:<id>, …) lives in the System → Sandbox panel.
    if let Some(kind) = &model.active_placement_kind {
        chips.push(format!("[{kind}]"));
    }
    if let Some(toolchain) = &model.panel.toolchain {
        let token = match toolchain.state.as_str() {
            "ready" | "shims" => "(mise)",
            "pending-trust" => "(mise ~)",
            "missing-binary" | "missing-shims" | "missing-tools" | "degraded" => "(mise !)",
            _ => "",
        };
        if !token.is_empty() {
            chips.push(token.to_string());
        }
    }
    chips
}

/// Runtime-truthful sandbox chips. A devcontainer status is repository/config
/// metadata until the observed backend confirms this pane actually entered the
/// devcontainer provider. Pending/degraded metadata remains available in the
/// sidebar, but never masquerades as active containment in the tab bar.
fn runtime_sandbox_chips(backend: &str, devcontainer_status: Option<&str>) -> Vec<String> {
    if backend == "devcontainer" {
        return vec![devcontainer_status.unwrap_or("(devcontainer)").to_string()];
    }
    if !backend.is_empty() && backend != "none" && backend != "host" {
        return vec![format!("({backend})")];
    }
    Vec::new()
}

/// Total columns the env cluster occupies, including the one-column leading gap
/// that separates it from the tab chips. `0` when there are no chips — the tab
/// boundary is then just the pin-chips start.
pub(crate) fn env_cluster_width(model: &FrameModel) -> usize {
    let chips = env_chips(model);
    if chips.is_empty() {
        return 0;
    }
    // Each chip is joined by a single space, plus a leading gap before the group.
    let inner: usize = chips.iter().map(|c| c.chars().count()).sum();
    1 + inner + chips.len().saturating_sub(1)
}

/// Draw the env cluster right-aligned so it ends just before `pins_start`, and
/// return the left-most column it occupies — the right boundary the tab chips
/// must stop before. Returns `pins_start` unchanged when the cluster is empty.
pub(crate) fn draw_env_chips(
    surface: &mut Surface,
    strip: Rect,
    pins_start: usize,
    model: &FrameModel,
) -> usize {
    let width = env_cluster_width(model);
    if width == 0 {
        return pins_start;
    }
    let start = pins_start.saturating_sub(width).max(strip.x);
    let dim = col(S::Dim);
    let bg = col(S::Bg0);
    // Leading gap, then space-separated chips.
    let mut x = start + 1;
    for (i, chip) in env_chips(model).iter().enumerate() {
        if i > 0 {
            x += 1; // space between chips
        }
        if x >= pins_start {
            break;
        }
        let avail = pins_start.saturating_sub(x);
        draw_text(surface, x, strip.y, chip, dim, bg, avail);
        x += chip.chars().count();
    }
    start
}

#[cfg(test)]
mod tests {
    use super::runtime_sandbox_chips;

    #[test]
    fn repo_devcontainer_status_never_labels_a_host_pane_as_contained() {
        let pending = "dc:.devcontainer.json [pending]";
        assert!(runtime_sandbox_chips("host", Some(pending)).is_empty());
        assert!(runtime_sandbox_chips("none", Some(pending)).is_empty());
        assert_eq!(
            runtime_sandbox_chips("docker", Some(pending)),
            vec!["(docker)"]
        );
        assert_eq!(
            runtime_sandbox_chips("devcontainer", Some(pending)),
            vec![pending]
        );
    }
}
