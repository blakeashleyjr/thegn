use super::{ForgetArgs, ForgetEnv, ForgetReport, forget, run};
use crate::handlers::sidebar_persist::SIDEBAR_SCOPE;
use crate::testenv::IsolatedState;
use clap::Parser;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::issue::{AgentDispatchStatus, NewDispatch};
use thegn_core::models::{GroupTabRow, TabGroupRow};
use thegn_core::store::{NotificationStore, RepoTrustStore, WorkspaceStore, WorktreeAuxStore};

struct Seeded {
    repo: String,
    worktree: String,
    marker: PathBuf,
    dispatch: i64,
}

fn group(name: &str, worktree: &str) -> TabGroupRow {
    TabGroupRow {
        name: name.into(),
        kind: "branch".into(),
        worktree: worktree.into(),
        ordinal: 0,
        active_tab: 0,
    }
}

fn tab(name: &str) -> GroupTabRow {
    GroupTabRow {
        group_name: name.into(),
        ordinal: 0,
        title: "1".into(),
        pane_tree: r#"{"leaf":0}"#.into(),
        focused_pane: 0,
        pane_cwds: String::new(),
        pane_cmds: String::new(),
        pane_sessions: String::new(),
        scrollback_snapshot: String::new(),
    }
}

/// A registered workspace whose home checkout and branch worktree exist on
/// disk, with a marker file in the worktree, tab groups in two sessions, a
/// pin, a merge-queue row, a queued dispatch and a trust decision.
fn seed(db: &Db, root: &Path, slug: &str) -> Seeded {
    let repo_dir = root.join("repos").join(slug);
    let wt_dir = root.join("worktrees").join(format!("{slug}-feat"));
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::create_dir_all(&wt_dir).unwrap();
    let marker = wt_dir.join("marker.txt");
    std::fs::write(&marker, slug).unwrap();
    let repo = repo_dir.to_string_lossy().into_owned();
    let worktree = wt_dir.to_string_lossy().into_owned();
    db.put_workspace(&repo, slug, "repo").unwrap();
    assert_eq!(db.slug_for_repo(&repo, slug).unwrap(), slug);
    let name = format!("{slug}/feat");
    db.put_worktree(&name, &repo, &worktree, "feat", None, None)
        .unwrap();
    for session in ["s1", "s2"] {
        db.put_tab_group(session, &group(&name, &worktree)).unwrap();
        db.put_group_tab(session, &tab(&name)).unwrap();
    }
    db.set_ui_state(SIDEBAR_SCOPE, &format!("pin:{name}"), "1")
        .unwrap();
    db.enqueue_merge(&worktree, "feat", "main").unwrap();
    let dispatch = db
        .put_agent_dispatch(NewDispatch::new(
            &format!("linear:{slug}"),
            &worktree,
            "claude",
        ))
        .unwrap();
    db.repo_trust_decide(&repo, &format!("rid-{slug}"), "{}", "approved", 1)
        .unwrap();
    Seeded {
        repo,
        worktree,
        marker,
        dispatch,
    }
}

fn not_running() -> Option<String> {
    None
}

fn run_forget(db: &Db, state: &IsolatedState, args: &ForgetArgs) -> anyhow::Result<ForgetReport> {
    let state_dir = state.state_home().join("thegn");
    forget(
        db,
        args,
        &ForgetEnv {
            state_dir: &state_dir,
            stamp: "20261118T000000Z",
            running: &not_running,
            backup: None,
        },
    )
}

fn registered(db: &Db) -> Vec<String> {
    let mut paths: Vec<String> = db
        .workspaces()
        .unwrap()
        .into_iter()
        .map(|row| row.repo_path)
        .collect();
    paths.sort();
    paths
}

fn group_names(db: &Db, session: &str) -> Vec<String> {
    db.groups_for_session(session)
        .unwrap()
        .into_iter()
        .map(|row| row.name)
        .collect()
}

/// Every row of every table, for "nothing changed" comparisons.
fn dump(path: &Path) -> Vec<String> {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
        rows.collect::<rusqlite::Result<Vec<String>>>().unwrap()
    };
    let mut out = Vec::new();
    for table in tables {
        let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
        let columns = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let mut line = format!("{table}:");
            for index in 0..columns {
                write!(line, "{:?}|", row.get_ref(index).unwrap())
                    .expect("writing to String cannot fail");
            }
            out.push(line);
        }
    }
    out.sort();
    out
}

fn db_file(state: &IsolatedState) -> PathBuf {
    state.state_home().join("thegn").join("thegn.db")
}

fn source_db(state: &IsolatedState) -> (PathBuf, Seeded, Seeded, Seeded) {
    let path = state.root().join("source.db");
    let db = Db::open_at(&path).unwrap();
    let app = seed(&db, state.root(), "app");
    let lib = seed(&db, state.root(), "lib");
    let keep = seed(&db, state.root(), "keep");
    drop(db);
    (path, app, lib, keep)
}

#[test]
fn keep_form_forgets_every_other_workspace_and_leaves_the_kept_one_intact() {
    let state = IsolatedState::new();
    let db = Db::open().unwrap();
    let app = seed(&db, state.root(), "app");
    let lib = seed(&db, state.root(), "lib");
    let keep = seed(&db, state.root(), "keep");
    db.set_active_workspace(&app.repo).unwrap();

    let report = run_forget(
        &db,
        &state,
        &ForgetArgs {
            keep: vec![keep.repo.clone()],
            apply: true,
            ..ForgetArgs::default()
        },
    )
    .unwrap();

    assert!(report.applied);
    let mut forgotten: Vec<&str> = report
        .forgotten
        .iter()
        .map(|entry| entry.repo_path.as_str())
        .collect();
    forgotten.sort_unstable();
    let mut expected = vec![app.repo.as_str(), lib.repo.as_str()];
    expected.sort_unstable();
    assert_eq!(forgotten, expected);
    assert_eq!(report.kept.len(), 1);
    assert_eq!(report.kept[0].repo_path, keep.repo);
    for entry in &report.forgotten {
        assert_eq!(entry.counts.worktrees, 1);
        assert_eq!(entry.counts.tab_groups, 2);
        assert_eq!(entry.counts.pins, 1);
        assert_eq!(entry.counts.merge_queue, 1);
        assert_eq!(entry.counts.dispatches_abandoned, 1);
    }

    assert_eq!(registered(&db), vec![keep.repo.clone()]);
    for session in ["s1", "s2"] {
        assert_eq!(group_names(&db, session), vec!["keep/feat".to_string()]);
    }
    assert_eq!(db.active_workspace().unwrap(), None);
    for gone in [&app, &lib] {
        assert!(db.workspace_tombstoned(&gone.repo).unwrap());
        assert_eq!(db.repo_slug_lookup(&gone.repo).unwrap(), None);
        assert!(
            db.list_merge_queue()
                .unwrap()
                .iter()
                .all(|row| row.worktree != gone.worktree)
        );
        assert_eq!(
            db.get_dispatch(gone.dispatch).unwrap().unwrap().status,
            AgentDispatchStatus::Abandoned,
            "abandoned, not deleted"
        );
        assert_eq!(db.repo_trust_list(&gone.repo).unwrap().len(), 1);
        assert!(gone.marker.exists(), "files on disk are never touched");
    }
    assert!(
        db.get_ui_state(SIDEBAR_SCOPE, "pin:app/feat")
            .unwrap()
            .is_none()
    );

    // The kept workspace is fully intact.
    assert!(!db.workspace_tombstoned(&keep.repo).unwrap());
    assert_eq!(
        db.repo_slug_lookup(&keep.repo).unwrap(),
        Some("keep".into())
    );
    assert_eq!(
        db.get_ui_state(SIDEBAR_SCOPE, "pin:keep/feat").unwrap(),
        Some("1".into())
    );
    assert!(
        db.list_merge_queue()
            .unwrap()
            .iter()
            .any(|row| row.worktree == keep.worktree)
    );
    assert_eq!(
        db.get_dispatch(keep.dispatch).unwrap().unwrap().status,
        AgentDispatchStatus::Queued
    );
    assert!(keep.marker.exists());

    // The backup exists, opens, and carries the same schema version.
    let backup = PathBuf::from(report.backup.expect("backup path"));
    assert_eq!(
        backup,
        state
            .state_home()
            .join("thegn")
            .join("forget-backup-20261118T000000Z")
            .join("thegn.db")
    );
    let copy = Db::open_at(&backup).unwrap();
    assert_eq!(copy.user_version().unwrap(), db.user_version().unwrap());
    assert_eq!(copy.workspaces().unwrap().len(), 3, "pre-forget snapshot");
}

#[test]
fn positional_form_forgets_exactly_the_named_workspace() {
    let state = IsolatedState::new();
    let db = Db::open().unwrap();
    let app = seed(&db, state.root(), "app");
    let lib = seed(&db, state.root(), "lib");

    // A unique basename resolves like `thegn open` does.
    let report = run_forget(
        &db,
        &state,
        &ForgetArgs {
            paths: vec!["app".into()],
            apply: true,
            ..ForgetArgs::default()
        },
    )
    .unwrap();

    assert_eq!(report.forgotten.len(), 1);
    assert_eq!(report.forgotten[0].repo_path, app.repo);
    assert_eq!(registered(&db), vec![lib.repo.clone()]);
    assert!(app.marker.exists());
    assert!(lib.marker.exists());
}

#[test]
fn a_directory_inside_a_registered_workspace_resolves_to_it() {
    let state = IsolatedState::new();
    let db = Db::open().unwrap();
    let app = seed(&db, state.root(), "app");
    seed(&db, state.root(), "lib");
    let inner = Path::new(&app.repo).join("src");
    std::fs::create_dir_all(&inner).unwrap();

    let report = run_forget(
        &db,
        &state,
        &ForgetArgs {
            paths: vec![inner.to_string_lossy().into_owned()],
            ..ForgetArgs::default()
        },
    )
    .unwrap();
    assert_eq!(report.forgotten.len(), 1);
    assert_eq!(report.forgotten[0].repo_path, app.repo);
}

#[test]
fn dry_run_changes_nothing() {
    let state = IsolatedState::new();
    let db = Db::open().unwrap();
    let app = seed(&db, state.root(), "app");
    seed(&db, state.root(), "lib");
    let before = dump(&db_file(&state));

    let report = run_forget(
        &db,
        &state,
        &ForgetArgs {
            paths: vec![app.repo.clone()],
            ..ForgetArgs::default()
        },
    )
    .unwrap();

    assert!(!report.applied);
    assert_eq!(report.backup, None);
    assert_eq!(report.forgotten.len(), 1);
    assert_eq!(report.forgotten[0].counts.tab_groups, 2);
    assert_eq!(report.kept.len(), 1);
    assert_eq!(dump(&db_file(&state)), before);
    assert!(
        !state
            .state_home()
            .join("thegn")
            .join("forget-backup-20261118T000000Z")
            .exists()
    );
}

#[test]
fn a_keep_that_matches_nothing_refuses_and_changes_nothing() {
    let state = IsolatedState::new();
    let db = Db::open().unwrap();
    seed(&db, state.root(), "app");
    seed(&db, state.root(), "lib");
    let before = dump(&db_file(&state));

    let error = run_forget(
        &db,
        &state,
        &ForgetArgs {
            keep: vec!["/no/such/workspace".into()],
            apply: true,
            ..ForgetArgs::default()
        },
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("--keep"), "{error:#}");
    assert_eq!(dump(&db_file(&state)), before);
}

#[test]
fn both_forms_together_or_neither_refuse() {
    let state = IsolatedState::new();
    let db = Db::open().unwrap();
    let app = seed(&db, state.root(), "app");
    let lib = seed(&db, state.root(), "lib");

    let both = run_forget(
        &db,
        &state,
        &ForgetArgs {
            paths: vec![app.repo.clone()],
            keep: vec![lib.repo.clone()],
            apply: true,
            ..ForgetArgs::default()
        },
    )
    .unwrap_err();
    assert!(both.to_string().contains("not both"), "{both:#}");
    let neither = run_forget(
        &db,
        &state,
        &ForgetArgs {
            apply: true,
            ..ForgetArgs::default()
        },
    )
    .unwrap_err();
    assert!(neither.to_string().contains("--keep"), "{neither:#}");
    assert_eq!(registered(&db).len(), 2);

    // clap refuses the combination in both directions before anything runs.
    for argv in [
        vec!["thegn", "repo", "forget", "a", "--keep", "b"],
        vec!["thegn", "repo", "forget", "--keep", "b", "a"],
        vec!["thegn", "repo", "forget"],
    ] {
        assert!(
            crate::Cli::try_parse_from(argv.clone()).is_err(),
            "{argv:?}"
        );
    }
    assert!(crate::Cli::try_parse_from(["thegn", "repo", "forget", "a", "b"]).is_ok());
    assert!(
        crate::Cli::try_parse_from(["thegn", "repo", "forget", "--keep", "a", "--keep", "b"])
            .is_ok()
    );
    assert!(
        crate::Cli::try_parse_from(["thegn", "repo", "forget", "a", "--copy-from", "db"]).is_ok()
    );
}

#[test]
fn a_running_instance_refuses_and_changes_nothing() {
    let state = IsolatedState::new();
    let db = Db::open().unwrap();
    let app = seed(&db, state.root(), "app");
    let before = dump(&db_file(&state));
    let running = || Some("the pane daemon answers at /run/test.sock".to_string());
    let state_dir = state.state_home().join("thegn");

    let error = forget(
        &db,
        &ForgetArgs {
            paths: vec![app.repo.clone()],
            apply: true,
            ..ForgetArgs::default()
        },
        &ForgetEnv {
            state_dir: &state_dir,
            stamp: "20261118T000000Z",
            running: &running,
            backup: None,
        },
    )
    .unwrap_err();

    let text = error.to_string();
    assert!(text.contains("refusing"), "{text}");
    assert!(text.contains("/run/test.sock"), "{text}");
    assert!(text.contains("Nothing was changed"), "{text}");
    assert_eq!(dump(&db_file(&state)), before);
    assert!(!state_dir.join("forget-backup-20261118T000000Z").exists());
}

#[test]
fn copy_from_refuses_when_this_instance_has_a_db() {
    let state = IsolatedState::new();
    let db = Db::open().unwrap();
    let destination = db_file(&state);
    drop(db);

    let error = run(
        &Config::default(),
        ForgetArgs {
            paths: vec!["app".into()],
            copy_from: Some(state.root().join("source.db")),
            ..ForgetArgs::default()
        },
    )
    .unwrap_err();

    let text = format!("{error:#}");
    assert!(text.contains("already has a state database"), "{text}");
    assert!(text.contains(destination.to_str().unwrap()), "{text}");
}

#[test]
fn copy_from_refuses_same_file() {
    let state = IsolatedState::new();
    let db = Db::open().unwrap();
    let destination = db_file(&state);
    drop(db);

    let error = run(
        &Config::default(),
        ForgetArgs {
            paths: vec!["app".into()],
            copy_from: Some(destination.clone()),
            ..ForgetArgs::default()
        },
    )
    .unwrap_err();

    let text = format!("{error:#}");
    assert!(
        text.contains("source and destination are the same file"),
        "{text}"
    );
}

#[test]
fn copy_from_dry_run_writes_nothing() {
    let state = IsolatedState::new();
    let (source, app, _, _) = source_db(&state);
    let before = std::fs::metadata(&source).unwrap().modified().unwrap();

    run(
        &Config::default(),
        ForgetArgs {
            paths: vec![app.repo],
            copy_from: Some(source.clone()),
            ..ForgetArgs::default()
        },
    )
    .unwrap();

    assert!(!db_file(&state).exists());
    assert_eq!(
        std::fs::metadata(source).unwrap().modified().unwrap(),
        before
    );
}

#[test]
fn copy_from_apply_seeds_then_forgets() {
    let state = IsolatedState::new();
    let (source, app, lib, keep) = source_db(&state);

    run(
        &Config::default(),
        ForgetArgs {
            keep: vec![keep.repo.clone()],
            apply: true,
            copy_from: Some(source.clone()),
            ..ForgetArgs::default()
        },
    )
    .unwrap();

    let destination = Db::open_at(&db_file(&state)).unwrap();
    assert_eq!(registered(&destination), vec![keep.repo.clone()]);
    assert!(destination.workspace_tombstoned(&app.repo).unwrap());
    assert!(destination.workspace_tombstoned(&lib.repo).unwrap());

    let source = Db::open_read_only_current_at(&source).unwrap().unwrap();
    assert_eq!(registered(&source).len(), 3);
    assert_eq!(source.workspaces().unwrap().len(), 3);
}

#[test]
fn copy_from_refuses_a_running_source() {
    let state = IsolatedState::new();
    let (source, _, _, keep) = source_db(&state);
    let mut shm = source.clone().into_os_string();
    shm.push("-shm");
    std::fs::write(&shm, b"").unwrap();

    let error = run(
        &Config::default(),
        ForgetArgs {
            keep: vec![keep.repo.clone()],
            apply: true,
            copy_from: Some(source.clone()),
            ..ForgetArgs::default()
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("looks running"), "{error:#}");
    assert!(!db_file(&state).exists());
}

#[test]
fn copy_from_schema_mismatch_refuses() {
    let state = IsolatedState::new();
    let (source, _, _, _) = source_db(&state);
    let connection = rusqlite::Connection::open(&source).unwrap();
    connection
        .pragma_update(None, "user_version", thegn_core::db::SCHEMA_VERSION - 1)
        .unwrap();
    drop(connection);

    let error = run(
        &Config::default(),
        ForgetArgs {
            keep: vec!["keep".into()],
            copy_from: Some(source),
            ..ForgetArgs::default()
        },
    )
    .unwrap_err();

    let text = format!("{error:#}");
    assert!(text.contains("schema v"), "{text}");
    assert!(text.contains("Nothing was changed"), "{text}");
    assert!(!db_file(&state).exists());
}

#[test]
fn kept_workspace_lists_provider_worktrees() {
    let state = IsolatedState::new();
    let db = Db::open().unwrap();
    let keep = seed(&db, state.root(), "keep");
    let provider = state.root().join("provider").to_string_lossy().into_owned();
    let location = r#"{"control_prefix":["sprite","exec"],"path":"/workspace"}"#;
    db.put_worktree(
        "keep/provider",
        &keep.repo,
        &provider,
        "provider",
        Some(location),
        None,
    )
    .unwrap();

    let report = run_forget(
        &db,
        &state,
        &ForgetArgs {
            keep: vec![keep.repo.clone()],
            ..ForgetArgs::default()
        },
    )
    .unwrap();
    assert_eq!(report.kept[0].provider_worktrees, vec![provider]);
}
