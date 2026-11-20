use super::*;
use crate::config::MapEnv;
use crate::info_model::{Shown, Source};
use crate::remote::GitLoc;
use crate::sandbox_pin::EnvSelection;
use crate::store::WorkspaceStore;
use std::collections::BTreeMap;
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    repo: PathBuf,
    worktree: PathBuf,
    config: PathBuf,
}

fn fixture(config: &str, repo_overlay: Option<&str>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("app");
    std::fs::create_dir(&repo).unwrap();
    let worktree = repo.join("feature");
    std::fs::create_dir(&worktree).unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config).unwrap();
    if let Some(overlay) = repo_overlay {
        std::fs::write(repo.join(".thegn.toml"), overlay).unwrap();
    }
    Fixture {
        _dir: dir,
        repo,
        worktree,
        config: config_path,
    }
}

fn loaded(fixture: &Fixture) -> Config {
    Config::load_layered(&MapEnv(BTreeMap::new()), &[], Some(fixture.config.clone()))
}

fn resolved(cfg: &Config, fixture: &Fixture, selection: &EnvSelection) -> crate::env::Environment {
    cfg.resolve_env_with(
        &fixture.repo,
        &GitLoc::Local(fixture.worktree.clone()),
        &fixture.worktree,
        selection,
        &Approvals::deny_all(),
    )
    .0
}

fn field<'a>(fields: &'a [Field], key: &str) -> &'a Field {
    fields.iter().find(|field| field.key == key).unwrap()
}

fn source<'a>(fields: &'a [Field], key: &str) -> &'a Source {
    field(fields, key).source.as_ref().unwrap()
}

#[test]
fn repo_network_is_reported_as_repo_source() {
    let _state = crate::testenv::IsolatedState::new();
    let fixture = fixture("", Some("[sandbox]\nnetwork = \"none\"\n"));
    let cfg = loaded(&fixture);
    let db = crate::db::Db::open_memory().unwrap();
    let env = resolved(&cfg, &fixture, &EnvSelection::none());
    let (fields, notes) = settings(
        &cfg,
        Some(fixture.config.clone()),
        &db,
        &fixture.repo,
        &fixture.worktree,
        &env,
    );

    assert!(notes.is_empty());
    assert!(
        matches!(source(&fields, "sandbox.network"), Source::Repo(path) if path.ends_with(".thegn.toml"))
    );
    assert_eq!(
        field(&fields, "sandbox.network").shown,
        Shown::Value("none".into())
    );
}

#[test]
fn backend_uses_global_or_builtin_source() {
    let _state = crate::testenv::IsolatedState::new();
    let global_fixture = fixture("[sandbox]\nbackend = \"podman\"\n", None);
    let global_cfg = loaded(&global_fixture);
    let global_db = crate::db::Db::open_memory().unwrap();
    let global_env = resolved(&global_cfg, &global_fixture, &EnvSelection::none());
    let (global_fields, _) = settings(
        &global_cfg,
        Some(global_fixture.config.clone()),
        &global_db,
        &global_fixture.repo,
        &global_fixture.worktree,
        &global_env,
    );
    assert_eq!(source(&global_fields, "sandbox.backend"), &Source::Global);

    let builtin_fixture = fixture("", None);
    let builtin_cfg = loaded(&builtin_fixture);
    let builtin_db = crate::db::Db::open_memory().unwrap();
    let builtin_env = resolved(&builtin_cfg, &builtin_fixture, &EnvSelection::none());
    let (builtin_fields, _) = settings(
        &builtin_cfg,
        Some(builtin_fixture.config.clone()),
        &builtin_db,
        &builtin_fixture.repo,
        &builtin_fixture.worktree,
        &builtin_env,
    );
    assert_eq!(source(&builtin_fields, "sandbox.backend"), &Source::Builtin);
}

#[test]
fn loaded_value_difference_is_not_misreported_as_global() {
    let _state = crate::testenv::IsolatedState::new();
    let fixture = fixture("[sandbox]\nbackend = \"podman\"\n", None);
    let cfg = Config::default();
    let db = crate::db::Db::open_memory().unwrap();
    let env = resolved(&cfg, &fixture, &EnvSelection::none());
    let (fields, _) = settings(
        &cfg,
        Some(fixture.config.clone()),
        &db,
        &fixture.repo,
        &fixture.worktree,
        &env,
    );

    assert_eq!(source(&fields, "sandbox.backend"), &Source::LoadedDiffers);
}

#[test]
fn worktree_and_workspace_environment_db_sources_win_in_order() {
    let _state = crate::testenv::IsolatedState::new();
    let fixture = fixture(
        "[env.x]\nplacement = \"local\"\n[env.y]\nplacement = \"local\"\n",
        None,
    );
    let cfg = loaded(&fixture);
    let db = crate::db::Db::open_memory().unwrap();
    db.put_workspace(&fixture.repo.to_string_lossy(), "app", "repo")
        .unwrap();
    db.set_workspace_env(&fixture.repo.to_string_lossy(), "y")
        .unwrap();

    let workspace_selection = db
        .effective_env_selection(
            &fixture.worktree.to_string_lossy(),
            &fixture.repo.to_string_lossy(),
        )
        .unwrap();
    let workspace_env = resolved(&cfg, &fixture, &workspace_selection);
    let (workspace_fields, _) = settings(
        &cfg,
        Some(fixture.config.clone()),
        &db,
        &fixture.repo,
        &fixture.worktree,
        &workspace_env,
    );
    assert_eq!(source(&workspace_fields, ENV_NAME), &Source::WorkspaceDb);

    db.set_worktree_env(&fixture.worktree.to_string_lossy(), "x")
        .unwrap();
    let worktree_selection = db
        .effective_env_selection(
            &fixture.worktree.to_string_lossy(),
            &fixture.repo.to_string_lossy(),
        )
        .unwrap();
    let worktree_env = resolved(&cfg, &fixture, &worktree_selection);
    let (worktree_fields, _) = settings(
        &cfg,
        Some(fixture.config.clone()),
        &db,
        &fixture.repo,
        &fixture.worktree,
        &worktree_env,
    );
    assert_eq!(source(&worktree_fields, ENV_NAME), &Source::WorktreeDb);
}

#[test]
fn unresolved_environment_name_is_warned() {
    let _state = crate::testenv::IsolatedState::new();
    let fixture = fixture("", None);
    let cfg = loaded(&fixture);
    let db = crate::db::Db::open_memory().unwrap();
    let env = resolved(&cfg, &fixture, &EnvSelection::named("missing"));
    let (fields, _) = settings(
        &cfg,
        Some(fixture.config.clone()),
        &db,
        &fixture.repo,
        &fixture.worktree,
        &env,
    );

    let environment = field(&fields, ENV_NAME);
    assert!(environment.warn);
    assert_eq!(
        environment.shown,
        Shown::Value("missing (no [env.missing]; runs locally)".into())
    );
}

#[test]
fn workspace_gate_is_selected_and_text_secrets_are_redacted() {
    let _state = crate::testenv::IsolatedState::new();
    let fixture = fixture(
        "[workspace.app.merge_queue]\ngate_command = \"TOKEN=abc just test\"\n",
        None,
    );
    let cfg = loaded(&fixture);
    let db = crate::db::Db::open_memory().unwrap();
    let env = resolved(&cfg, &fixture, &EnvSelection::none());
    let (fields, _) = settings(
        &cfg,
        Some(fixture.config.clone()),
        &db,
        &fixture.repo,
        &fixture.worktree,
        &env,
    );

    assert_eq!(
        source(&fields, "merge_queue.gate_command"),
        &Source::Workspace("app".into())
    );
    assert_eq!(
        field(&fields, "merge_queue.gate_command").shown,
        Shown::Value("TOKEN=***redacted*** just test".into())
    );
}

#[test]
fn provider_summary_excludes_api_credentials_and_commands() {
    let _state = crate::testenv::IsolatedState::new();
    let fixture = fixture(
        "[env.cloud]\nplacement = \"provider\"\n[env.cloud.provider]\nprovider = \"sprite\"\nid = \"box\"\ntemplate = \"dev\"\napi_key_env = \"TOKEN\"\nexec_command = [\"sprite\", \"exec\", \"--token\", \"abc\"]\n",
        None,
    );
    let cfg = loaded(&fixture);
    let db = crate::db::Db::open_memory().unwrap();
    let env = resolved(&cfg, &fixture, &EnvSelection::named("cloud"));
    let (fields, _) = settings(
        &cfg,
        Some(fixture.config.clone()),
        &db,
        &fixture.repo,
        &fixture.worktree,
        &env,
    );

    let Shown::Value(provider) = &field(&fields, ENV_PROVIDER).shown else {
        panic!("provider field should have a value");
    };
    assert!(provider.contains("provider=sprite"));
    assert!(!provider.contains("api_key_env"));
    assert!(!provider.contains("abc"));
    assert!(!provider.contains("exec_command"));
}

#[test]
fn worktree_environment_does_not_report_repo_hint_as_unapplied() {
    let _state = crate::testenv::IsolatedState::new();
    let fixture = fixture("[env.x]\nplacement = \"local\"\n", Some("env = \"x\"\n"));
    let cfg = loaded(&fixture);
    let db = crate::db::Db::open_memory().unwrap();
    db.set_worktree_env(&fixture.worktree.to_string_lossy(), "x")
        .unwrap();
    let selection = db
        .effective_env_selection(
            &fixture.worktree.to_string_lossy(),
            &fixture.repo.to_string_lossy(),
        )
        .unwrap();
    let env = resolved(&cfg, &fixture, &selection);
    let (_, notes) = settings(
        &cfg,
        Some(fixture.config.clone()),
        &db,
        &fixture.repo,
        &fixture.worktree,
        &env,
    );

    assert!(!notes.iter().any(|note| note.contains("was not applied")));
}
