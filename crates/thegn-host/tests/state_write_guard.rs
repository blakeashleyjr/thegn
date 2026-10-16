//! THE-744: dependent test binaries must refuse writes to the live XDG DB.

use std::path::Path;

use thegn_core::db::Db;

fn isolated_state() -> thegn_core::testenv::IsolatedState {
    thegn_core::testenv::IsolatedState::new()
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
    let isolated = isolated_state();
    let state = isolated.state_home();
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
    let isolated = isolated_state();
    let path = isolated.root().join("elsewhere/x.db");

    assert!(Db::open_at(&path).is_ok());
}
