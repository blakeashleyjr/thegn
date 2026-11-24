use super::*;
use std::path::Path;

const TOKEN_VAR: &str = "THEGN_TEST_JOBS_TOKEN";
const MISSING_VAR: &str = "THEGN_TEST_JOBS_TOKEN_NEVER_SET";

fn pc(provider: &str) -> EnvProviderConfig {
    EnvProviderConfig {
        provider: provider.into(),
        ..Default::default()
    }
}

fn sprites(id: &str, token_var: &str) -> EnvProviderConfig {
    EnvProviderConfig {
        id: id.into(),
        api_key_env: token_var.into(),
        ..pc("sprites")
    }
}

fn vercel(team: &str, project: &str, token_var: &str) -> EnvProviderConfig {
    EnvProviderConfig {
        team_id: team.into(),
        project_id: project.into(),
        api_key_env: token_var.into(),
        ..pc("vercel")
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

fn try_build(pc: &EnvProviderConfig, rt: &tokio::runtime::Runtime) -> Result<Runner, RunnerError> {
    build_from("ci", pc, "demo", Path::new("."), rt.handle().clone())
}

#[test]
fn an_explicit_request_beats_the_configured_runner() {
    let mut cfg = Config::default();
    assert_eq!(select_env(&cfg, None), Err(RunnerError::NotSelected));
    assert_eq!(select_env(&cfg, Some("  ")), Err(RunnerError::NotSelected));
    cfg.jobs.runner = " configured ".into();
    assert_eq!(select_env(&cfg, None), Ok("configured"));
    assert_eq!(select_env(&cfg, Some("asked")), Ok("asked"));
    assert_eq!(select_env(&cfg, Some("")), Ok("configured"));
}

#[test]
fn the_project_key_is_alias_safe() {
    assert_eq!(
        project_key(Path::new("/r/My_Repo.git")).as_deref(),
        Some("my-repo")
    );
    assert_eq!(
        project_key(Path::new("/r/--a  b--")).as_deref(),
        Some("a-b")
    );
    assert_eq!(project_key(Path::new("/r/___")), None);
    let long = format!("/r/{}", "x".repeat(200));
    assert_eq!(project_key(Path::new(&long)).map(|key| key.len()), Some(48));
}

#[test]
fn an_unknown_env_and_an_unsupported_provider_are_named() {
    let rt = runtime();
    let cfg = Config::default();
    let error = build(&cfg, "ghost", "demo", Path::new("."), rt.handle().clone())
        .err()
        .unwrap();
    assert_eq!(error, RunnerError::UnknownEnv("ghost".into()));
    assert!(error.to_string().contains("[env.ghost]"), "{error}");

    let error = try_build(&pc("fly"), &rt).err().unwrap();
    assert_eq!(
        error,
        RunnerError::UnsupportedProvider {
            env: "ci".into(),
            provider: "fly".into()
        }
    );
    assert!(
        error.to_string().contains("only sprites and vercel"),
        "{error}"
    );
}

#[test]
fn a_sprites_env_needs_one_fixed_sprite_before_any_token_is_resolved() {
    let rt = runtime();
    let error = try_build(&sprites("", MISSING_VAR), &rt).err().unwrap();
    assert_eq!(
        error,
        RunnerError::MissingBinding {
            env: "ci".into(),
            field: "id",
            what: "the one Sprite this project is bound to"
        }
    );
    assert!(
        error.to_string().contains("set id in [env.ci.provider]"),
        "{error}"
    );

    let error = try_build(&sprites("wt-{worktree}", MISSING_VAR), &rt)
        .err()
        .unwrap();
    assert!(
        matches!(error, RunnerError::InvalidBinding { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("template"), "{error}");
}

#[test]
fn a_vercel_env_needs_a_team_and_a_project_before_any_token_is_resolved() {
    let rt = runtime();
    let error = try_build(&vercel("", "prj_abc", MISSING_VAR), &rt)
        .err()
        .unwrap();
    assert!(
        matches!(
            error,
            RunnerError::MissingBinding {
                field: "team_id",
                ..
            }
        ),
        "{error}"
    );
    let error = try_build(&vercel("team_abc", " ", MISSING_VAR), &rt)
        .err()
        .unwrap();
    assert!(
        matches!(
            error,
            RunnerError::MissingBinding {
                field: "project_id",
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn a_missing_token_is_a_typed_error_that_names_the_reference_only() {
    let _state = crate::testenv::IsolatedState::new();
    let rt = runtime();
    for env in [
        sprites("ci-sprite", MISSING_VAR),
        vercel("team_abc", "prj_abc", MISSING_VAR),
    ] {
        let error = try_build(&env, &rt).err().unwrap();
        match &error {
            RunnerError::NoToken { env, detail } => {
                assert_eq!(env, "ci");
                assert!(detail.contains(MISSING_VAR), "{detail}");
            }
            other => panic!("expected NoToken, got {other:?}"),
        }
    }
}

#[test]
fn a_bound_sprites_env_builds_a_runner_with_the_sprite_alias() {
    let _state = crate::testenv::IsolatedState::with_env(&[(TOKEN_VAR, "sprites-secret")]);
    let rt = runtime();
    let runner = try_build(&sprites("ci-sprite", TOKEN_VAR), &rt)
        .ok()
        .unwrap();
    assert_eq!(runner.alias(), "sprites-ci-sprite");
    assert!(matches!(runner, Runner::Sprites(_)));
}

#[test]
fn a_bound_vercel_env_builds_a_runner_with_the_project_alias() {
    let _state = crate::testenv::IsolatedState::with_env(&[(TOKEN_VAR, "vercel-secret")]);
    let rt = runtime();
    let runner = try_build(&vercel("team_abc", "prj_abc", TOKEN_VAR), &rt)
        .ok()
        .unwrap();
    assert_eq!(runner.alias(), "vercel-demo");
    assert!(matches!(runner, Runner::Vercel(_)));
}

#[test]
fn no_error_message_carries_the_token_value() {
    let _state = crate::testenv::IsolatedState::with_env(&[(TOKEN_VAR, "super-secret-value")]);
    let rt = runtime();
    let error = try_build(&sprites("Not Valid", TOKEN_VAR), &rt)
        .err()
        .unwrap();
    assert!(!error.to_string().contains("super-secret-value"), "{error}");
}

#[test]
fn an_idle_ledger_has_no_pending_work_and_no_projects() {
    let _state = crate::testenv::IsolatedState::new();
    for kind in [EnvProviderKind::Sprites, EnvProviderKind::Vercel] {
        assert!(ledger_projects(kind).is_empty());
        assert!(!has_pending_work(kind, "demo"));
    }
    assert!(ledger_projects(EnvProviderKind::Fly).is_empty());
    assert!(!has_pending_work(EnvProviderKind::Fly, "demo"));
}

#[test]
fn an_unconfigured_runner_starts_no_reconcile() {
    // No runtime is entered: the early return must happen before
    // `Handle::current()`, which would panic here.
    spawn_startup_reconcile(&Config::default());
    let mut cfg = Config::default();
    cfg.jobs.runner = "ghost".into();
    spawn_startup_reconcile(&cfg);
}

#[test]
fn startup_reattach_skips_live_leases_and_rows_without_a_session() {
    use sprites_job::{AttemptState, OwnerLease};
    let now = thegn_core::util::now_ms();
    let live = OwnerLease::current(now);
    let mut dead = OwnerLease::current(now);
    dead.heartbeat_ms = now - 3_600_000;
    dead.boot_id = Some("some-other-boot".to_owned());
    let orphan = |state, session, lease: Option<&OwnerLease>| {
        is_orphaned_running(state, session, lease, now)
    };
    // A live owner (fresh heartbeat) is never touched.
    assert!(!orphan(AttemptState::Running, Some("s"), Some(&live)));
    // A dead owner, or a row with no lease at all, is reattached.
    assert!(orphan(AttemptState::Running, Some("s"), Some(&dead)));
    assert!(orphan(AttemptState::Running, Some("s"), None));
    // Nothing to follow without a session handle, and only `Running` rows count.
    assert!(!orphan(AttemptState::Running, None, None));
    assert!(!orphan(AttemptState::Dispatching, Some("s"), None));
    assert!(!orphan(AttemptState::Finished, Some("s"), None));
    assert!(orphaned_running(&[], now).is_empty());
}
