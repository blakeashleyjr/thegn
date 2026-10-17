use super::Db;
use std::path::Path;

fn isolated_state() -> crate::testenv::IsolatedState {
    crate::testenv::IsolatedState::with_real_home_layout()
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
    let isolated = isolated_state();
    let state = isolated.state_home();
    let outcome = std::panic::catch_unwind(Db::open);

    assert_guard_panic(outcome, &state.join("thegn/thegn.db"));
    assert!(
        !state.join("thegn").exists(),
        "the guard must run before creating the state directory"
    );
}

#[test]
fn open_at_refuses_test_write_under_real_state() {
    let isolated = isolated_state();
    let state = isolated.state_home();
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
    let _isolated = isolated_state();
    let outcome = std::panic::catch_unwind(Db::open_read_only);

    assert!(
        outcome.is_ok(),
        "read-only open must not trigger the write guard"
    );
    assert!(outcome.unwrap().unwrap().is_none());
}

#[test]
fn open_at_allows_a_distinct_path() {
    let isolated = isolated_state();
    let path = isolated.root().join("elsewhere/x.db");

    assert!(Db::open_at(&path).is_ok());
}
