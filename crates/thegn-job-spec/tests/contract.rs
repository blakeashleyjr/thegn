use thegn_job_spec::{Feature, JobSpec, RetryLimit, byte_quantity, duration_ms};

const EXAMPLE: &str = include_str!("fixtures/web-e2e.yaml");
const MINIMAL: &str = "apiVersion: thegn.dev/jobs/v0\nkind: Job\nmetadata: {name: build}\nenvironment: {image: ci}\nsteps: [{name: build, argv: [cargo, build]}]\n";

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
    assert_eq!(input["properties"]["steps"]["minItems"], 1);
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
