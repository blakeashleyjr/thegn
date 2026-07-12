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

use crate::chrome::{LoadStep, S, StepKind, StepState, col};

/// The canonical glyph + foreground color for a step state — the single source
/// of truth for "what does each loading state look like". Both the center-pane
/// splash ([`crate::logotype`]) and the clone modal
/// ([`crate::workspace_picker`]) render through this, so they can never drift.
///
/// The in-progress (`Active`) glyph is a distinct "working" mark (`↻` / ascii
/// `@`) in the caller's `accent`, not another dot, so it reads as actively
/// loading against the pending rows. A failed step is RED — the one state the
/// user must not skim past. Glyphs come from [`crate::caps`] so an ASCII-only
/// terminal degrades correctly.
pub fn visual_glyph(state: StepState, accent: ColorAttribute) -> (&'static str, ColorAttribute) {
    let g = crate::caps::active_glyphs();
    match state {
        StepState::Done => (g.check, col(S::Dim)),
        StepState::Active => (g.refresh, accent),
        StepState::Pending => (g.diamond_hollow, col(S::Ghost)),
        StepState::Failed => (g.cross, crate::chrome::theme_color(thegn_core::theme::RED)),
    }
}

/// [`visual_glyph`] with liveness: an `Active` step whose elapsed time is
/// known animates through the spinner frames instead of the static working
/// mark. Frame choice is a pure function of the ambient clock (see
/// [`spinner_now`]) — the splash-scoped ticker just causes repaints.
pub fn visual_glyph_live(
    state: StepState,
    accent: ColorAttribute,
    animate: bool,
) -> (&'static str, ColorAttribute) {
    match state {
        StepState::Active if animate => (spinner_now(), accent),
        _ => visual_glyph(state, accent),
    }
}

/// The canonical label foreground color for a step state (the text beside the
/// glyph). Active is full-strength text; done dims; pending recedes; failed
/// stays full-strength so the failure line reads.
pub fn label_color(state: StepState) -> ColorAttribute {
    match state {
        StepState::Done => col(S::Dim),
        StepState::Active | StepState::Failed => col(S::Text),
        StepState::Pending => col(S::Ghost),
    }
}

/// Spinner period per frame. Four frames at 250ms = one revolution/second,
/// matched to the splash ticker cadence so every tick advances one frame.
const SPINNER_FRAME_MS: u128 = 250;

/// The current spinner frame, sampled from a process-lifetime epoch (the
/// `owl::blink_now` pattern): pure — nothing schedules a wake for it; the
/// splash-scoped ticker repaints while a splash is visible and each repaint
/// samples the frame fresh. Frames come from [`crate::caps`] (ASCII `|/-\`).
pub fn spinner_now() -> &'static str {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    spinner_at(EPOCH.get_or_init(Instant::now).elapsed().as_millis())
}

/// The spinner frame at `elapsed_ms` since the epoch. Pure core of
/// [`spinner_now`], unit-testable.
pub fn spinner_at(elapsed_ms: u128) -> &'static str {
    let frames = crate::caps::active_glyphs().spin;
    frames[(elapsed_ms / SPINNER_FRAME_MS) as usize % frames.len()]
}

/// Progress-bar cell strings for `width` cells at `fraction` (0..=1):
/// `(filled, empty)` runs of the capability-correct bar glyphs. An unknown
/// fraction (`None`, indeterminate) renders all-empty. Shared by the splash
/// and the clone modal so the bar can never drift between them.
pub fn bar(width: usize, fraction: Option<f64>) -> (String, String) {
    let g = crate::caps::active_glyphs();
    let filled = fraction
        .map(|f| ((f.clamp(0.0, 1.0) * width as f64).round() as usize).min(width))
        .unwrap_or(0);
    (
        g.bar_fill.repeat(filled),
        g.bar_empty.repeat(width - filled),
    )
}

/// Compact elapsed-time text for the splash's right column: `3.4s`, `38s`,
/// `1m09s`, `1h02m`. Fixed six-column budget (see the splash layout).
pub fn fmt_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 10 {
        format!("{:.1}s", d.as_secs_f64())
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Byte-count text for progress lines — re-exported from the core parser so
/// the splash and the pull snapshot format identically.
pub use thegn_core::pull_progress::fmt_bytes;

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

    /// Append a step tagged with its stable [`StepKind`] identity (the
    /// backend-aware catalog plans set kinds so the tracker can match steps
    /// across relabels and the splash can pick slow-step hints).
    #[must_use]
    pub fn step_kinded(
        mut self,
        label: impl Into<String>,
        state: StepState,
        kind: StepKind,
    ) -> Self {
        self.steps.push(step(label, state, None).with_kind(kind));
        self
    }

    /// Build from an ordered label list plus a `cursor` (the index of the
    /// running step): everything before the cursor reads `Done`, the step at
    /// the cursor reads `Active` (or `Failed` when `failed`), everything after
    /// reads `Pending`. A cursor past the end marks every step `Done`.
    /// (The materialize path now seeds from the backend-aware catalog; this
    /// stays as the general cursor-shape builder for label-list producers and
    /// the locked shell-wait-invariant tests.)
    #[cfg_attr(not(test), allow(dead_code))]
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
    let s = match state {
        StepState::Pending => LoadStep::pending(label),
        StepState::Active => LoadStep::active(label),
        StepState::Done => LoadStep::done(label),
        StepState::Failed => LoadStep::failed(label),
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
        // Active is full text; done dims; pending recedes; failed reads at
        // full strength (a failure must not recede into the ghost rows).
        assert_eq!(label_color(StepState::Active), col(S::Text));
        assert_eq!(label_color(StepState::Done), col(S::Dim));
        assert_eq!(label_color(StepState::Pending), col(S::Ghost));
        assert_eq!(label_color(StepState::Failed), col(S::Text));
    }

    #[test]
    fn failed_glyph_is_red() {
        let (glyph, color) = visual_glyph(StepState::Failed, col(S::Text));
        assert_eq!(glyph, crate::caps::active_glyphs().cross);
        assert_eq!(color, crate::chrome::theme_color(thegn_core::theme::RED));
    }

    #[test]
    fn spinner_cycles_with_the_clock() {
        let g = crate::caps::active_glyphs();
        assert_eq!(spinner_at(0), g.spin[0]);
        assert_eq!(spinner_at(250), g.spin[1]);
        assert_eq!(spinner_at(500), g.spin[2]);
        assert_eq!(spinner_at(750), g.spin[3]);
        assert_eq!(spinner_at(1000), g.spin[0], "wraps after one revolution");
        assert_eq!(spinner_at(249), g.spin[0], "holds within a frame window");
    }

    #[test]
    fn visual_glyph_live_animates_only_active() {
        let accent = col(S::Text);
        let g = crate::caps::active_glyphs();
        let (spun, c) = visual_glyph_live(StepState::Active, accent, true);
        assert!(g.spin.contains(&spun), "active+animate uses a spinner frame");
        assert_eq!(c, accent);
        assert_eq!(
            visual_glyph_live(StepState::Active, accent, false),
            visual_glyph(StepState::Active, accent),
            "no animation falls back to the static working mark"
        );
        assert_eq!(
            visual_glyph_live(StepState::Done, accent, true),
            visual_glyph(StepState::Done, accent),
            "non-active states never animate"
        );
    }

    #[test]
    fn bar_fill_math() {
        let g = crate::caps::active_glyphs();
        let (f, e) = bar(20, Some(0.5));
        assert_eq!(f, g.bar_fill.repeat(10));
        assert_eq!(e, g.bar_empty.repeat(10));
        let (f, e) = bar(20, Some(0.0));
        assert!(f.is_empty());
        assert_eq!(e, g.bar_empty.repeat(20));
        let (f, e) = bar(20, Some(1.0));
        assert_eq!(f, g.bar_fill.repeat(20));
        assert!(e.is_empty());
        // Indeterminate: all empty. Out-of-range clamps.
        assert!(bar(20, None).0.is_empty());
        assert_eq!(bar(20, Some(7.0)).0, g.bar_fill.repeat(20));
        assert_eq!(bar(0, Some(0.5)), (String::new(), String::new()));
    }

    #[test]
    fn fmt_elapsed_boundaries() {
        use std::time::Duration as D;
        assert_eq!(fmt_elapsed(D::from_millis(3_400)), "3.4s");
        assert_eq!(fmt_elapsed(D::from_secs(9)), "9.0s");
        assert_eq!(fmt_elapsed(D::from_secs(10)), "10s");
        assert_eq!(fmt_elapsed(D::from_secs(59)), "59s");
        assert_eq!(fmt_elapsed(D::from_secs(69)), "1m09s");
        assert_eq!(fmt_elapsed(D::from_secs(3720)), "1h02m");
    }

    #[test]
    fn step_kinded_tags_identity() {
        let steps = LoadPlan::new()
            .step_kinded("image debian", StepState::Active, StepKind::Image)
            .into_steps();
        assert_eq!(steps[0].kind, StepKind::Image);
        assert_eq!(steps[0].state, StepState::Active);
    }
}
