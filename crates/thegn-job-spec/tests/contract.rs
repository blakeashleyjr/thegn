use std::time::Duration;

use thegn_cas::Digest;
use thegn_job_spec::{
    Feature, JobSpec, JobStatus, MAX_SELECTED_LOG_BYTES, Recipe, RetryLimit, RunnerDirectory,
    RunnerRefusal, SelectedCancel, SelectedCompletion, SelectedFailure, SelectedJobResult,
    SelectedRunner, byte_quantity, duration_ms, selected_job_readback, submit_selected,
};

const EXAMPLE: &str = include_str!("fixtures/web-e2e.yaml");
const MINIMAL: &str = "apiVersion: thegn.dev/jobs/v0\nkind: Job\nmetadata: {name: build}\nenvironment: {image: ci}\nsteps: [{name: build, argv: [cargo, build]}]\n";
const SELECTED: &str = include_str!("fixtures/selected-cargo-test.yaml");
const SELECTED_BUILD: &str = include_str!("fixtures/selected-cargo-build.yaml");

struct FixtureRunner;
impl SelectedRunner for FixtureRunner {
    fn submit(
        &self,
        runner_alias: &str,
        recipe: Recipe,
        source: &Digest,
        deadline: Duration,
        _cancel: &SelectedCancel,
    ) -> Result<SelectedJobResult, RunnerRefusal> {
        assert_eq!(runner_alias, "fixture-linux");
        assert_eq!(deadline, Duration::from_secs(60 * 60));
        match recipe {
            Recipe::CargoBuild => assert_eq!(recipe.argv(), ["cargo", "build"]),
            Recipe::CargoTest => assert_eq!(recipe.argv(), ["cargo", "test"]),
        }
        Ok(SelectedJobResult {
            runner: runner_alias.to_owned(),
            recipe,
            source: source.clone(),
            completion: SelectedCompletion::Exited(0),
            log_tail: "fixture passed".into(),
            truncated: false,
        })
    }
}

struct FixtureDirectory;
impl RunnerDirectory for FixtureDirectory {
    fn resolve(&self, alias: &str) -> Option<&dyn SelectedRunner> {
        (alias == "fixture-linux").then_some(&FixtureRunner)
    }
}

#[test]
fn typed_selected_recipe_routes_only_to_owner_named_fixture_runner() {
    let spec = JobSpec::parse_yaml(SELECTED_BUILD).unwrap();
    let source = Digest::of(b"");
    let result =
        submit_selected(&spec, &source, &FixtureDirectory, &SelectedCancel::new()).unwrap();
    assert_eq!(result.runner, "fixture-linux");
    assert_eq!(result.recipe, Recipe::CargoBuild);
    assert_eq!(result.source, source);
    assert_eq!(result.completion, SelectedCompletion::Exited(0));
    assert_eq!(result.log_tail, "fixture passed");
}

#[test]
fn selected_runner_readback_distinguishes_accepted_unavailable_cancelled_and_failed() {
    let spec = JobSpec::parse_yaml(SELECTED_BUILD).unwrap();
    let source = Digest::of(b"");
    let accepted =
        submit_selected(&spec, &source, &FixtureDirectory, &SelectedCancel::new()).unwrap();
    let accepted = selected_job_readback("job-accepted", Ok(accepted));
    assert!(matches!(accepted.status, JobStatus::Succeeded));
    assert_eq!(accepted.log_tail, "fixture passed");
    assert!(!accepted.truncated.log_tail);
    assert!(accepted.placement.is_empty());
    assert!(accepted.artifacts.is_empty());

    struct Empty;
    impl RunnerDirectory for Empty {
        fn resolve(&self, _: &str) -> Option<&dyn SelectedRunner> {
            None
        }
    }
    let unavailable = submit_selected(&spec, &source, &Empty, &SelectedCancel::new()).unwrap_err();
    assert_eq!(
        unavailable,
        SelectedFailure::Refused(RunnerRefusal::RunnerUnavailable("fixture-linux".into()))
    );
    let unavailable = selected_job_readback("job-unavailable", Err(unavailable));
    assert!(matches!(unavailable.status, JobStatus::Rejected { .. }));
    assert_eq!(unavailable.log_tail, "");

    let mut cancelled = FixtureRunner
        .submit(
            "fixture-linux",
            Recipe::CargoBuild,
            &source,
            Duration::from_secs(60 * 60),
            &SelectedCancel::new(),
        )
        .unwrap();
    cancelled.completion = SelectedCompletion::Cancelled;
    let cancelled = selected_job_readback("job-cancelled", Ok(cancelled));
    assert!(matches!(cancelled.status, JobStatus::Cancelled));
    assert!(cancelled.summary.incomplete);

    let mut timed_out = FixtureRunner
        .submit(
            "fixture-linux",
            Recipe::CargoBuild,
            &source,
            Duration::from_secs(60 * 60),
            &SelectedCancel::new(),
        )
        .unwrap();
    timed_out.completion = SelectedCompletion::TimedOut;
    let timed_out = selected_job_readback("job-timed-out", Ok(timed_out));
    assert!(matches!(timed_out.status, JobStatus::TimedOut));
    assert!(timed_out.summary.incomplete);
    assert!(timed_out.diagnostics[0].message.contains("deadline"));

    let mut signalled = FixtureRunner
        .submit(
            "fixture-linux",
            Recipe::CargoBuild,
            &source,
            Duration::from_secs(60 * 60),
            &SelectedCancel::new(),
        )
        .unwrap();
    signalled.completion = SelectedCompletion::Signalled(9);
    let signalled = selected_job_readback("job-signalled", Ok(signalled));
    assert!(matches!(signalled.status, JobStatus::Failed { .. }));
    assert!(signalled.diagnostics[0].message.contains("signal 9"));

    let mut failed = FixtureRunner
        .submit(
            "fixture-linux",
            Recipe::CargoBuild,
            &source,
            Duration::from_secs(60 * 60),
            &SelectedCancel::new(),
        )
        .unwrap();
    failed.completion = SelectedCompletion::Exited(17);
    let failed = selected_job_readback("job-failed", Ok(failed));
    assert!(matches!(failed.status, JobStatus::Failed { .. }));
    assert!(failed.diagnostics[0].message.contains("status 17"));
    let wire = serde_json::to_value(failed).unwrap();
    assert_eq!(wire["jobId"], "job-failed");
    assert_eq!(wire["status"]["kind"], "Failed");

    let uncertain = selected_job_readback(
        "job-indeterminate",
        Err(SelectedFailure::Unproven(RunnerRefusal::Indeterminate)),
    );
    assert!(matches!(uncertain.status, JobStatus::Indeterminate { .. }));
}

#[test]
fn typed_selected_recipe_refuses_source_mismatch_and_missing_runner_without_fallback() {
    let spec = JobSpec::parse_yaml(SELECTED).unwrap();
    assert!(matches!(
        submit_selected(
            &spec,
            &Digest::of(b"changed"),
            &FixtureDirectory,
            &SelectedCancel::new()
        ),
        Err(SelectedFailure::Refused(
            RunnerRefusal::SourceMismatch { .. }
        ))
    ));
    struct Empty;
    impl RunnerDirectory for Empty {
        fn resolve(&self, _: &str) -> Option<&dyn SelectedRunner> {
            None
        }
    }
    assert_eq!(
        submit_selected(&spec, &Digest::of(b""), &Empty, &SelectedCancel::new()),
        Err(SelectedFailure::Refused(RunnerRefusal::RunnerUnavailable(
            "fixture-linux".into()
        )))
    );
}

#[test]
fn portable_example_requires_one_offering_to_support_every_feature() {
    let spec = JobSpec::parse_yaml(EXAMPLE).unwrap();
    assert_eq!(spec.shards.get(), 8);
    let required = spec.required_features();
    assert_eq!(
        required,
        [
            Feature::Oci,
            Feature::Shm,
            Feature::Services,
            Feature::CacheVolume,
            Feature::Junit
        ]
        .into()
    );
    let offerings = [
        ["oci", "shm", "services"].map(str::to_owned).into(),
        ["oci", "cache-volume", "junit"].map(str::to_owned).into(),
    ];
    assert_eq!(
        spec.missing_features(&offerings[0]),
        ["cache-volume", "junit"]
    );
    assert_eq!(spec.missing_features(&offerings[1]), ["shm", "services"]);
    let complete = required
        .into_iter()
        .map(|f| f.wire_name().to_owned())
        .collect();
    assert!(spec.missing_features(&complete).is_empty());
    assert_eq!(
        byte_quantity(spec.resources.memory.as_ref().unwrap()).unwrap(),
        16 << 30
    );
    assert_eq!(duration_ms(&spec.timeout).unwrap(), 1_800_000);
    let json = serde_json::to_string(&spec).unwrap();
    JobSpec::parse_yaml(&json).unwrap();
}

#[test]
fn defaults_are_the_same_for_absent_and_empty_retry_policy() {
    for retry in ["", "retry: {}\n", "retry: {onPreempt: {max: 2}}\n"] {
        let spec = JobSpec::parse_yaml(&format!("{MINIMAL}{retry}")).unwrap();
        assert_eq!(spec.shards.get(), 1);
        assert_eq!(spec.timeout, "60m");
        assert!(matches!(
            spec.retry.on_preempt,
            RetryLimit::Count { max: 2 }
        ));
        assert!(matches!(
            spec.retry.on_failure,
            RetryLimit::Count { max: 0 }
        ));
    }
}

#[test]
fn unsupported_or_ambiguous_execution_fields_fail_closed() {
    for invalid in [
        MINIMAL.replace("jobs/v0", "jobs/v9"),
        EXAMPLE.replace("hash(package-lock.json)", "hash(../outside)"),
        EXAMPLE.replace("hash(package-lock.json)", "unknown(package-lock.json)"),
        MINIMAL.replace("{image: ci}", "{image: ci, nix: {flake: ci}}"),
        MINIMAL.replace("{image: ci}", "{image: ci, futureFlag: 1}"),
        MINIMAL.replace("argv: [cargo, build]", "run: cargo, argv: [cargo]"),
        MINIMAL.replace("argv: [cargo, build]", "argv: []"),
        format!("{MINIMAL}resources: {{shm: 0Gi}}"),
        format!("{MINIMAL}shards: 0"),
        format!("{MINIMAL}env: {{THEGN_SHARD: other}}"),
        format!("{MINIMAL}caches: [{{key: k, paths: [dir], tool: nix}}]"),
        format!("{MINIMAL}artifacts: [../report]"),
        format!("{MINIMAL}timeout: 9999999999999999999h"),
        format!("{MINIMAL}typo: true"),
        format!("{MINIMAL}source: {{snapshot: {{hash: bad, sizeBytes: 1}}}}"),
        format!("{MINIMAL}retry: {{onFailure: {{max: 1, unexpected: true}}}}"),
    ] {
        assert!(JobSpec::parse_yaml(&invalid).is_err(), "{invalid}");
    }
}

#[test]
fn accepts_nix_environment_and_pinned_source_without_a_worktree() {
    let source = "source: {snapshot: {hash: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855, sizeBytes: 0}}";
    let input = MINIMAL.replace("{image: ci}", "{nix: {flake: '.#ci'}}");
    let spec = JobSpec::parse_yaml(&format!("{input}{source}\ncaches: [{{tool: nix}}]")).unwrap();
    assert!(spec.required_features().contains(&Feature::NixEnv));
}

#[test]
fn schemas_and_a_terminal_result_are_available_without_a_daemon() {
    use thegn_job_spec::{JobResult, job_schema, result_schema};
    let input = serde_json::to_value(job_schema()).unwrap();
    assert_eq!(input["additionalProperties"], false);
    assert_eq!(
        input["definitions"]["Digest"]["properties"]["hash"]["pattern"],
        "^[0-9a-f]{64}$"
    );
    assert!(input["properties"]["steps"]["minItems"].is_null());
    assert_eq!(
        input["definitions"]["Recipe"]["enum"],
        serde_json::json!(["cargo-build", "cargo-test"])
    );
    let output = serde_json::to_value(result_schema()).unwrap();
    assert!(output["properties"]["artifacts"].is_object());
    let published_job: serde_json::Value =
        serde_json::from_str(include_str!("../schemas/job-v0.json")).unwrap();
    let published_result: serde_json::Value =
        serde_json::from_str(include_str!("../schemas/result-v0.json")).unwrap();
    assert_eq!(input, published_job);
    assert_eq!(output, published_result);
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/rejected.json")).unwrap();
    let status = &fixture["status"];
    let shapes = output["definitions"]["JobStatus"]["oneOf"]
        .as_array()
        .unwrap();
    let rejected = shapes
        .iter()
        .find(|shape| shape["properties"]["kind"]["enum"][0] == status["kind"])
        .unwrap();
    for key in rejected["required"].as_array().unwrap() {
        assert!(
            status.get(key.as_str().unwrap()).is_some(),
            "missing status field {key}"
        );
    }
    let result: JobResult = serde_json::from_value(fixture.clone()).unwrap();
    assert!(result.shards.is_empty());
    assert_eq!(serde_json::to_value(result).unwrap(), fixture);
}

#[test]
fn a_structural_error_identifies_the_offending_field() {
    let invalid = format!("{MINIMAL}resources: {{arch: sparc}}");
    assert!(
        JobSpec::parse_yaml(&invalid)
            .unwrap_err()
            .to_string()
            .contains("resources.arch")
    );
}

#[test]
fn typed_selected_recipe_forwards_the_deadline_and_refuses_unhonoured_fields() {
    struct DeadlineRunner;
    impl SelectedRunner for DeadlineRunner {
        fn submit(
            &self,
            runner_alias: &str,
            _recipe: Recipe,
            source: &Digest,
            deadline: Duration,
            _cancel: &SelectedCancel,
        ) -> Result<SelectedJobResult, RunnerRefusal> {
            assert_eq!(runner_alias, "fixture-linux");
            assert_eq!(deadline, Duration::from_secs(5 * 60));
            Ok(SelectedJobResult {
                runner: runner_alias.to_owned(),
                recipe: Recipe::CargoBuild,
                source: source.clone(),
                completion: SelectedCompletion::Exited(0),
                log_tail: String::new(),
                truncated: false,
            })
        }
    }
    struct DeadlineDirectory;
    impl RunnerDirectory for DeadlineDirectory {
        fn resolve(&self, alias: &str) -> Option<&dyn SelectedRunner> {
            (alias == "fixture-linux").then_some(&DeadlineRunner)
        }
    }

    let spec = JobSpec::parse_yaml(&SELECTED_BUILD.replace("steps: []", "steps: []\ntimeout: 5m"))
        .unwrap();
    submit_selected(
        &spec,
        &Digest::of(b""),
        &DeadlineDirectory,
        &SelectedCancel::new(),
    )
    .unwrap();

    for ignored in [
        "env: {CI: 'true'}",
        "secrets: [TOKEN]",
        "services: [{name: db, image: postgres}]",
        "caches: [{tool: nix}]",
        "shards: 2",
        "resources: {cpu: 2}",
        "isolation: process",
        "placement: {spot: forbidden}",
        "artifacts: [target]",
        "results: {cargo: true}",
        "retry: {onFailure: {max: 1}}",
    ] {
        let input = SELECTED_BUILD.replace("steps: []", &format!("steps: []\n{ignored}"));
        assert!(JobSpec::parse_yaml(&input).is_err(), "{ignored}");
    }
    let excluded = SELECTED_BUILD.replace(
        "source:\n  snapshot:",
        "source:\n  exclude: [target]\n  snapshot:",
    );
    assert_ne!(excluded, SELECTED_BUILD);
    assert!(JobSpec::parse_yaml(&excluded).is_err());
}

#[test]
fn selected_runner_failure_after_submission_reads_back_as_indeterminate() {
    struct LostRunner;
    impl SelectedRunner for LostRunner {
        fn submit(
            &self,
            _runner_alias: &str,
            _recipe: Recipe,
            _source: &Digest,
            _deadline: Duration,
            _cancel: &SelectedCancel,
        ) -> Result<SelectedJobResult, RunnerRefusal> {
            Err(RunnerRefusal::RunnerUnavailable("transport dropped".into()))
        }
    }
    struct LostDirectory;
    impl RunnerDirectory for LostDirectory {
        fn resolve(&self, _: &str) -> Option<&dyn SelectedRunner> {
            Some(&LostRunner)
        }
    }

    let spec = JobSpec::parse_yaml(SELECTED_BUILD).unwrap();
    let failure = submit_selected(
        &spec,
        &Digest::of(b""),
        &LostDirectory,
        &SelectedCancel::new(),
    )
    .unwrap_err();
    assert_eq!(
        failure,
        SelectedFailure::Unproven(RunnerRefusal::RunnerUnavailable("transport dropped".into()))
    );
    let readback = selected_job_readback("job-lost", Err(failure));
    assert!(matches!(readback.status, JobStatus::Indeterminate { .. }));
    assert!(readback.summary.incomplete);
    assert!(readback.log_tail.is_empty());
}

#[test]
fn selected_readback_reports_a_bounded_and_truncated_log_tail() {
    let source = Digest::of(b"");
    let mut log_tail = String::new();
    log_tail.push_str(&"\u{20ac}".repeat(MAX_SELECTED_LOG_BYTES / 3 + 1));
    let result = SelectedJobResult {
        runner: "fixture-linux".into(),
        recipe: Recipe::CargoBuild,
        source: source.clone(),
        completion: SelectedCompletion::Exited(0),
        log_tail,
        truncated: false,
    };
    let readback = selected_job_readback("job-truncated", Ok(result));
    assert!(readback.truncated.log_tail);
    assert_eq!(readback.log_tail.len(), MAX_SELECTED_LOG_BYTES - 1);
    assert!(readback.log_tail.chars().all(|glyph| glyph == '\u{20ac}'));

    let complete = SelectedJobResult {
        runner: "fixture-linux".into(),
        recipe: Recipe::CargoBuild,
        source,
        completion: SelectedCompletion::Exited(0),
        log_tail: "short".into(),
        truncated: false,
    };
    let complete = selected_job_readback("job-complete", Ok(complete));
    assert!(!complete.truncated.log_tail);
    assert_eq!(complete.log_tail, "short");
}

#[test]
fn a_cancel_before_dispatch_reads_back_as_cancelled_without_calling_the_runner() {
    struct Panicking;
    impl SelectedRunner for Panicking {
        fn submit(
            &self,
            _: &str,
            _: Recipe,
            _: &Digest,
            _: Duration,
            _: &SelectedCancel,
        ) -> Result<SelectedJobResult, RunnerRefusal> {
            panic!("a cancelled submission must not reach the runner");
        }
    }
    struct Directory;
    impl RunnerDirectory for Directory {
        fn resolve(&self, alias: &str) -> Option<&dyn SelectedRunner> {
            (alias == "fixture-linux").then_some(&Panicking)
        }
    }
    let spec = JobSpec::parse_yaml(SELECTED_BUILD).unwrap();
    let source = Digest::of(b"");
    let cancel = SelectedCancel::new();
    cancel.cancel();
    let result = submit_selected(&spec, &source, &Directory, &cancel).unwrap();
    assert_eq!(result.completion, SelectedCompletion::Cancelled);
    let readback = selected_job_readback("job-cancelled", Ok(result));
    assert!(matches!(readback.status, JobStatus::Cancelled));
}

#[test]
fn a_cancel_during_the_runner_call_reaches_its_hook_and_reads_back_as_cancelled() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    struct Hooked {
        hooked: Arc<AtomicBool>,
        started: Mutex<Sender<()>>,
    }
    impl SelectedRunner for Hooked {
        fn submit(
            &self,
            runner_alias: &str,
            recipe: Recipe,
            source: &Digest,
            _: Duration,
            cancel: &SelectedCancel,
        ) -> Result<SelectedJobResult, RunnerRefusal> {
            let seen = self.hooked.clone();
            cancel.on_cancel(move || seen.store(true, Ordering::SeqCst));
            self.started.lock().unwrap().send(()).unwrap();
            // The cancel comes from outside this call; wait for its hook.
            let limit = Instant::now() + Duration::from_secs(10);
            while !self.hooked.load(Ordering::SeqCst) && Instant::now() < limit {
                std::thread::sleep(Duration::from_millis(2));
            }
            let completion = if self.hooked.load(Ordering::SeqCst) {
                SelectedCompletion::Cancelled
            } else {
                SelectedCompletion::Exited(0)
            };
            Ok(SelectedJobResult {
                runner: runner_alias.to_owned(),
                recipe,
                source: source.clone(),
                completion,
                log_tail: String::new(),
                truncated: false,
            })
        }
    }
    struct Directory(Hooked);
    impl RunnerDirectory for Directory {
        fn resolve(&self, alias: &str) -> Option<&dyn SelectedRunner> {
            (alias == "fixture-linux").then_some(&self.0 as &dyn SelectedRunner)
        }
    }
    let spec = JobSpec::parse_yaml(SELECTED_BUILD).unwrap();
    let source = Digest::of(b"");
    let (started_tx, started_rx) = channel();
    let directory = Directory(Hooked {
        hooked: Arc::new(AtomicBool::new(false)),
        started: Mutex::new(started_tx),
    });
    let cancel = SelectedCancel::new();
    let result = std::thread::scope(|scope| {
        let run = scope.spawn(|| submit_selected(&spec, &source, &directory, &cancel));
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the runner started");
        cancel.cancel();
        run.join().unwrap()
    })
    .unwrap();
    assert_eq!(result.completion, SelectedCompletion::Cancelled);

    // A hook registered after the cancel runs immediately.
    let late = SelectedCancel::new();
    late.cancel();
    let fired = Arc::new(AtomicBool::new(false));
    let flag = fired.clone();
    late.on_cancel(move || flag.store(true, Ordering::SeqCst));
    assert!(fired.load(Ordering::SeqCst));
}
