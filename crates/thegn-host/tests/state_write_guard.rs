//! THE-744: dependent test binaries must refuse writes to the live XDG DB.

use std::path::{Path, PathBuf};

use thegn_core::db::Db;

fn isolated_state() -> Option<(tempfile::TempDir, thegn_core::testenv::EnvGuard, PathBuf)> {
    let temp = tempfile::tempdir().unwrap();
    thegn_core::fsperm::restrict_dir_to_owner(temp.path()).unwrap();
    let state = temp.path().join(".local/state");
    let home = temp.path().to_str().unwrap();
    let state_home = state.to_str().unwrap();
    let env = thegn_core::testenv::EnvGuard::set(&[("HOME", home), ("XDG_STATE_HOME", state_home)]);
    // On Windows these helpers resolve through USERPROFILE and LOCALAPPDATA.
    // Skip when the overrides do not isolate state from the real user profile.
    if thegn_core::util::home() != temp.path() || thegn_core::util::xdg_state_home() != state {
        return None;
    }
    Some((temp, env, state))
}

fn assert_the744_panic(outcome: std::thread::Result<anyhow::Result<Db>>, path: &Path) {
    let panic = match outcome {
        Err(panic) => panic,
        Ok(_) => panic!("expected the THE-744 state write guard to panic"),
    };
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(message.contains("THE-744"), "unexpected panic: {message}");
    assert!(
        message.contains(&path.display().to_string()),
        "panic should name {}: {message}",
        path.display()
    );
}

#[test]
fn host_test_binary_refuses_state_db_write_before_creating_directory() {
    let Some((_temp, _env, state)) = isolated_state() else {
        return;
    };
    let path = state.join("thegn/probe.db");
    let outcome = std::panic::catch_unwind(|| Db::open_at(&path));

    assert_the744_panic(outcome, &path);
    assert!(
        !state.join("thegn").exists(),
        "the guard must run before creating the state directory"
    );
}

#[test]
fn host_test_binary_allows_a_distinct_database_path() {
    let Some((temp, _env, _state)) = isolated_state() else {
        return;
    };
    let path = temp.path().join("elsewhere/x.db");

    assert!(Db::open_at(&path).is_ok());
}
