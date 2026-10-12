use super::Db;
use std::path::{Path, PathBuf};

fn isolated_state() -> Option<(tempfile::TempDir, crate::testenv::EnvGuard, PathBuf)> {
    let temp = tempfile::tempdir().unwrap();
    crate::fsperm::restrict_dir_to_owner(temp.path()).unwrap();
    let state = temp.path().join(".local/state");
    let home = temp.path().to_str().unwrap();
    let state_str = state.to_str().unwrap();
    let env = crate::testenv::EnvGuard::set(&[("HOME", home), ("XDG_STATE_HOME", state_str)]);
    // Windows resolves these paths through USERPROFILE and LOCALAPPDATA, so
    // skip if the test overrides did not isolate the DB from real user state.
    if crate::util::home() != temp.path() || crate::util::xdg_state_home() != state {
        return None;
    }
    Some((temp, env, state))
}

fn assert_guard_panic<T>(outcome: std::thread::Result<T>, path: &Path) {
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
fn open_refuses_test_write_under_real_state() {
    let Some((_temp, _env, state)) = isolated_state() else {
        return;
    };
    let outcome = std::panic::catch_unwind(Db::open);

    assert_guard_panic(outcome, &state.join("thegn/thegn.db"));
    assert!(
        !state.join("thegn").exists(),
        "the guard must run before creating the state directory"
    );
}

#[test]
fn open_at_refuses_test_write_under_real_state() {
    let Some((_temp, _env, state)) = isolated_state() else {
        return;
    };
    let path = state.join("thegn/probe.db");
    let outcome = std::panic::catch_unwind(|| Db::open_at(&path));

    assert_guard_panic(outcome, &path);
    assert!(
        !state.join("thegn").exists(),
        "the guard must run before creating the state directory"
    );
}

#[test]
fn read_only_open_does_not_trigger_the_write_guard() {
    let Some((_temp, _env, _state)) = isolated_state() else {
        return;
    };
    let outcome = std::panic::catch_unwind(Db::open_read_only);

    assert!(
        outcome.is_ok(),
        "read-only open must not trigger the write guard"
    );
    assert!(outcome.unwrap().unwrap().is_none());
}

#[test]
fn open_at_allows_a_distinct_path() {
    let Some((temp, _env, _state)) = isolated_state() else {
        return;
    };
    let path = temp.path().join("elsewhere/x.db");

    assert!(Db::open_at(&path).is_ok());
}
