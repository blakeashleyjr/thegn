use super::*;
use clap::Parser;
use thegn_svc::selected_job::Digest;

fn parse(args: &[&str]) -> Result<crate::Cli, clap::Error> {
    crate::Cli::try_parse_from(std::iter::once("thegn").chain(args.iter().copied()))
}

fn run_args(args: &[&str]) -> RunArgs {
    match parse(args).unwrap().command {
        Some(crate::Command::Job {
            action: Action::Run(args),
        }) => args,
        _ => panic!("not a job run: {args:?}"),
    }
}

fn result(completion: SelectedCompletion, log_tail: &str, truncated: bool) -> SelectedJobResult {
    SelectedJobResult {
        runner: "sprites-ci".into(),
        recipe: Recipe::CargoTest,
        source: Digest::of(b"tree"),
        completion,
        log_tail: log_tail.into(),
        truncated,
    }
}

#[test]
fn job_run_parses_a_recipe_with_defaults() {
    let args = run_args(&["job", "run", "cargo-test"]);
    assert_eq!(args.recipe, RecipeArg::CargoTest);
    assert_eq!(args.deadline, DEFAULT_DEADLINE_SECS);
    assert_eq!(args.runner, None);
    assert_eq!(args.recipe.recipe(), Recipe::CargoTest);
}

#[test]
fn job_run_parses_the_runner_and_deadline_flags() {
    let args = run_args(&[
        "job",
        "run",
        "--runner",
        "ci",
        "--deadline",
        "90",
        "cargo-build",
    ]);
    assert_eq!(args.recipe, RecipeArg::CargoBuild);
    assert_eq!(args.runner.as_deref(), Some("ci"));
    assert_eq!(args.deadline, 90);
    assert_eq!(args.recipe.recipe(), Recipe::CargoBuild);
}

#[test]
fn job_run_accepts_only_the_closed_recipe_set() {
    for argv in [
        &["job", "run"][..],
        &["job", "run", "make"][..],
        &["job", "run", "cargo-build", "--release"][..],
        &["job", "run", "cargo-build", "extra"][..],
        &["job", "run", "--deadline", "0", "cargo-build"][..],
        &["job", "run", "--deadline", "86401", "cargo-build"][..],
        &["job", "run", "--deadline", "soon", "cargo-build"][..],
    ] {
        assert!(parse(argv).is_err(), "must be refused: {argv:?}");
    }
}

#[test]
fn job_is_a_stable_channel_verb() {
    // Selected runners are Sprites and Vercel only, both stable-channel.
    let cli = parse(&["job", "run", "cargo-build"]).unwrap();
    assert_eq!(
        crate::experimental_command(cli.command.as_ref().unwrap()),
        None
    );
    // Fleet list/cost/inspect are stable (Sprites and Vercel envs only).
    let fleet = parse(&["fleet", "list"]).unwrap();
    assert_eq!(
        crate::experimental_command(fleet.command.as_ref().unwrap()),
        None
    );
    // The managed pool stays dev-only.
    let host = parse(&["host", "list"]).unwrap();
    assert_eq!(
        crate::experimental_command(host.command.as_ref().unwrap()),
        Some(("host", thegn_core::channel::Feature::Providers))
    );
}

#[test]
fn a_selected_job_refuses_to_start_another() {
    assert!(refuse_recursion(false).is_ok());
    let error = refuse_recursion(true).unwrap_err().to_string();
    assert!(error.contains("THEGN_SELECTED_JOB"), "{error}");
    assert!(error.contains("refusing"), "{error}");
}

#[test]
fn the_exit_code_follows_the_terminal_state() {
    assert_eq!(exit_code(SelectedCompletion::Exited(0)), 0);
    assert_eq!(exit_code(SelectedCompletion::Exited(101)), 101);
    assert_eq!(exit_code(SelectedCompletion::Exited(255)), 255);
    assert_eq!(exit_code(SelectedCompletion::Exited(256)), 1);
    assert_eq!(exit_code(SelectedCompletion::Exited(-1)), 1);
    assert_eq!(exit_code(SelectedCompletion::Cancelled), 130);
    assert_eq!(exit_code(SelectedCompletion::TimedOut), 124);
    assert_eq!(exit_code(SelectedCompletion::Signalled(9)), 137);
    assert_eq!(exit_code(SelectedCompletion::Signalled(1000)), 255);
    assert_eq!(exit_code(SelectedCompletion::Indeterminate), 1);
}

#[test]
fn the_receipt_names_completion_duration_cost_and_log() {
    let summary = AttemptSummary {
        attempt: 3,
        duration_ms: Some(12_345),
        spent_micros: Some(1_234),
        reserved_micros: 60_000,
    };
    let text = render_receipt(
        "ci",
        "sprites-ci",
        1800,
        &result(SelectedCompletion::Exited(3), "last line\n", false),
        Some(summary),
    );
    assert!(
        text.contains("job: cargo-test on sprites-ci (env ci)"),
        "{text}"
    );
    assert!(text.contains("completion: exited 3"), "{text}");
    assert!(text.contains("attempt: 3"), "{text}");
    assert!(text.contains("duration: 12.3s"), "{text}");
    assert!(text.contains("cost: ~$0.001234 (estimate)"), "{text}");
    assert!(text.ends_with("log:\nlast line"), "{text}");
    assert!(text.contains(&format!("source: {}", &Digest::of(b"tree").hash()[..12])));
}

#[test]
fn the_receipt_says_when_cost_and_log_are_missing_or_cut() {
    let unreported = AttemptSummary {
        attempt: 1,
        duration_ms: None,
        spent_micros: None,
        reserved_micros: 60_000,
    };
    let text = render_receipt(
        "ci",
        "vercel-demo",
        30,
        &result(SelectedCompletion::TimedOut, "", false),
        Some(unreported),
    );
    assert!(
        text.contains("completion: timed out (deadline 30s)"),
        "{text}"
    );
    assert!(text.contains("duration: unknown"), "{text}");
    assert!(
        text.contains("not reported (up to $0.060000 reserved)"),
        "{text}"
    );
    assert!(text.ends_with("log: (empty)"), "{text}");

    let text = render_receipt(
        "ci",
        "vercel-demo",
        30,
        &result(SelectedCompletion::Cancelled, "tail", true),
        None,
    );
    assert!(text.contains("cost: unknown (no ledger row)"), "{text}");
    assert!(text.contains("earlier output was dropped"), "{text}");
}

#[test]
fn remote_output_cannot_inject_terminal_controls() {
    let text = render_receipt(
        "ci",
        "sprites-ci",
        1800,
        &result(SelectedCompletion::Exited(1), "\u{1b}[2Jowned\n", false),
        None,
    );
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(text.contains("^[[2Jowned"), "{text:?}");
}

#[test]
fn job_route_parses_with_and_without_json() {
    for (args, want) in [
        (&["job", "route"][..], false),
        (&["job", "route", "--json"][..], true),
    ] {
        match parse(args).unwrap().command {
            Some(crate::Command::Job {
                action: Action::Route { json },
            }) => assert_eq!(json, want),
            _ => panic!("not a job route: {args:?}"),
        }
    }
}

#[test]
fn the_route_report_reads_project_then_global_then_local() {
    use thegn_core::config::{EnvConfig, EnvProviderConfig, WorkspaceConfig};
    let root = std::path::Path::new("/nonexistent/route-demo");
    let mut cfg = Config::default();
    let local = route_report(&cfg, root, "route-demo");
    assert_eq!(
        (local.destination.as_str(), local.source),
        ("local", "unset")
    );
    cfg.env.insert(
        "sp".into(),
        EnvConfig {
            provider: EnvProviderConfig {
                provider: "sprites".into(),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    cfg.jobs.runner = "sp".into();
    let global = route_report(&cfg, root, "route-demo");
    assert_eq!(
        (global.destination.as_str(), global.source),
        ("sprites", "global")
    );
    assert_eq!(global.status, "ready");
    cfg.workspace.insert(
        thegn_core::config::workspace_slug(root),
        WorkspaceConfig {
            jobs: thegn_core::config::JobsOverlay {
                runner: Some("missing".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let bound = route_report(&cfg, root, "route-demo");
    assert_eq!(bound.source, "project");
    assert!(bound.status.starts_with("invalid"), "{bound:?}");
    assert!(render_route(&bound).contains("reason: project binding"));
}

#[test]
fn an_unavailable_runner_refusal_names_the_bound_and_the_local_escape() {
    let text = refusal_text(
        &RunnerRefusal::RunnerUnavailable("sprites-ci".into()),
        Duration::from_secs(30),
    );
    assert!(text.contains("THEGN_JOB_LOCAL=1"), "{text}");
    assert!(text.contains("30s"), "{text}");
    let other = refusal_text(&RunnerRefusal::WrongRunner, Duration::from_secs(30));
    assert!(!other.contains("THEGN_JOB_LOCAL"), "{other}");
}

#[test]
fn a_not_started_refusal_reads_as_a_plain_failure() {
    let text = refusal_text(
        &RunnerRefusal::NotStarted("sprites-ci: no token".into()),
        Duration::from_secs(30),
    );
    assert!(text.starts_with("job not started: "), "{text}");
    assert!(!text.contains("unproven"), "{text}");
    assert!(!text.contains("may have run"), "{text}");
}
