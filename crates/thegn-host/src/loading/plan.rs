//! The one loading-screen standard: a single visual vocabulary (glyph + color
//! per [`StepState`]) and a single builder ([`LoadPlan`]) that every progress
//! producer — worktree creation, sandbox/provider provisioning, the focused
//! materialize path, and the workspace-clone modal — funnels through.
//!
//! Before this module each producer hand-rolled its own `Vec<LoadStep>` and the
//! clone modal drew a different "working" glyph (`⟳`) in different colors from
//! the center-pane splash (`↻`), so the two drifted. Now the glyph/color choice
//! lives in exactly one place ([`visual_glyph`] / [`label_color`]) and the step
//! list is assembled in exactly one place ([`LoadPlan`]). Pure + unit-tested;
//! no I/O, so the standard is locked by the tests, not by convention.

use termwiz::color::ColorAttribute;

use crate::chrome::{LoadStep, S, StepState, col};

/// The canonical glyph + foreground color for a step state — the single source
/// of truth for "what does each loading state look like". Both the center-pane
/// splash ([`crate::logotype`]) and the clone modal
/// ([`crate::workspace_picker`]) render through this, so they can never drift.
///
/// The in-progress (`Active`) glyph is a distinct "working" mark (`↻` / ascii
/// `@`) in the caller's `accent`, not another dot, so it reads as actively
/// loading against the pending rows. Glyphs come from [`crate::caps`] so an
/// ASCII-only terminal degrades correctly.
pub fn visual_glyph(state: StepState, accent: ColorAttribute) -> (&'static str, ColorAttribute) {
    let g = crate::caps::active_glyphs();
    match state {
        StepState::Done => (g.check, col(S::Dim)),
        StepState::Active => (g.refresh, accent),
        StepState::Pending => (g.diamond_hollow, col(S::Ghost)),
        StepState::Failed => (g.cross, col(S::Ghost)),
    }
}

/// The canonical label foreground color for a step state (the text beside the
/// glyph). Active is full-strength text; done dims; pending/failed recede.
pub fn label_color(state: StepState) -> ColorAttribute {
    match state {
        StepState::Done => col(S::Dim),
        StepState::Active => col(S::Text),
        StepState::Pending | StepState::Failed => col(S::Ghost),
    }
}

/// A built loading screen: an ordered list of steps. Every progress producer
/// builds one of these instead of assembling a raw `Vec<LoadStep>`, so step
/// shape stays consistent across surfaces. (The key/value context facts drawn
/// beneath the steps are carried separately in `FrameModel::load_context`.)
#[derive(Debug, Default, Clone)]
pub struct LoadPlan {
    steps: Vec<LoadStep>,
}

impl LoadPlan {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a step in the given state.
    #[must_use]
    pub fn step(mut self, label: impl Into<String>, state: StepState) -> Self {
        self.steps.push(step(label, state, None));
        self
    }

    /// Append a step carrying a dim sub-line (a live status for an active step
    /// or the captured error for a failed one).
    #[must_use]
    pub fn step_detail(
        mut self,
        label: impl Into<String>,
        state: StepState,
        detail: impl Into<String>,
    ) -> Self {
        self.steps.push(step(label, state, Some(detail.into())));
        self
    }

    /// Build from an ordered label list plus a `cursor` (the index of the
    /// running step): everything before the cursor reads `Done`, the step at
    /// the cursor reads `Active` (or `Failed` when `failed`), everything after
    /// reads `Pending`. A cursor past the end marks every step `Done`. This is
    /// the shape the focused materialize path and the provider provisioner both
    /// want, so they no longer open-code the before/at/after match.
    pub fn from_cursor(labels: &[&str], cursor: usize, failed: bool) -> Self {
        let steps = labels
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let state = match i.cmp(&cursor) {
                    std::cmp::Ordering::Less => StepState::Done,
                    std::cmp::Ordering::Equal if failed => StepState::Failed,
                    std::cmp::Ordering::Equal => StepState::Active,
                    std::cmp::Ordering::Greater => StepState::Pending,
                };
                step(*label, state, None)
            })
            .collect();
        Self { steps }
    }

    /// The assembled steps.
    pub fn into_steps(self) -> Vec<LoadStep> {
        self.steps
    }
}

/// Construct a [`LoadStep`] with an optional detail sub-line. The one place a
/// `LoadStep` is stamped from `(label, state, detail)`.
fn step(label: impl Into<String>, state: StepState, detail: Option<String>) -> LoadStep {
    let s = LoadStep {
        label: label.into(),
        state,
        detail: None,
    };
    match detail {
        Some(d) => s.with_detail(d),
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_cursor_marks_before_at_after() {
        let steps = LoadPlan::from_cursor(&["a", "b", "c"], 1, false).into_steps();
        assert_eq!(steps[0].state, StepState::Done);
        assert_eq!(steps[1].state, StepState::Active);
        assert_eq!(steps[2].state, StepState::Pending);
    }

    #[test]
    fn from_cursor_zero_and_past_end() {
        let first = LoadPlan::from_cursor(&["a", "b", "c"], 0, false).into_steps();
        assert_eq!(first[0].state, StepState::Active);
        assert_eq!(first[1].state, StepState::Pending);
        // Cursor past the end ⇒ every step is done.
        let done = LoadPlan::from_cursor(&["a", "b", "c"], 9, false).into_steps();
        assert!(done.iter().all(|s| s.state == StepState::Done));
    }

    #[test]
    fn from_cursor_failed_marks_only_the_cursor() {
        let steps = LoadPlan::from_cursor(&["a", "b", "c"], 1, true).into_steps();
        assert_eq!(steps[0].state, StepState::Done);
        assert_eq!(steps[1].state, StepState::Failed);
        assert_eq!(steps[2].state, StepState::Pending);
    }

    #[test]
    fn from_cursor_preserves_shell_wait_invariant() {
        // The materialize shape must keep `shell` as the last label so
        // `loading::is_shell_wait` still gates the premature-shell machinery.
        let steps =
            LoadPlan::from_cursor(&["sandbox", "container", "shell"], 2, false).into_steps();
        assert_eq!(steps.last().unwrap().label, "shell");
        assert!(super::super::is_shell_wait(&steps));
    }

    #[test]
    fn builder_assembles_steps_with_detail() {
        let steps = LoadPlan::new()
            .step("resolve base", StepState::Done)
            .step_detail("create worktree", StepState::Failed, "boom")
            .step("register", StepState::Pending)
            .into_steps();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].state, StepState::Done);
        assert_eq!(steps[1].detail.as_deref(), Some("boom"));
        assert_eq!(steps[2].state, StepState::Pending);
    }

    #[test]
    fn empty_detail_is_dropped() {
        let steps = LoadPlan::new()
            .step_detail("x", StepState::Active, "   ")
            .into_steps();
        assert_eq!(steps[0].detail, None, "blank detail is not attached");
    }

    #[test]
    fn label_color_distinguishes_states() {
        // Active is full text; done dims; pending/failed share the recede color.
        assert_eq!(label_color(StepState::Active), col(S::Text));
        assert_eq!(label_color(StepState::Done), col(S::Dim));
        assert_eq!(label_color(StepState::Pending), col(S::Ghost));
        assert_eq!(label_color(StepState::Failed), col(S::Ghost));
    }
}
