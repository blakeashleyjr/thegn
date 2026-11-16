//! Durability of the managed SSH custody ledger (THE-1993): torn rows on both
//! read paths, a torn generation counter, and two real processes racing one
//! ledger directory.

use std::path::{Path, PathBuf};

use super::{
    AuthorizedKeyRecord, CustodyChanged, GENERATION_FILE, RotationRecovery, base64_unpadded,
    legacy_file_name, list_at, publish_if_unchanged_at, read_at, record_authorized_at,
    record_file_name, record_revoked_at, revoke_at, try_key_fingerprint,
};

fn row() -> AuthorizedKeyRecord {
    AuthorizedKeyRecord {
        provider: "fly".into(),
        account: "env:FLY_WORK".into(),
        instance: "tg-project-feature".into(),
        key_path: PathBuf::from("/state/ssh/fly-env-FLY-WORK_ed25519"),
        key_fingerprint: "SHA256:primary".into(),
        authorized_at: 1_700_000_000,
        generation: 7,
        proxy_worktree: None,
        rotation_recovery: None,
    }
}

/// Every on-disk shape a custody row takes: a current row, a row carrying
/// proxy context and an in-flight rotation, and a pre-generation row.
fn persisted_row_kinds() -> Vec<(&'static str, Vec<u8>)> {
    let recovering = AuthorizedKeyRecord {
        proxy_worktree: Some("/work/tg-project-feature".into()),
        rotation_recovery: Some(RotationRecovery {
            key_path: PathBuf::from("/state/ssh/fly-env-FLY-WORK_ed25519.next-0123"),
            key_fingerprint: "SHA256:replacement".into(),
            backup_key_path: Some(PathBuf::from(
                "/state/ssh/fly-env-FLY-WORK_ed25519.retiring-0123",
            )),
            authorized: true,
            verified: true,
            retiring_started: true,
            retiring_completed: false,
        }),
        ..row()
    };
    let mut pre_generation = serde_json::to_value(row()).unwrap();
    pre_generation
        .as_object_mut()
        .unwrap()
        .remove("generation")
        .unwrap();
    vec![
        ("current", serde_json::to_vec_pretty(&row()).unwrap()),
        (
            "recovering",
            serde_json::to_vec_pretty(&recovering).unwrap(),
        ),
        (
            "pre-generation",
            serde_json::to_vec(&pre_generation).unwrap(),
        ),
    ]
}

/// The decode failure of a truncated row, not a lookalike filesystem error.
fn is_truncated_row(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<serde_json::Error>()
            .is_some_and(serde_json::Error::is_eof)
    })
}

#[test]
fn every_torn_prefix_of_every_custody_row_is_a_typed_error_on_both_read_paths() {
    let expected = row();
    let (provider, account, instance) = (
        expected.provider.as_str(),
        expected.account.as_str(),
        expected.instance.as_str(),
    );
    for (kind, bytes) in persisted_row_kinds() {
        for (path_kind, name) in [
            ("canonical", record_file_name(provider, account, instance)),
            ("legacy", legacy_file_name(provider, account, instance)),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path();
            let path = dir.join(&name);

            std::fs::write(&path, &bytes).unwrap();
            let complete = read_at(dir, provider, account, instance)
                .unwrap()
                .expect("the complete row reads back");
            assert_eq!(
                (complete.provider, complete.account, complete.instance),
                (
                    provider.to_string(),
                    account.to_string(),
                    instance.to_string()
                ),
                "{kind} {path_kind} control row"
            );

            for len in 0..bytes.len() {
                let torn = &bytes[..len];
                std::fs::write(&path, torn).unwrap();
                let context = format!("{kind} {path_kind} row torn at {len}/{}", bytes.len());

                let read = read_at(dir, provider, account, instance).expect_err(&context);
                assert!(is_truncated_row(&read), "{context}: read {read:#}");
                let listed = list_at(dir).expect_err(&context);
                assert!(is_truncated_row(&listed), "{context}: list {listed:#}");
                let published = record_authorized_at(dir, &expected).expect_err(&context);
                assert!(
                    is_truncated_row(&published),
                    "{context}: publish {published:#}"
                );
                let swapped =
                    publish_if_unchanged_at(dir, &expected, &expected).expect_err(&context);
                assert!(is_truncated_row(&swapped), "{context}: swap {swapped:#}");
                let revoked =
                    record_revoked_at(dir, provider, account, instance).expect_err(&context);
                assert!(is_truncated_row(&revoked), "{context}: revoke {revoked:#}");

                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    torn,
                    "{context}: torn custody evidence must be preserved"
                );
            }
        }
    }
}

#[test]
fn torn_generation_counter_fails_closed_or_still_advances_the_row() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let mut current = record_authorized_at(dir, &row()).unwrap();
    while current.generation < 12 {
        current = record_authorized_at(dir, &row()).unwrap();
    }
    let counter = dir.join(GENERATION_FILE);
    let complete = std::fs::read(&counter).unwrap();
    assert_eq!(complete, b"12\n");

    for len in 0..complete.len() {
        let torn = &complete[..len];
        std::fs::write(&counter, torn).unwrap();
        let before = read_at(dir, "fly", "env:FLY_WORK", "tg-project-feature")
            .unwrap()
            .unwrap();
        let result = record_authorized_at(dir, &row());
        let context = format!("counter torn at {len}/{}", complete.len());
        if torn.is_empty() {
            let error = result.expect_err(&context);
            assert!(
                format!("{error:#}").contains("generation counter"),
                "{context}: {error:#}"
            );
            assert_eq!(
                read_at(dir, "fly", "env:FLY_WORK", "tg-project-feature")
                    .unwrap()
                    .unwrap(),
                before,
                "{context}: a refused publish leaves the row"
            );
        } else {
            let published = result.unwrap();
            assert!(
                published.generation > before.generation,
                "{context}: a short counter must not regress the row generation"
            );
        }
    }
}

// --- two-process races -------------------------------------------------------

const RACE_ROLE: &str = "THEGN_TEST_MANAGED_SSH_RACE_ROLE";
const RACE_DIR: &str = "THEGN_TEST_MANAGED_SSH_RACE_DIR";
const RACE_OUT: &str = "THEGN_TEST_MANAGED_SSH_RACE_OUT";
const RACE_GATE: &str = "THEGN_TEST_MANAGED_SSH_RACE_GATE";
const RACE_SNAPSHOT: &str = "THEGN_TEST_MANAGED_SSH_RACE_SNAPSHOT";
const RACE_CHILD: &str = "managed_ssh::durability_tests::custody_race_subprocess_child";
const RACE_ROUNDS: u64 = 25;

/// A syntactically valid ed25519 public key whose key bytes are all `seed`.
/// The 51-byte blob needs no base64 padding.
fn ed25519_public(seed: u8) -> String {
    let mut blob = vec![0, 0, 0, 11];
    blob.extend_from_slice(b"ssh-ed25519");
    blob.extend_from_slice(&[0, 0, 0, 32]);
    blob.extend_from_slice(&[seed; 32]);
    format!("ssh-ed25519 {} race-{seed}", base64_unpadded(&blob))
}

fn rotated(expected: &AuthorizedKeyRecord, seed: u8) -> AuthorizedKeyRecord {
    AuthorizedKeyRecord {
        key_path: PathBuf::from(format!("/state/ssh/rotated-{seed}")),
        key_fingerprint: try_key_fingerprint(&ed25519_public(seed)).unwrap(),
        rotation_recovery: None,
        ..expected.clone()
    }
}

// Only does work when re-executed by a race test below: the ledger lock and
// the generation counter are shared between processes, which threads in one
// process cannot exercise.
#[test]
fn custody_race_subprocess_child() {
    let Ok(role) = std::env::var(RACE_ROLE) else {
        return;
    };
    let dir = PathBuf::from(std::env::var_os(RACE_DIR).unwrap());
    let out = PathBuf::from(std::env::var_os(RACE_OUT).unwrap());
    let gate = std::fs::File::open(std::env::var_os(RACE_GATE).unwrap()).unwrap();
    // Blocks until the parent has started both children and opened the gate.
    gate.lock_shared().unwrap();
    gate.unlock().unwrap();
    let base = row();

    let report = match role.as_str() {
        "provision" => {
            let generations = (1..=RACE_ROUNDS)
                .map(|round| {
                    let next = AuthorizedKeyRecord {
                        authorized_at: base.authorized_at + i64::try_from(round).unwrap(),
                        ..base.clone()
                    };
                    record_authorized_at(&dir, &next).unwrap().generation
                })
                .collect::<Vec<_>>();
            serde_json::to_vec(&generations).unwrap()
        }
        "revoke" => {
            let removed = (0..RACE_ROUNDS)
                .map(|_| {
                    revoke_at(&dir, &base.provider, &base.account, &base.instance)
                        .unwrap()
                        .map(|record| record.generation)
                })
                .collect::<Vec<_>>();
            serde_json::to_vec(&removed).unwrap()
        }
        "rotate-1" | "rotate-2" => {
            let seed = if role == "rotate-1" { 1 } else { 2 };
            let snapshot: AuthorizedKeyRecord = serde_json::from_slice(
                &std::fs::read(std::env::var_os(RACE_SNAPSHOT).unwrap()).unwrap(),
            )
            .unwrap();
            // Round 0 starts from the parent's shared snapshot; later rounds
            // re-read the row like a retried rotation would.
            let mut attempts = Vec::new();
            for round in 0..RACE_ROUNDS {
                let expected = if round == 0 {
                    snapshot.clone()
                } else {
                    read_at(&dir, &base.provider, &base.account, &base.instance)
                        .unwrap()
                        .unwrap()
                };
                let won = match publish_if_unchanged_at(&dir, &expected, &rotated(&expected, seed))
                {
                    Ok(published) => Some(published.generation),
                    Err(error) => {
                        assert!(
                            error.downcast_ref::<CustodyChanged>().is_some(),
                            "{error:#}"
                        );
                        None
                    }
                };
                attempts.push((expected.generation, won));
            }
            serde_json::to_vec(&attempts).unwrap()
        }
        unknown => panic!("unknown custody race role {unknown}"),
    };
    std::fs::write(out, report).unwrap();
}

/// Start one child per role against `dir`, release them together, and return
/// each child's report.
fn race(dir: &Path, work: &Path, roles: [&str; 2], snapshot: Option<&Path>) -> [Vec<u8>; 2] {
    let gate_path = work.join("gate");
    let gate = std::fs::File::create(&gate_path).unwrap();
    gate.lock().unwrap();
    let children = roles.map(|role| {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", RACE_CHILD, "--nocapture"])
            .env(RACE_ROLE, role)
            .env(RACE_DIR, dir)
            .env(RACE_OUT, work.join(role))
            .env(RACE_GATE, &gate_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(snapshot) = snapshot {
            command.env(RACE_SNAPSHOT, snapshot);
        }
        (role, command.spawn().unwrap())
    });
    gate.unlock().unwrap();
    children.map(|(role, child)| {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "custody race child {role} failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::read(work.join(role))
            .unwrap_or_else(|error| panic!("child {role} never ran the race: {error}"))
    })
}

fn race_dirs(state: &crate::testenv::IsolatedState) -> (PathBuf, PathBuf) {
    let dir = state.root().join("ssh").join("authorized.d");
    let work = state.root().join("race");
    std::fs::create_dir_all(&work).unwrap();
    (dir, work)
}

#[test]
fn two_processes_provisioning_and_revoking_one_row_never_lose_or_duplicate_it() {
    let state = crate::testenv::IsolatedState::new();
    let (dir, work) = race_dirs(&state);
    let base = row();
    let seed = record_authorized_at(&dir, &base).unwrap();

    let [provisioned, revoked] = race(&dir, &work, ["provision", "revoke"], None);
    let provisioned: Vec<u64> = serde_json::from_slice(&provisioned).unwrap();
    let revoked: Vec<Option<u64>> = serde_json::from_slice(&revoked).unwrap();

    assert_eq!(provisioned.len() as u64, RACE_ROUNDS);
    assert!(
        provisioned
            .first()
            .is_some_and(|first| *first > seed.generation)
            && provisioned.windows(2).all(|pair| pair[0] < pair[1]),
        "every publish stamps a fresh, increasing generation: {provisioned:?}"
    );
    let removed = revoked.iter().flatten().copied().collect::<Vec<_>>();
    assert!(
        removed.windows(2).all(|pair| pair[0] < pair[1]),
        "no row is revoked twice and revocations follow publish order: {removed:?}"
    );
    for generation in &removed {
        assert!(
            *generation == seed.generation || provisioned.contains(generation),
            "revoked generation {generation} was never published"
        );
    }

    let last = *provisioned.last().unwrap();
    let rows = list_at(&dir).unwrap();
    match read_at(&dir, &base.provider, &base.account, &base.instance).unwrap() {
        Some(survivor) => {
            assert_eq!(survivor.generation, last, "only the last publish survives");
            assert_eq!(
                survivor.authorized_at,
                base.authorized_at + i64::try_from(RACE_ROUNDS).unwrap()
            );
            assert!(!removed.contains(&last), "a revoked row reappeared");
            assert_eq!(rows, vec![survivor]);
        }
        None => {
            assert!(
                removed.contains(&last),
                "the last publish vanished without a revoke"
            );
            assert!(rows.is_empty());
        }
    }
    let counter = std::fs::read_to_string(dir.join(GENERATION_FILE)).unwrap();
    assert!(counter.trim().parse::<u64>().unwrap() >= last);
}

#[test]
fn two_processes_rotating_one_row_have_one_winner_per_generation() {
    let state = crate::testenv::IsolatedState::new();
    let (dir, work) = race_dirs(&state);
    let base = row();
    let seed = record_authorized_at(&dir, &base).unwrap();
    let snapshot = work.join("snapshot.json");
    std::fs::write(&snapshot, serde_json::to_vec(&seed).unwrap()).unwrap();

    let reports = race(&dir, &work, ["rotate-1", "rotate-2"], Some(&snapshot));
    let attempts =
        reports.map(|report| serde_json::from_slice::<Vec<(u64, Option<u64>)>>(&report).unwrap());

    // Both processes compare against the same snapshot first: exactly one
    // may replace it.
    let first_round = attempts
        .iter()
        .map(|attempts| attempts[0])
        .collect::<Vec<_>>();
    assert!(
        first_round
            .iter()
            .all(|(expected, _)| *expected == seed.generation)
    );
    assert_eq!(
        first_round.iter().filter(|(_, won)| won.is_some()).count(),
        1,
        "one snapshot, one winner: {first_round:?}"
    );

    // Every successful swap replaced exactly the previous winner: no update
    // was lost and no generation was published twice.
    let mut wins = attempts
        .iter()
        .enumerate()
        .flat_map(|(child, attempts)| {
            attempts
                .iter()
                .filter_map(move |(expected, won)| won.map(|won| (won, *expected, child)))
        })
        .collect::<Vec<_>>();
    wins.sort_unstable();
    let mut previous = seed.generation;
    for (published, expected, _) in &wins {
        assert_eq!(*expected, previous, "lost update in chain {wins:?}");
        assert!(published > expected);
        previous = *published;
    }

    let (last, _, winner) = *wins.last().unwrap();
    let survivor = read_at(&dir, &base.provider, &base.account, &base.instance)
        .unwrap()
        .unwrap();
    assert_eq!(survivor.generation, last);
    let winner_seed = u8::try_from(winner + 1).unwrap();
    assert_eq!(
        survivor.key_fingerprint,
        try_key_fingerprint(&ed25519_public(winner_seed)).unwrap()
    );
    assert_eq!(list_at(&dir).unwrap(), vec![survivor]);
}
