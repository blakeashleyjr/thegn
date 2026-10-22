//! Keep the self-hosted CI runbook aligned with the workflow's executable
//! triggers, runner selector, and opt-in jobs.

use serde_yaml::Value;

const RUNNER_EXPRESSION: &str = "${{ fromJSON((github.event_name == 'workflow_dispatch' && github.repository == 'blakeashleyjr/thegn' && vars.CI_RUNS_ON) || '[\"ubuntu-latest\"]') }}";

fn mapping_value<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_mapping()?.get(Value::String(key.to_string()))
}

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn self_hosted_ci_runbook_matches_workflow_policy() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let workflow_path = root.join(".github/workflows/ci.yml");
    let workflow_text = std::fs::read_to_string(&workflow_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", workflow_path.display()));
    let workflow: Value = serde_yaml::from_str(&workflow_text)
        .unwrap_or_else(|error| panic!("cannot parse {}: {error}", workflow_path.display()));
    let runbook_path = root.join("docs/self-hosted-ci.md");
    let runbook = std::fs::read_to_string(&runbook_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", runbook_path.display()));
    let runbook = normalized(&runbook);

    // YAML 1.1 parsers may resolve the unquoted GitHub Actions key `on` as a
    // boolean. Accept either representation while still inspecting parsed YAML.
    let triggers =
        mapping_value(&workflow, "on").or_else(|| workflow.as_mapping()?.get(Value::Bool(true)));
    let triggers = triggers.expect("workflow must declare an `on` mapping");
    let trigger_names: Vec<&str> = triggers
        .as_mapping()
        .expect("workflow triggers must be a mapping")
        .keys()
        .filter_map(Value::as_str)
        .collect();
    let dispatch_only = trigger_names.len() == 1 && trigger_names[0] == "workflow_dispatch";
    if dispatch_only {
        assert!(
            runbook.contains("dispatch-only"),
            "workflow has no push/pull_request trigger, but runbook does not say dispatch-only"
        );
        assert!(
            runbook.contains("gh workflow run ci.yml --ref <branch>"),
            "dispatch-only workflow must document its manual run command"
        );
    } else {
        assert!(
            !runbook.contains("dispatch-only"),
            "runbook claims dispatch-only while push or pull_request is enabled"
        );
        for trigger in &trigger_names {
            assert!(
                runbook.contains(trigger),
                "enabled workflow trigger `{trigger}` is absent from the runbook"
            );
        }
    }
    assert!(
        runbook.contains("workflow_dispatch"),
        "runbook must name the workflow_dispatch event"
    );
    let dispatch_inputs = mapping_value(triggers, "workflow_dispatch")
        .and_then(|event| mapping_value(event, "inputs"))
        .expect("workflow_dispatch must declare its inputs");
    let extras_default = mapping_value(dispatch_inputs, "extras")
        .and_then(|input| mapping_value(input, "default"))
        .and_then(Value::as_bool)
        .expect("workflow_dispatch.extras must declare a boolean default");
    let extras_default_claim = if extras_default {
        "enabled by default"
    } else {
        "disabled by default"
    };
    assert!(
        runbook.contains(extras_default_claim),
        "runbook must describe extras jobs as {extras_default_claim}"
    );

    let jobs = mapping_value(&workflow, "jobs")
        .and_then(Value::as_mapping)
        .expect("workflow must declare jobs");
    let mut selected_linux_job = false;
    let mut optional_jobs = Vec::new();
    for (job_name, job) in jobs {
        let Some(job_name) = job_name.as_str() else {
            continue;
        };
        if let Some(runs_on) = mapping_value(job, "runs-on").and_then(Value::as_str)
            && runs_on.contains("CI_RUNS_ON")
        {
            selected_linux_job = true;
            assert_eq!(
                runs_on, RUNNER_EXPRESSION,
                "job `{job_name}` must use the exact documented fromJSON fallback"
            );
        }
        if mapping_value(job, "if")
            .and_then(Value::as_str)
            .is_some_and(|condition| condition.contains("inputs.extras"))
        {
            optional_jobs.push(job_name);
        }
    }
    assert!(
        selected_linux_job,
        "no job uses the CI_RUNS_ON runner selector"
    );
    assert!(
        runbook.contains(RUNNER_EXPRESSION),
        "runbook runner selector must exactly match the workflow fallback"
    );
    for job in &optional_jobs {
        assert!(
            runbook.contains(*job),
            "optional workflow job `{job}` must be named in the runbook"
        );
    }
    if !optional_jobs.is_empty() {
        assert!(
            runbook.contains("-f extras=true"),
            "runbook must explain how to enable jobs gated by inputs.extras"
        );
    }

    let opt_in = "gh variable set CI_RUNS_ON --body '[\"self-hosted\", \"ageless\"]'";
    let rollback = "gh variable delete CI_RUNS_ON";
    assert!(
        runbook.contains(opt_in),
        "missing exact self-hosted opt-in command"
    );
    let rollback_context = runbook
        .find("Roll back to the GitHub-hosted default")
        .expect("rollback command must have a clearly labelled rollback section");
    let rollback_command = runbook
        .find(rollback)
        .expect("missing exact rollback command");
    assert!(
        rollback_command > rollback_context,
        "rollback command must appear inside its labelled rollback section"
    );
    assert!(
        runbook.contains("(unset → GitHub-hosted ubuntu-latest)"),
        "unset selector fallback must be documented accurately"
    );
    assert!(
        runbook.contains("`NIX_GITHUB_TOKEN` is optional"),
        "runbook must state that NIX_GITHUB_TOKEN is optional"
    );

    for stale_claim in [
        "defaults to self-hosted",
        "default to self-hosted",
        "(unset → self-hosted)",
        "repo is **private**",
        "push a commit",
    ] {
        assert!(
            !runbook.contains(stale_claim),
            "runbook contains stale claim: {stale_claim}"
        );
    }
    assert!(
        runbook.contains("public") && runbook.contains("hostile"),
        "runbook must assume a public repository and hostile code"
    );
    for security_claim in [
        "Approval is a human gate",
        "isolated or disposable",
        "`/nix/store`",
        "sccache",
        "`CARGO_HOME`",
    ] {
        assert!(
            runbook.contains(security_claim),
            "runbook omits required security posture: {security_claim}"
        );
    }
}
