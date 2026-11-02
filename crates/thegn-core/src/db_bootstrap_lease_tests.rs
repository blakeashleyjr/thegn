use super::*;

fn stamp_version(path: &Path, version: i64) {
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "user_version", version).unwrap();
}

// The creator stamps the version before unlocking, then reacquires shared.
// If this thread wins exclusive in that gap, release it so the scoped creator
// can finish; only a Joined outcome proves both shared leases coexist.
#[test]
fn bootstrap_loser_waits_then_joins_the_created_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("thegn.db");
    let conn = Connection::open(&path).unwrap();
    let holder = open_schema_lock(&path).unwrap();
    let file = open_schema_lock(&path).unwrap();
    holder.try_lock().unwrap();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            stamp_version(&path, SCHEMA_VERSION);
            holder.unlock().unwrap();
            holder.lock_shared().unwrap();
        });

        let outcome = wait_for_bootstrap_lease(&file, &conn, &path, SCHEMA_BOOTSTRAP_WAIT).unwrap();
        match outcome {
            BootstrapLease::Joined(version) => {
                assert_eq!(version, SCHEMA_VERSION, "wait outcome: Joined({version})");
                let third = open_schema_lock(&path).unwrap();
                assert!(
                    matches!(third.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
                    "wait outcome: Joined({version}); the joined lease must remain shared while the creator holds its shared lease"
                );
            }
            BootstrapLease::Exclusive => {
                let version = query_user_version(&conn);
                file.unlock().unwrap();
                assert_eq!(
                    version, SCHEMA_VERSION,
                    "wait outcome: Exclusive; the creator stamped the completed schema before releasing its exclusive lease"
                );
            }
        }
    });
}

#[test]
fn bootstrap_loser_takes_over_when_the_creator_abandons() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("thegn.db");
    let conn = Connection::open(&path).unwrap();
    let holder = open_schema_lock(&path).unwrap();
    let file = open_schema_lock(&path).unwrap();
    holder.try_lock().unwrap();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            holder.unlock().unwrap();
        });

        assert_eq!(
            wait_for_bootstrap_lease(&file, &conn, &path, SCHEMA_BOOTSTRAP_WAIT).unwrap(),
            BootstrapLease::Exclusive
        );
    });
}

#[test]
fn bootstrap_wait_times_out_with_a_creation_message_not_old_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("thegn.db");
    let conn = Connection::open(&path).unwrap();
    let holder = open_schema_lock(&path).unwrap();
    let file = open_schema_lock(&path).unwrap();
    holder.try_lock().unwrap();
    let wait = std::time::Duration::from_millis(200);
    let start = std::time::Instant::now();
    let error = wait_for_bootstrap_lease(&file, &conn, &path, wait).unwrap_err();
    let elapsed = start.elapsed();
    let message = error.to_string();

    assert!(
        elapsed >= wait && elapsed < std::time::Duration::from_secs(5),
        "bootstrap timeout elapsed {elapsed:?} for requested wait {wait:?}"
    );
    assert!(
        message.contains("being created") && !message.contains("old schema"),
        "timeout after {elapsed:?} must describe creation, got: {message}"
    );
}

#[test]
fn older_schema_holder_still_gets_the_old_schema_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("thegn.db");
    stamp_version(&path, SCHEMA_VERSION - 1);
    let conn = Connection::open(&path).unwrap();
    let holder = open_schema_lock(&path).unwrap();
    let file = open_schema_lock(&path).unwrap();
    holder.lock_shared().unwrap();
    let start = std::time::Instant::now();
    let result =
        wait_for_bootstrap_lease(&file, &conn, &path, std::time::Duration::from_millis(500));
    let elapsed = start.elapsed();
    let error = result.unwrap_err();
    let message = error.to_string();

    assert!(
        message.contains("still using the old schema"),
        "existing older schema must keep its refusal, got: {message}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(400),
        "older-schema refusal should be immediate; elapsed {elapsed:?}, message: {message}"
    );
}

#[test]
fn existing_schema_conflict_keeps_the_immediate_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("thegn.db");
    stamp_version(&path, SCHEMA_VERSION - 1);
    let holder = open_schema_lock(&path).unwrap();
    let file = open_schema_lock(&path).unwrap();
    holder.lock_shared().unwrap();

    let error = try_exclusive_lock(&file, &path).unwrap_err();
    assert!(
        error.to_string().contains("still using the old schema"),
        "positive control must preserve the immediate refusal, got: {error}"
    );
}
