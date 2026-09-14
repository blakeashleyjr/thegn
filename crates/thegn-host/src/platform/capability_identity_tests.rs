//! Private filesystem race fixtures for the diagnostic CLI identity seam.
use std::os::unix::fs::OpenOptionsExt;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn selected_file_replaced_with_fifo_is_refused_without_waiting_for_a_writer() {
    let dir = tempfile::tempdir().unwrap();
    let candidate = dir.path().join("devcontainer");
    std::fs::write(&candidate, "selected regular executable").unwrap();
    assert!(
        candidate.is_file(),
        "production discovery selected this path"
    );
    std::fs::remove_file(&candidate).unwrap();
    let cpath = std::ffi::CString::new(candidate.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: private owned path is NUL-terminated and valid through this call.
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    let (tx, rx) = mpsc::sync_channel(1);
    let path = candidate.clone();
    let worker = std::thread::spawn(move || {
        tx.send(crate::platform::open_capability_identity(&path))
            .unwrap();
    });
    let result = rx.recv_timeout(Duration::from_secs(1));
    // A counterfactual blocking File::open must fail the assertion, not strand
    // the suite. Opening both ends nonblocking releases that old reader even
    // if it reaches open after our deadline. Retain this rescue through join.
    let rescue = result.as_ref().err().map(|_| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&candidate)
            .unwrap()
    });
    worker.join().unwrap();
    drop(rescue);
    assert!(
        result
            .expect("identity open blocked on a swapped FIFO")
            .is_err()
    );
    dir.close().unwrap();
}

#[test]
fn executable_symlink_keeps_lexical_discovery_and_pins_regular_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("actual-cli");
    let link = dir.path().join("devcontainer");
    std::fs::write(&target, "fixture target").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let identity =
        same_file::Handle::from_file(crate::platform::open_capability_identity(&link).unwrap())
            .unwrap();
    assert_eq!(identity, same_file::Handle::from_path(&target).unwrap());
    assert!(identity.as_file().metadata().unwrap().is_file());
    drop(identity);
    dir.close().unwrap();
}
