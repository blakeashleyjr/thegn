//! Multi-process regression tests for the provider registry's atomic capacity reservation.
//!
//! Run with `cargo nextest run -p thegn-svc --test provider_capacity_race`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thegn_svc::vps::registry::{self, ReserveError, ReserveScope, VpsRecord};

const FIXTURE_DIR: &str = "THEGN_CAPACITY_RACE_DIR";
const FIXTURE_NAME: &str = "THEGN_CAPACITY_RACE_NAME";
const FIXTURE_CAP: &str = "THEGN_CAPACITY_RACE_CAP";
const FIXTURE_SCOPE: &str = "THEGN_CAPACITY_RACE_SCOPE";
const FIXTURE_ACCOUNT: &str = "THEGN_CAPACITY_RACE_ACCOUNT";
const FIXTURE_GO: &str = "THEGN_CAPACITY_RACE_GO";
const FIXTURE_REPORT: &str = "THEGN_CAPACITY_RACE_REPORT";

#[test]
fn capacity_race_child_fixture() {
    let Ok(directory) = std::env::var(FIXTURE_DIR) else {
        return;
    };
    let name = std::env::var(FIXTURE_NAME).expect("fixture child name is set");
    let cap = std::env::var(FIXTURE_CAP)
        .expect("fixture cap is set")
        .parse::<usize>()
        .expect("fixture cap is a usize");
    let scope = match std::env::var(FIXTURE_SCOPE)
        .expect("fixture scope is set")
        .as_str()
    {
        "all" => ReserveScope::All,
        "hetzner" => ReserveScope::Provider("hetzner"),
        other => panic!("unsupported fixture scope: {other}"),
    };
    let account = std::env::var(FIXTURE_ACCOUNT).expect("fixture account is set");
    let start = PathBuf::from(std::env::var(FIXTURE_GO).expect("fixture start path is set"));
    let report_dir =
        PathBuf::from(std::env::var(FIXTURE_REPORT).expect("fixture report dir is set"));
    let deadline = Instant::now() + Duration::from_secs(30);
    let started = loop {
        if start.exists() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(5));
    };

    let result = if started {
        let record = VpsRecord {
            name: name.clone(),
            provider: "hetzner".to_owned(),
            state: "creating".to_owned(),
            instance_id: String::new(),
            ip: String::new(),
            created_at: 1,
            owner: String::new(),
            account,
        };
        match registry::reserve_at(Path::new(&directory), &record, cap, scope) {
            Ok(()) => "ok".to_owned(),
            Err(ReserveError::AtCap { .. }) => "atcap".to_owned(),
            Err(error) => format!("err:{error:?}"),
        }
    } else {
        "timeout".to_owned()
    };
    let temporary_report = report_dir.join(format!("{name}.tmp"));
    let final_report = report_dir.join(&name);
    fs::write(&temporary_report, result).expect("write fixture report");
    fs::rename(temporary_report, final_report).expect("publish fixture report");
}

struct ChildProcess {
    name: String,
    child: Child,
}

struct Children(Vec<ChildProcess>);

impl Drop for Children {
    fn drop(&mut self) {
        for process in &mut self.0 {
            if let Ok(None) = process.child.try_wait()
                && let Err(error) = process.child.kill()
            {
                tracing::debug!(%error, child = %process.name, "capacity race child already gone");
            }
            if let Err(error) = process.child.wait() {
                tracing::debug!(%error, child = %process.name, "capacity race child not reaped");
            }
        }
    }
}

struct RaceFixture<'a> {
    directory: &'a Path,
    start_file: &'a Path,
    report_dir: &'a Path,
    child_count: usize,
    cap: usize,
    scope: &'a str,
    account: &'a str,
}

fn run_children(fixture: RaceFixture<'_>) -> Vec<(String, String)> {
    let executable = std::env::current_exe().expect("integration test executable is available");
    let mut children = Children(Vec::with_capacity(fixture.child_count));
    for index in 0..fixture.child_count {
        let name = format!("race-{index}");
        let child = Command::new(&executable)
            .args([
                "--exact",
                "capacity_race_child_fixture",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(FIXTURE_DIR, fixture.directory)
            .env(FIXTURE_NAME, &name)
            .env(FIXTURE_CAP, fixture.cap.to_string())
            .env(FIXTURE_SCOPE, fixture.scope)
            .env(FIXTURE_ACCOUNT, fixture.account)
            .env(FIXTURE_GO, fixture.start_file)
            .env(FIXTURE_REPORT, fixture.report_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("failed to spawn capacity race child {name}: {error}"));
        children.0.push(ChildProcess { name, child });
    }

    fs::write(fixture.start_file, b"go").expect("release all capacity race children together");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let missing: Vec<&str> = children
            .0
            .iter()
            .filter(|process| !fixture.report_dir.join(&process.name).is_file())
            .map(|process| process.name.as_str())
            .collect();
        if missing.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for capacity race child reports: {missing:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }

    let mut results = Vec::with_capacity(children.0.len());
    for process in &mut children.0 {
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = process.child.try_wait().unwrap_or_else(|error| {
                panic!("failed waiting for child {}: {error}", process.name)
            }) {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for capacity race child {}",
                process.name
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert!(
            status.success(),
            "capacity race child {} failed: {status}",
            process.name
        );
        let result =
            fs::read_to_string(fixture.report_dir.join(&process.name)).unwrap_or_else(|error| {
                panic!("failed reading child {} report: {error}", process.name)
            });
        results.push((process.name.clone(), result));
    }
    results
}

fn temporary_paths() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let temporary = tempfile::tempdir().expect("create private temporary directory");
    let directory = temporary.path().join("registry");
    let start_file = temporary.path().join("go");
    let report_dir = temporary.path().join("reports");
    fs::create_dir(&report_dir).expect("create separate report directory");
    (temporary, directory, start_file, report_dir)
}

fn record(name: &str, provider: &str, account: &str) -> VpsRecord {
    VpsRecord {
        name: name.to_owned(),
        provider: provider.to_owned(),
        state: "creating".to_owned(),
        instance_id: String::new(),
        ip: String::new(),
        created_at: 1,
        owner: String::new(),
        account: account.to_owned(),
    }
}

fn assert_outcomes(results: &[(String, String)], ok_count: usize, atcap_count: usize) {
    let mut ok_names = HashSet::new();
    let mut atcap: usize = 0;
    for (name, result) in results {
        match result.as_str() {
            "ok" => {
                assert!(
                    ok_names.insert(name.as_str()),
                    "duplicate child result for {name}"
                );
            }
            "atcap" => atcap += 1,
            other => panic!("child {name} returned unexpected reservation result {other}"),
        }
    }
    assert_eq!(ok_names.len(), ok_count);
    assert_eq!(atcap, atcap_count);
}

// Each test releases all independently spawned test-harness processes from one start file.
// reserve_at holds its cross-process file lock from occupancy count through atomic record write.
#[test]
fn capacity_race_six_processes_fill_a_ceiling_of_two() {
    let (_temporary, directory, start_file, report_dir) = temporary_paths();
    let results = run_children(RaceFixture {
        directory: &directory,
        start_file: &start_file,
        report_dir: &report_dir,
        child_count: 6,
        cap: 2,
        scope: "all",
        account: "",
    });

    assert_outcomes(&results, 2, 4);
    let records = registry::list_at(&directory);
    assert_eq!(records.len(), 2);
    let successful_names: HashSet<&str> = results
        .iter()
        .filter(|(_, outcome)| outcome == "ok")
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(
        records
            .iter()
            .all(|record| successful_names.contains(record.name.as_str()))
    );
}

// Two seeded Fly records are excluded by Provider("hetzner"); four children then
// contend for the sole remaining provider-scoped slot, independent of winner order.
#[test]
fn capacity_race_provider_scope_ignores_other_providers() {
    let (_temporary, directory, start_file, report_dir) = temporary_paths();
    registry::write_at(&directory, &record("other-0", "fly", "")).expect("seed first Fly record");
    registry::write_at(&directory, &record("other-1", "fly", "")).expect("seed second Fly record");
    let results = run_children(RaceFixture {
        directory: &directory,
        start_file: &start_file,
        report_dir: &report_dir,
        child_count: 4,
        cap: 1,
        scope: "hetzner",
        account: "",
    });

    assert_outcomes(&results, 1, 3);
    assert_eq!(registry::list_at(&directory).len(), 3);
}

// Account b's records are excluded for account a; the shared reservation lock then
// admits exactly one of four account-a children at capacity one.
#[test]
fn capacity_race_other_account_records_do_not_count() {
    let (_temporary, directory, start_file, report_dir) = temporary_paths();
    registry::write_at(&directory, &record("other-0", "hetzner", "acct-b"))
        .expect("seed first other-account record");
    registry::write_at(&directory, &record("other-1", "hetzner", "acct-b"))
        .expect("seed second other-account record");
    let results = run_children(RaceFixture {
        directory: &directory,
        start_file: &start_file,
        report_dir: &report_dir,
        child_count: 4,
        cap: 1,
        scope: "all",
        account: "acct-a",
    });

    assert_outcomes(&results, 1, 3);
    assert_eq!(registry::list_at(&directory).len(), 3);
}
