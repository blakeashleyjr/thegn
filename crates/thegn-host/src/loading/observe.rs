//! `MaterializeObserver` — maps the core's [`SandboxPhase`] progress events
//! (emitted by `sandbox::ensure` / prefetch / build / backend probing on the
//! materialize worker thread) onto a live loading-step list.
//!
//! Pure state machine: `on_event` folds one event and returns the fresh step
//! Vec to publish (the worker sends it over `provision_tx` + wakes the loop;
//! the loop-side `LoadingTracker` stamps timing). Every output ends in the
//! pending `"shell"` step, so `is_shell_wait` semantics for the materialize
//! path are IDENTICAL to the historical `[sandbox, container, shell]` seed —
//! the arbitration helpers in `loading/mod.rs` need no changes (locked by
//! tests here).
//!
//! Phases arrive open-ended (`Connect`/`ImageProbe`/…) and close with
//! `PhaseDone`/`PhaseFailed`. Robustness rule: opening a NEW phase marks any
//! still-active step done — a missing `PhaseDone` must never leave two
//! spinners.

use thegn_core::progress::SandboxPhase;

use crate::chrome::{LoadStep, StepKind, StepState};
use crate::loading::catalog::SHELL_LABEL;

#[derive(Debug, Default)]
pub(crate) struct MaterializeObserver {
    /// Built phase steps, in arrival order — WITHOUT the trailing shell step
    /// (appended at render time so it is always last).
    steps: Vec<LoadStep>,
}

impl MaterializeObserver {
    /// Start with the generic resolve step active — matching the loop-side
    /// seed, so the first refinement doesn't visually restart the splash.
    pub(crate) fn new() -> Self {
        Self {
            steps: vec![LoadStep::active("sandbox").with_kind(StepKind::Resolve)],
        }
    }

    /// Start from the loop-side seed plan (the config-classified catalog plan
    /// or the generic three-step shape), so the observer's first event REFINES
    /// the visible rows instead of replacing them with a shorter list. The
    /// trailing shell step is stripped (render re-appends it); steps the seed
    /// couldn't kind (the generic `container` label) are matched by upsert's
    /// kind vocabulary as events arrive.
    pub(crate) fn from_steps(seed: &[LoadStep]) -> Self {
        let steps: Vec<LoadStep> = seed
            .iter()
            .filter(|s| s.kind != StepKind::Shell && s.label != SHELL_LABEL)
            .cloned()
            .collect();
        if steps.is_empty() {
            return Self::new();
        }
        Self { steps }
    }

    /// Fold one event; returns the step list to publish.
    pub(crate) fn on_event(&mut self, ev: SandboxPhase) -> Vec<LoadStep> {
        match ev {
            SandboxPhase::Resolve => {
                self.upsert(StepKind::Resolve, "sandbox".to_string());
            }
            SandboxPhase::Connect { host } => {
                self.upsert(StepKind::Connect, format!("connect {host}"));
            }
            SandboxPhase::ConnectRetry { attempt, max } => {
                if let Some(s) = self.find_mut(StepKind::Connect) {
                    s.detail = Some(format!("retrying {attempt}/{max}"));
                }
            }
            SandboxPhase::ImageProbe { image } => {
                self.upsert(StepKind::Image, format!("image {image}"));
            }
            SandboxPhase::ImagePull { image } => {
                // Same phase as the probe — relabel, don't stack a step.
                self.upsert(StepKind::Image, format!("pull {image}"));
            }
            SandboxPhase::PullProgress(snap) => {
                if let Some(s) = self.find_mut(StepKind::Image) {
                    s.progress = Some((snap.bytes_done, snap.bytes_total));
                    s.detail = snap.detail();
                }
            }
            SandboxPhase::ImageBuild { tag } => {
                self.upsert(StepKind::Build, format!("build {tag}"));
            }
            SandboxPhase::ContainerCreate { backend } => {
                self.upsert(StepKind::Create, format!("container ({backend})"));
            }
            SandboxPhase::Vpn => {
                self.upsert(StepKind::Vpn, "vpn sidecar".to_string());
            }
            SandboxPhase::PhaseDone => {
                if let Some(s) = self.active_mut() {
                    s.state = StepState::Done;
                    s.detail = None;
                }
            }
            SandboxPhase::PhaseFailed { err } => {
                if let Some(s) = self.active_mut() {
                    s.state = StepState::Failed;
                    s.detail = Some(err);
                }
            }
        }
        self.render()
    }

    /// The published list: built steps + the trailing pending shell step.
    /// ALWAYS ends in `"shell"` (the `is_shell_wait` contract).
    pub(crate) fn render(&self) -> Vec<LoadStep> {
        let mut out = self.steps.clone();
        out.push(LoadStep::pending(SHELL_LABEL).with_kind(StepKind::Shell));
        out
    }

    fn find_mut(&mut self, kind: StepKind) -> Option<&mut LoadStep> {
        self.steps.iter_mut().find(|s| s.kind == kind)
    }

    fn active_mut(&mut self) -> Option<&mut LoadStep> {
        self.steps
            .iter_mut()
            .rev()
            .find(|s| s.state == StepState::Active)
    }

    /// (Re)open the phase step of `kind`: an existing step (same kind — a
    /// probe→pull relabel, a reconnect, or a seeded pending row) reactivates
    /// with the fresh label; a phase the plan didn't anticipate is INSERTED at
    /// the current chronological position (before the first still-pending
    /// step), not appended — events arrive in true order, so an image pull
    /// slots before the seeded container row. Either way every OTHER
    /// still-active step is closed first — phases are sequential, and a
    /// missing `PhaseDone` must never leave two spinners.
    fn upsert(&mut self, kind: StepKind, label: String) {
        for s in &mut self.steps {
            if s.kind != kind && s.state == StepState::Active {
                s.state = StepState::Done;
                s.detail = None;
            }
        }
        if let Some(s) = self.steps.iter_mut().find(|s| s.kind == kind) {
            s.label = label;
            s.state = StepState::Active;
            s.detail = None;
            return;
        }
        let at = self
            .steps
            .iter()
            .position(|s| s.state == StepState::Pending)
            .unwrap_or(self.steps.len());
        let mut s = LoadStep::active(label);
        s.kind = kind;
        self.steps.insert(at, s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::pull_progress::PullSnapshot;

    fn labels(steps: &[LoadStep]) -> Vec<&str> {
        steps.iter().map(|s| s.label.as_str()).collect()
    }

    #[test]
    fn every_output_ends_in_shell() {
        let mut o = MaterializeObserver::new();
        let events = [
            SandboxPhase::Resolve,
            SandboxPhase::Connect { host: "ssh:h".into() },
            SandboxPhase::PhaseDone,
            SandboxPhase::ImageProbe { image: "img".into() },
            SandboxPhase::ImagePull { image: "img".into() },
            SandboxPhase::PhaseDone,
            SandboxPhase::ContainerCreate { backend: "podman" },
            SandboxPhase::PhaseDone,
        ];
        for ev in events {
            let out = o.on_event(ev);
            assert!(
                crate::loading::is_shell_wait(&out),
                "must end in shell: {:?}",
                labels(&out)
            );
        }
    }

    #[test]
    fn oci_local_happy_path_with_probe_hit() {
        let mut o = MaterializeObserver::new();
        o.on_event(SandboxPhase::ImageProbe { image: "debian:stable".into() });
        // Opening the image phase closes the seed resolve step.
        let out = o.render();
        assert_eq!(out[0].state, StepState::Done, "resolve closed");
        assert_eq!(out[1].label, "image debian:stable");
        assert_eq!(out[1].state, StepState::Active);
        // Probe hit: phase closes without a pull relabel.
        let out = o.on_event(SandboxPhase::PhaseDone);
        assert_eq!(out[1].state, StepState::Done);
        let out = o.on_event(SandboxPhase::ContainerCreate { backend: "podman" });
        assert_eq!(
            labels(&out),
            vec!["sandbox", "image debian:stable", "container (podman)", "shell"]
        );
    }

    #[test]
    fn pull_relabels_and_carries_progress() {
        let mut o = MaterializeObserver::new();
        o.on_event(SandboxPhase::ImageProbe { image: "dev:latest".into() });
        let out = o.on_event(SandboxPhase::ImagePull { image: "dev:latest".into() });
        assert_eq!(out[1].label, "pull dev:latest", "probe step relabels");
        assert_eq!(out.len(), 3, "no stacked image step");
        let snap = PullSnapshot {
            bytes_done: 10,
            bytes_total: Some(100),
            layers_done: 0,
            layers_total: 2,
        };
        let out = o.on_event(SandboxPhase::PullProgress(snap));
        assert_eq!(out[1].progress, Some((10, Some(100))));
        assert!(out[1].detail.as_deref().unwrap().contains("/"), "byte detail");
    }

    #[test]
    fn failure_marks_the_active_step_with_the_error() {
        let mut o = MaterializeObserver::new();
        o.on_event(SandboxPhase::ImagePull { image: "x".into() });
        let out = o.on_event(SandboxPhase::PhaseFailed { err: "manifest unknown".into() });
        let failed = &out[1];
        assert_eq!(failed.state, StepState::Failed);
        assert_eq!(failed.detail.as_deref(), Some("manifest unknown"));
        assert!(crate::loading::is_shell_wait(&out), "shell still last");
    }

    #[test]
    fn connect_retry_annotates_and_reconnect_reactivates() {
        let mut o = MaterializeObserver::new();
        o.on_event(SandboxPhase::Connect { host: "ssh:h".into() });
        let out = o.on_event(SandboxPhase::ConnectRetry { attempt: 2, max: 3 });
        let connect = out.iter().find(|s| s.kind == StepKind::Connect).unwrap();
        assert_eq!(connect.detail.as_deref(), Some("retrying 2/3"));
        // Chain probes reconnect: the SAME step reactivates, never a second one.
        o.on_event(SandboxPhase::PhaseDone);
        let out = o.on_event(SandboxPhase::Connect { host: "ssh:h".into() });
        assert_eq!(
            out.iter().filter(|s| s.kind == StepKind::Connect).count(),
            1
        );
        assert_eq!(
            out.iter().find(|s| s.kind == StepKind::Connect).unwrap().state,
            StepState::Active
        );
    }

    #[test]
    fn refines_a_seeded_plan_in_place_and_in_order() {
        // The config-classified seed: sandbox → image → container → shell.
        let seed = crate::loading::catalog::plan_for(
            &crate::loading::catalog::ResolvedTarget {
                backend: crate::loading::catalog::BackendClass::Oci("podman-rootless".into()),
                image: Some("debian:stable".into()),
                ..crate::loading::catalog::ResolvedTarget::host_local()
            },
        )
        .into_steps();
        let mut o = MaterializeObserver::from_steps(&seed);
        // The probe event refines the SEEDED image row (no duplicate), closes
        // the resolve row, and the shell stays last.
        let out = o.on_event(SandboxPhase::ImageProbe { image: "debian:stable".into() });
        assert_eq!(
            labels(&out),
            vec!["sandbox", "image debian:stable", "container (podman-rootless)", "shell"],
            "same rows, refined in place"
        );
        assert_eq!(out[0].state, StepState::Done);
        assert_eq!(out[1].state, StepState::Active);
        // Create matches the seeded container row by kind and relabels it.
        o.on_event(SandboxPhase::PhaseDone);
        let out = o.on_event(SandboxPhase::ContainerCreate { backend: "podman" });
        assert_eq!(out[2].label, "container (podman)");
        assert_eq!(out[2].state, StepState::Active);
        assert_eq!(out.len(), 4, "no stacked duplicates");
    }

    #[test]
    fn generic_seed_slots_a_pull_before_the_container_row() {
        // The kinded generic seed: an unanticipated image phase must insert
        // BEFORE the pending container row (chronological), not append.
        let seed = crate::loading::catalog::generic_seed();
        let mut o = MaterializeObserver::from_steps(&seed);
        let out = o.on_event(SandboxPhase::ImagePull { image: "img".into() });
        assert_eq!(
            labels(&out),
            vec!["sandbox", "pull img", "container", "shell"]
        );
    }

    #[test]
    fn a_new_phase_closes_a_dangling_active_step() {
        let mut o = MaterializeObserver::new();
        // No PhaseDone between these: the create open must close the image.
        o.on_event(SandboxPhase::ImagePull { image: "x".into() });
        let out = o.on_event(SandboxPhase::ContainerCreate { backend: "docker" });
        let image = out.iter().find(|s| s.kind == StepKind::Image).unwrap();
        assert_eq!(image.state, StepState::Done, "dangling active closed");
        assert_eq!(
            out.iter().filter(|s| s.state == StepState::Active).count(),
            1,
            "never two spinners"
        );
    }
}
