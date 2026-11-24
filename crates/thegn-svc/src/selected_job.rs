//! The CLI-facing half of the typed selected-runner route.
//!
//! `thegn-job-spec` owns the closed contract (`submit_selected`, the recipes,
//! the receipt) and the Sprites and Vercel executors own the remote work; this
//! module is the glue a command needs between them: capture the pinned source
//! identity of a worktree the way the executors verify it, build the typed spec
//! the route accepts, dispatch it to exactly one runner, and recognise a
//! recursive call from inside a selected job. It holds no credential and no
//! provider knowledge.

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
pub use thegn_cas::Digest;
use thegn_job_spec::{JobSpec, submit_selected};
pub use thegn_job_spec::{
    Recipe, RunnerDirectory, RunnerRefusal, SelectedCancel, SelectedCompletion, SelectedFailure,
    SelectedJobResult, SelectedRunner,
};

pub use crate::sprites::job::SELECTED_JOB_ENV;
use crate::sprites::job::{DirSnapshot, SnapshotSource};

/// True when this process runs inside a selected job: the executors' launch
/// scripts set [`SELECTED_JOB_ENV`] in the recipe's scrubbed environment, so a
/// `thegn job run` started by the recipe itself must refuse to recurse.
pub fn inside_selected_job() -> bool {
    marker_set(std::env::var_os(SELECTED_JOB_ENV).as_deref())
}

fn marker_set(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value.to_str() != Some("0"))
}

/// The stable spelling of a recipe (its job-spec name).
pub fn recipe_name(recipe: Recipe) -> &'static str {
    match recipe {
        Recipe::CargoBuild => "cargo-build",
        Recipe::CargoTest => "cargo-test",
    }
}

/// The REAPI root digest of `root` as the executors capture it: the same
/// `.gitignore`-aware walk (`DirSnapshot`) they run before materializing, so a
/// tree edited between this call and the dispatch fails the executor's digest
/// check instead of running unpinned content.
pub fn capture_digest(root: &Path) -> Result<Digest> {
    DirSnapshot::new(root)
        .capture()
        .with_context(|| format!("cannot snapshot {}", root.display()))?
        .digest()
        .with_context(|| format!("cannot compute the source digest of {}", root.display()))
}

/// The typed spec the selected route accepts: a recipe, an explicit runner alias,
/// a pinned source and a deadline (the spec `timeout`). Nothing else is set, so
/// the route has no field it could silently ignore.
pub fn pinned_spec(
    alias: &str,
    recipe: Recipe,
    source: &Digest,
    deadline: Duration,
) -> Result<JobSpec> {
    let name = recipe_name(recipe);
    let value = serde_json::json!({
        "apiVersion": "thegn.dev/jobs/v0",
        "kind": "Job",
        "metadata": { "name": format!("selected-{name}") },
        "runner": alias,
        "recipe": name,
        "source": { "snapshot": source },
        "environment": { "image": "selected-runner" },
        "timeout": format!("{}s", deadline.as_secs().max(1)),
    });
    let spec: JobSpec =
        serde_json::from_value(value).context("cannot build the selected job spec")?;
    spec.validate()
        .context("the selected job spec is not valid")?;
    Ok(spec)
}

/// A directory that resolves exactly one alias to one runner. There is no
/// placement-based fallback: any other alias is unavailable.
pub struct SingleRunner<'a> {
    alias: &'a str,
    runner: &'a dyn SelectedRunner,
}

impl<'a> SingleRunner<'a> {
    pub fn new(alias: &'a str, runner: &'a dyn SelectedRunner) -> Self {
        Self { alias, runner }
    }
}

impl RunnerDirectory for SingleRunner<'_> {
    fn resolve(&self, alias: &str) -> Option<&dyn SelectedRunner> {
        (alias == self.alias).then_some(self.runner)
    }
}

/// Snapshot `root`, pin its digest, and submit `recipe` to `runner` through the
/// typed route. The outer error is a local failure before anything was sent
/// (snapshot or spec); the inner result is the route's verdict.
///
/// Blocking: it walks the tree and the runner's `submit` blocks on its own
/// runtime, so call it from a plain thread, never from a runtime worker.
pub fn run_selected(
    runner: &dyn SelectedRunner,
    alias: &str,
    recipe: Recipe,
    root: &Path,
    deadline: Duration,
    cancel: &SelectedCancel,
) -> Result<Result<SelectedJobResult, SelectedFailure>> {
    let source = capture_digest(root)?;
    let spec = pinned_spec(alias, recipe, &source, deadline)?;
    let directory = SingleRunner::new(alias, runner);
    Ok(submit_selected(&spec, &source, &directory, cancel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Recording {
        alias: String,
        seen: Mutex<Vec<(Recipe, Digest, Duration)>>,
        completion: SelectedCompletion,
    }

    impl SelectedRunner for Recording {
        fn submit(
            &self,
            runner_alias: &str,
            recipe: Recipe,
            source: &Digest,
            deadline: Duration,
            _cancel: &SelectedCancel,
        ) -> Result<SelectedJobResult, RunnerRefusal> {
            self.seen
                .lock()
                .unwrap()
                .push((recipe, source.clone(), deadline));
            Ok(SelectedJobResult {
                runner: runner_alias.to_owned(),
                recipe,
                source: source.clone(),
                completion: self.completion,
                log_tail: "ok".into(),
                truncated: false,
            })
        }
    }

    fn recording(alias: &str, completion: SelectedCompletion) -> Recording {
        Recording {
            alias: alias.to_owned(),
            seen: Mutex::new(Vec::new()),
            completion,
        }
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        dir
    }

    #[test]
    fn the_recursion_marker_is_any_value_but_empty_or_zero() {
        assert!(!marker_set(None));
        assert!(!marker_set(Some(OsStr::new(""))));
        assert!(!marker_set(Some(OsStr::new("0"))));
        assert!(marker_set(Some(OsStr::new("1"))));
        assert!(marker_set(Some(OsStr::new("yes"))));
    }

    #[test]
    fn the_marker_name_is_the_one_the_launchers_set() {
        assert_eq!(SELECTED_JOB_ENV, "THEGN_SELECTED_JOB");
    }

    #[test]
    fn a_pinned_spec_is_valid_for_every_recipe_and_carries_the_deadline() {
        let digest = Digest::of(b"tree");
        for recipe in [Recipe::CargoBuild, Recipe::CargoTest] {
            let spec = pinned_spec("sprites-ci", recipe, &digest, Duration::from_secs(90)).unwrap();
            assert_eq!(spec.runner.as_deref(), Some("sprites-ci"));
            assert_eq!(spec.recipe, Some(recipe));
            assert_eq!(spec.timeout, "90s");
            assert!(spec.steps.is_empty());
        }
    }

    #[test]
    fn a_pinned_spec_refuses_an_invalid_runner_alias() {
        let digest = Digest::of(b"tree");
        let error = pinned_spec(
            "Not An Alias",
            Recipe::CargoBuild,
            &digest,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("runner"), "{error:#}");
    }

    #[test]
    fn the_directory_resolves_only_its_own_alias() {
        let runner = recording("sprites-ci", SelectedCompletion::Exited(0));
        let directory = SingleRunner::new("sprites-ci", &runner);
        assert!(directory.resolve("sprites-ci").is_some());
        assert!(directory.resolve("vercel-other").is_none());
    }

    #[test]
    fn the_captured_digest_follows_the_tree_content() {
        let dir = tree();
        let first = capture_digest(dir.path()).unwrap();
        assert_eq!(first, capture_digest(dir.path()).unwrap());
        std::fs::write(dir.path().join("src/main.rs"), "fn main() { dirty(); }\n").unwrap();
        assert_ne!(first, capture_digest(dir.path()).unwrap());
    }

    #[test]
    fn run_selected_pins_the_tree_and_forwards_recipe_and_deadline() {
        let dir = tree();
        let runner = recording("sprites-ci", SelectedCompletion::Exited(3));
        let cancel = SelectedCancel::new();
        let result = run_selected(
            &runner,
            &runner.alias,
            Recipe::CargoTest,
            dir.path(),
            Duration::from_secs(120),
            &cancel,
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.completion, SelectedCompletion::Exited(3));
        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, Recipe::CargoTest);
        assert_eq!(seen[0].1, capture_digest(dir.path()).unwrap());
        assert_eq!(seen[0].2, Duration::from_secs(120));
    }

    #[test]
    fn run_selected_reports_a_local_snapshot_failure_before_any_dispatch() {
        let runner = recording("sprites-ci", SelectedCompletion::Exited(0));
        let error = run_selected(
            &runner,
            &runner.alias,
            Recipe::CargoBuild,
            Path::new("/nonexistent/thegn-selected-job-root"),
            Duration::from_secs(5),
            &SelectedCancel::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("cannot snapshot"),
            "{error:#}"
        );
        assert!(runner.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_cancel_before_dispatch_never_reaches_the_runner() {
        let dir = tree();
        let runner = recording("sprites-ci", SelectedCompletion::Exited(0));
        let cancel = SelectedCancel::new();
        cancel.cancel();
        let result = run_selected(
            &runner,
            &runner.alias,
            Recipe::CargoBuild,
            dir.path(),
            Duration::from_secs(5),
            &cancel,
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.completion, SelectedCompletion::Cancelled);
        assert!(runner.seen.lock().unwrap().is_empty());
    }
}
