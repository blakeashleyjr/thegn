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

const SPRITES_FIXTURE_DIR: &str = "THEGN_SPRITES_RACE_DIR";
const SPRITES_FIXTURE_NAME: &str = "THEGN_SPRITES_RACE_NAME";
const SPRITES_FIXTURE_CAP: &str = "THEGN_SPRITES_RACE_CAP";
const SPRITES_FIXTURE_OWNER: &str = "THEGN_SPRITES_RACE_OWNER";
const SPRITES_FIXTURE_ACCOUNT: &str = "THEGN_SPRITES_RACE_ACCOUNT";
const SPRITES_FIXTURE_MODE: &str = "THEGN_SPRITES_RACE_MODE";
const SPRITES_FIXTURE_GO: &str = "THEGN_SPRITES_RACE_GO";
const SPRITES_FIXTURE_REPORT: &str = "THEGN_SPRITES_RACE_REPORT";

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

#[test]
fn sprites_capacity_race_child_fixture() {
    let Ok(directory) = std::env::var(SPRITES_FIXTURE_DIR) else {
        return;
    };
    let name = std::env::var(SPRITES_FIXTURE_NAME).expect("sprites fixture child name is set");
    let cap = std::env::var(SPRITES_FIXTURE_CAP)
        .expect("sprites fixture cap is set")
        .parse::<usize>()
        .expect("sprites fixture cap is a usize");
    let owner = std::env::var(SPRITES_FIXTURE_OWNER).expect("sprites fixture owner is set");
    let account = std::env::var(SPRITES_FIXTURE_ACCOUNT).expect("sprites fixture account is set");
    let mode = std::env::var(SPRITES_FIXTURE_MODE).unwrap_or_default();
    let start = PathBuf::from(
        std::env::var(SPRITES_FIXTURE_GO).expect("sprites fixture start path is set"),
    );
    let report_dir = PathBuf::from(
        std::env::var(SPRITES_FIXTURE_REPORT).expect("sprites fixture report dir is set"),
    );
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
        let scope = thegn_svc::sprites::ledger::OwnerScope { owner, account };
        if mode == "adopt" {
            match thegn_svc::sprites::ledger::adopt_at(Path::new(&directory), &scope, &name, cap) {
                Ok(_) => "ok".to_owned(),
                Err(error) => sprites_reservation_error(error),
            }
        } else {
            match thegn_svc::sprites::ledger::reserve_at(Path::new(&directory), &scope, &name, cap)
            {
                Ok(mut guard) => match thegn_svc::sprites::ledger::commit_at(
                    Path::new(&directory),
                    &scope,
                    &name,
                ) {
                    Ok(()) => {
                        guard.preserve();
                        "ok".to_owned()
                    }
                    Err(error) => format!("err:{error:?}"),
                },
                Err(error) => sprites_reservation_error(error),
            }
        }
    } else {
        "timeout".to_owned()
    };
    let temporary_report = report_dir.join(format!("{name}.tmp"));
    let final_report = report_dir.join(&name);
    fs::write(&temporary_report, result).expect("write sprites fixture report");
    fs::rename(temporary_report, final_report).expect("publish sprites fixture report");
}

fn sprites_reservation_error(error: anyhow::Error) -> String {
    let debug = format!("{error:?}");
    if debug.contains("max_instances reached") {
        "atcap".to_owned()
    } else {
        format!("err:{debug}")
    }
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
    account: &'a str,
}

struct RaceEnv<'a> {
    directory: &'a str,
    name: &'a str,
    cap: &'a str,
    account: &'a str,
    start: &'a str,
    report: &'a str,
}

const CAPACITY_RACE_ENV: RaceEnv<'static> = RaceEnv {
    directory: FIXTURE_DIR,
    name: FIXTURE_NAME,
    cap: FIXTURE_CAP,
    account: FIXTURE_ACCOUNT,
    start: FIXTURE_GO,
    report: FIXTURE_REPORT,
};

const SPRITES_RACE_ENV: RaceEnv<'static> = RaceEnv {
    directory: SPRITES_FIXTURE_DIR,
    name: SPRITES_FIXTURE_NAME,
    cap: SPRITES_FIXTURE_CAP,
    account: SPRITES_FIXTURE_ACCOUNT,
    start: SPRITES_FIXTURE_GO,
    report: SPRITES_FIXTURE_REPORT,
};

fn shared_child_env<'a>(
    env: &'a [(&'a str, &'a str)],
    child_count: usize,
) -> Vec<&'a [(&'a str, &'a str)]> {
    vec![env; child_count]
}

fn run_children(
    fixture: RaceFixture<'_>,
    child_fixture: &str,
    env: RaceEnv<'_>,
    child_env: &[&[(&str, &str)]],
) -> Vec<(String, String)> {
    let executable = std::env::current_exe().expect("integration test executable is available");
    let mut children = Children(Vec::with_capacity(fixture.child_count));
    for index in 0..fixture.child_count {
        let name = format!("race-{index}");
        let stderr = fs::File::create(fixture.report_dir.join(format!("{name}.stderr")))
            .unwrap_or_else(|error| panic!("failed to create child {name} stderr file: {error}"));
        let child_env = child_env.get(index).copied().unwrap_or(&[]);
        let mut command = Command::new(&executable);
        command
            .args(["--exact", child_fixture, "--nocapture", "--test-threads=1"])
            .env(env.directory, fixture.directory)
            .env(env.name, &name)
            .env(env.cap, fixture.cap.to_string())
            .env(env.account, fixture.account)
            .env(env.start, fixture.start_file)
            .env(env.report, fixture.report_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        for (key, value) in child_env {
            command.env(key, value);
        }
        let child = command
            .spawn()
            .unwrap_or_else(|error| panic!("failed to spawn capacity race child {name}: {error}"));
        children.0.push(ChildProcess { name, child });
    }

    fs::write(fixture.start_file, b"go").expect("release all capacity race children together");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let missing: Vec<String> = children
            .0
            .iter()
            .filter(|process| !fixture.report_dir.join(&process.name).is_file())
            .map(|process| process.name.clone())
            .collect();
        if missing.is_empty() {
            break;
        }
        for process in &mut children.0 {
            if !fixture.report_dir.join(&process.name).is_file() {
                match process.child.try_wait() {
                    Ok(Some(status)) => panic!(
                        "capacity race child {} exited without a report: {status}; stderr: {}",
                        process.name,
                        child_stderr(fixture.report_dir, &process.name)
                    ),
                    Ok(None) => {}
                    Err(error) => panic!(
                        "failed checking child {} before its report: {error}; stderr: {}",
                        process.name,
                        child_stderr(fixture.report_dir, &process.name)
                    ),
                }
            }
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
            "capacity race child {} failed: {status}; stderr: {}",
            process.name,
            child_stderr(fixture.report_dir, &process.name)
        );
        let result =
            fs::read_to_string(fixture.report_dir.join(&process.name)).unwrap_or_else(|error| {
                panic!("failed reading child {} report: {error}", process.name)
            });
        results.push((process.name.clone(), result));
    }
    results
}

fn child_stderr(report_dir: &Path, name: &str) -> String {
    let path = report_dir.join(format!("{name}.stderr"));
    fs::read_to_string(&path)
        .unwrap_or_else(|error| format!("<failed to read {}: {error}>", path.display()))
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

fn sprites_record(name: &str, owner: &str, account: &str) -> VpsRecord {
    VpsRecord {
        name: name.to_owned(),
        provider: thegn_svc::sprites::ledger::LEDGER_PROVIDER.to_owned(),
        state: "ready".to_owned(),
        instance_id: name.to_owned(),
        ip: String::new(),
        created_at: 1,
        owner: owner.to_owned(),
        account: account.to_owned(),
    }
}

// Each test releases all independently spawned test-harness processes from one start file.
// reserve_at holds its cross-process file lock from occupancy count through atomic record write.
#[test]
fn capacity_race_six_processes_fill_a_ceiling_of_two() {
    let (_temporary, directory, start_file, report_dir) = temporary_paths();
    let extra_env = [(FIXTURE_SCOPE, "all")];
    let child_env = shared_child_env(&extra_env, 6);
    let results = run_children(
        RaceFixture {
            directory: &directory,
            start_file: &start_file,
            report_dir: &report_dir,
            child_count: 6,
            cap: 2,
            account: "",
        },
        "capacity_race_child_fixture",
        CAPACITY_RACE_ENV,
        &child_env,
    );

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
    let extra_env = [(FIXTURE_SCOPE, "hetzner")];
    let child_env = shared_child_env(&extra_env, 4);
    let results = run_children(
        RaceFixture {
            directory: &directory,
            start_file: &start_file,
            report_dir: &report_dir,
            child_count: 4,
            cap: 1,
            account: "",
        },
        "capacity_race_child_fixture",
        CAPACITY_RACE_ENV,
        &child_env,
    );

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
    let extra_env = [(FIXTURE_SCOPE, "all")];
    let child_env = shared_child_env(&extra_env, 4);
    let results = run_children(
        RaceFixture {
            directory: &directory,
            start_file: &start_file,
            report_dir: &report_dir,
            child_count: 4,
            cap: 1,
            account: "acct-a",
        },
        "capacity_race_child_fixture",
        CAPACITY_RACE_ENV,
        &child_env,
    );

    assert_outcomes(&results, 1, 3);
    assert_eq!(registry::list_at(&directory).len(), 3);
}

#[test]
fn sprites_ledger_race_six_processes_fill_a_cap_of_two() {
    let (_temporary, directory, start_file, report_dir) = temporary_paths();
    let extra_env = [
        (SPRITES_FIXTURE_OWNER, "owner-a"),
        (SPRITES_FIXTURE_MODE, "reserve"),
    ];
    let child_env = shared_child_env(&extra_env, 6);
    let results = run_children(
        RaceFixture {
            directory: &directory,
            start_file: &start_file,
            report_dir: &report_dir,
            child_count: 6,
            cap: 2,
            account: "acct-a",
        },
        "sprites_capacity_race_child_fixture",
        SPRITES_RACE_ENV,
        &child_env,
    );

    assert_outcomes(&results, 2, 4);
    let records = registry::list_at(&directory);
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| {
        record.provider == thegn_svc::sprites::ledger::LEDGER_PROVIDER
            && record.owner == "owner-a"
            && record.account == "acct-a"
            && record.state == "ready"
    }));
}

#[test]
fn sprites_ledger_race_other_account_records_do_not_count() {
    let (_temporary, directory, start_file, report_dir) = temporary_paths();
    registry::write_at(&directory, &sprites_record("other-0", "owner-a", "acct-b"))
        .expect("seed first other-account sprite record");
    registry::write_at(&directory, &sprites_record("other-1", "owner-a", "acct-b"))
        .expect("seed second other-account sprite record");
    // reserve_checked_at excludes records with a different non-empty account
    // from the occupancy count, so account a still has one slot at cap one.
    let extra_env = [
        (SPRITES_FIXTURE_OWNER, "owner-a"),
        (SPRITES_FIXTURE_MODE, "reserve"),
    ];
    let child_env = shared_child_env(&extra_env, 4);
    let results = run_children(
        RaceFixture {
            directory: &directory,
            start_file: &start_file,
            report_dir: &report_dir,
            child_count: 4,
            cap: 1,
            account: "acct-a",
        },
        "sprites_capacity_race_child_fixture",
        SPRITES_RACE_ENV,
        &child_env,
    );

    assert_outcomes(&results, 1, 3);
    let records = registry::list_at(&directory);
    assert_eq!(records.len(), 3);
    assert_eq!(
        records
            .iter()
            .filter(|record| record.account == "acct-a")
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.account == "acct-b")
            .count(),
        2
    );
}

#[test]
fn sprites_ledger_race_adopt_and_reserve_share_one_cap() {
    let (_temporary, directory, start_file, report_dir) = temporary_paths();
    let adopt_env = [
        (SPRITES_FIXTURE_OWNER, "owner-a"),
        (SPRITES_FIXTURE_MODE, "adopt"),
    ];
    let reserve_env = [
        (SPRITES_FIXTURE_OWNER, "owner-a"),
        (SPRITES_FIXTURE_MODE, "reserve"),
    ];
    let child_env = [
        &adopt_env[..],
        &adopt_env[..],
        &adopt_env[..],
        &reserve_env[..],
        &reserve_env[..],
        &reserve_env[..],
    ];
    let results = run_children(
        RaceFixture {
            directory: &directory,
            start_file: &start_file,
            report_dir: &report_dir,
            child_count: 6,
            cap: 1,
            account: "acct-a",
        },
        "sprites_capacity_race_child_fixture",
        SPRITES_RACE_ENV,
        &child_env,
    );

    assert_outcomes(&results, 1, 5);
    assert_eq!(registry::list_at(&directory).len(), 1);
}
