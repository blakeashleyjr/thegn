//! Fault injection for the whole managed-SSH rotation transaction (THE-1993).
//!
//! A stand-in remote runs the real authorize/de-authorize scripts with `/bin/sh`
//! against one private `$HOME` per instance and authenticates every command
//! with the key held in the identity file, as ssh would. Each scenario stops
//! the rotation after one durable phase (a simulated process crash) or fails
//! one remote command, resumes the way the next `thegn secret ssh rotate`
//! would, and then requires exactly one managed key authorized on every
//! instance, a ledger without recovery state that names that key, and a
//! matching local pair.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use thegn_core::managed_ssh::AuthorizedKeyRecord;

use super::{RotationRemote, public_path, rotate_scope};

const INSTANCES: [&str; 2] = ["alpha", "beta"];
const PRIVATE_PREFIX: &str = "fake-private ";
const OPERATOR_KEY: &str = "ssh-ed25519 AAAAOPERATORKEY operator@example";
const OPERATOR_BODY: &str = "AAAAOPERATORKEY";

/// Every phase the rotation itself reports, in order.
const ROTATION_PHASES: [&str; 10] = [
    "plan-recorded",
    "replacement-authorized",
    "authorization-recorded",
    "replacement-verified",
    "verification-recorded",
    "retirement-recorded",
    "pair-promoted",
    "old-key-removed",
    "retirement-completion-recorded",
    "custody-completed",
];

/// Every phase an interrupted rotation's recovery reports.
const RECOVERY_PHASES: [&str; 5] = [
    "recovery-primary-restored",
    "recovery-auxiliary-removed",
    "recovery-local-restored",
    "recovery-cleared",
    "recovery-completed-forward",
];

/// Phases after which the replacement is canonical and the rotation must
/// complete forward rather than roll back.
const FORWARD_PHASES: [&str; 2] = ["retirement-completion-recorded", "custody-completed"];

/// Crashes after the last ledger row is cleaned and before the retired or
/// auxiliary local pair is deleted leave that pair on disk with no custody
/// row. It is never authorized anywhere (asserted before anything else runs),
/// and the next uninterrupted rotation of the key deletes it (asserted by
/// [`assert_next_rotation_sweeps_orphans`]).
const ORPHAN_PAIR_PHASES: [&str; 3] = [
    "custody-completed",
    "recovery-cleared",
    "recovery-completed-forward",
];

#[derive(Clone, Copy, Debug)]
enum Fault {
    None,
    /// Stop the process at the nth phase boundary (1-based).
    CrashAtPhase(usize),
    /// Fail the nth remote command (1-based) before it runs.
    FailBeforeRun(usize),
    /// Run the nth remote command, then lose its success response.
    FailAfterRun(usize),
}

/// Panic payload standing in for a process that died at a phase boundary.
struct SimulatedCrash;

struct FakeTransport {
    home: PathBuf,
    identity: PathBuf,
}

struct FakeRemote {
    homes: PathBuf,
    fault: Fault,
    phases: Cell<usize>,
    runs: Cell<usize>,
    next_seed: Cell<u8>,
    fired: RefCell<Option<String>>,
}

impl FakeRemote {
    fn new(homes: &Path, fault: Fault, first_seed: u8) -> Self {
        Self {
            homes: homes.to_path_buf(),
            fault,
            phases: Cell::new(0),
            runs: Cell::new(0),
            next_seed: Cell::new(first_seed),
            fired: RefCell::new(None),
        }
    }

    fn fired(&self) -> Option<String> {
        self.fired.borrow().clone()
    }

    fn fire(&self, what: &str) {
        *self.fired.borrow_mut() = Some(what.to_string());
    }
}

impl RotationRemote for FakeRemote {
    type Transport = FakeTransport;

    fn connect(
        &self,
        record: &AuthorizedKeyRecord,
        key: &Path,
        _public: &str,
        _label: &str,
    ) -> Result<FakeTransport> {
        Ok(FakeTransport {
            home: self.homes.join(&record.instance),
            identity: key.to_path_buf(),
        })
    }

    fn run(
        &self,
        transport: &FakeTransport,
        record: &AuthorizedKeyRecord,
        script: &str,
        operation: &str,
    ) -> Result<()> {
        let index = self.runs.get() + 1;
        self.runs.set(index);
        if let Fault::FailBeforeRun(target) = self.fault
            && target == index
        {
            self.fire(operation);
            bail!("{operation}: injected transport failure before the command ran");
        }
        authenticate(transport)?;
        let output = run_sh(&transport.home, script)?;
        if !output.status.success() {
            bail!(
                "{operation} failed for {} (exit {}): {}",
                record.instance,
                output.status.code().unwrap_or(255),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if let Fault::FailAfterRun(target) = self.fault
            && target == index
        {
            self.fire(operation);
            bail!("{operation}: injected loss of the response after the command ran");
        }
        Ok(())
    }

    fn verify(&self, record: &AuthorizedKeyRecord, key: &Path, public: &str) -> Result<()> {
        let transport = self.connect(record, key, public, "replacement-key provider")?;
        self.run(&transport, record, "true", "verify replacement key")
    }

    fn generate_keypair(&self, key: &Path) -> Result<()> {
        let seed = self.next_seed.get();
        self.next_seed.set(seed.wrapping_add(1));
        if key.exists() || public_path(key).exists() {
            bail!("refusing to overwrite keypair at {}", key.display());
        }
        let public = ed25519_public(seed);
        std::fs::write(key, format!("{PRIVATE_PREFIX}{}\n", key_body(&public)))?;
        std::fs::write(public_path(key), format!("{public}\n"))?;
        Ok(())
    }

    fn phase(&self, name: &'static str) {
        let index = self.phases.get() + 1;
        self.phases.set(index);
        if let Fault::CrashAtPhase(target) = self.fault
            && target == index
        {
            self.fire(name);
            std::panic::panic_any(SimulatedCrash);
        }
    }
}

/// A syntactically valid ed25519 public key whose 32 key bytes are `seed`.
fn ed25519_public(seed: u8) -> String {
    let mut blob = vec![0, 0, 0, 11];
    blob.extend_from_slice(b"ssh-ed25519");
    blob.extend_from_slice(&[0, 0, 0, 32]);
    blob.extend_from_slice(&[seed; 32]);
    format!(
        "ssh-ed25519 {} thegn-test-{seed}",
        base64::engine::general_purpose::STANDARD.encode(blob)
    )
}

fn key_body(public: &str) -> String {
    public
        .split_whitespace()
        .nth(1)
        .expect("public key has a body")
        .to_string()
}

/// ssh authenticates with whatever key the identity file holds, so the
/// stand-in does too: a missing file or an unauthorized key is refused.
fn authenticate(transport: &FakeTransport) -> Result<()> {
    let private = std::fs::read_to_string(&transport.identity).map_err(|error| {
        anyhow!(
            "identity {} is unreadable: {error}",
            transport.identity.display()
        )
    })?;
    let body = private
        .strip_prefix(PRIVATE_PREFIX)
        .map(str::trim)
        .ok_or_else(|| anyhow!("identity {} is malformed", transport.identity.display()))?;
    if !authorized_bodies(&transport.home).iter().any(|b| b == body) {
        bail!("Permission denied (publickey) for {body}");
    }
    Ok(())
}

/// Key bodies on non-comment `authorized_keys` lines, in file order.
fn authorized_bodies(home: &Path) -> Vec<String> {
    let path = home.join(".ssh").join("authorized_keys");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            fields
                .windows(2)
                .find(|pair| pair[0] == "ssh-ed25519")
                .map(|pair| pair[1].to_string())
        })
        .collect()
}

#[expect(
    clippy::disallowed_methods,
    reason = "test runs the generated remote script with a local sh against a private HOME"
)]
fn run_sh(home: &Path, script: &str) -> Result<std::process::Output> {
    Ok(std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .envs(thegn_core::testenv::state_env_all(home))
        .output()?)
}

struct Fixture {
    _state: thegn_core::testenv::IsolatedState,
    keys: PathBuf,
    key_path: PathBuf,
    homes: PathBuf,
    old_body: String,
}

/// Two instances authorized with one managed key in an isolated ledger.
/// `None` when the host has no `/bin/sh` to stand in for the remote.
fn fixture() -> Option<Fixture> {
    if !Path::new("/bin/sh").exists() {
        return None;
    }
    let state = thegn_core::testenv::IsolatedState::new();
    let keys = state.root().join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    // Managed key directories are owner-only (`generate_managed_keypair_at`).
    thegn_core::fsperm::restrict_dir_to_owner(&keys).unwrap();
    let key_path = keys.join("managed_ed25519");
    let homes = state.root().join("remotes");
    FakeRemote::new(&homes, Fault::None, 0)
        .generate_keypair(&key_path)
        .unwrap();
    let old_public = std::fs::read_to_string(public_path(&key_path))
        .unwrap()
        .trim()
        .to_string();
    for instance in INSTANCES {
        let ssh = homes.join(instance).join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("authorized_keys"),
            format!("{OPERATOR_KEY}\n{old_public}\n"),
        )
        .unwrap();
        thegn_core::managed_ssh::record_authorized_at(
            &thegn_core::managed_ssh::dir(),
            &AuthorizedKeyRecord {
                provider: "fake".to_string(),
                account: "acct".to_string(),
                instance: instance.to_string(),
                key_path: key_path.clone(),
                key_fingerprint: thegn_core::managed_ssh::key_fingerprint(&old_public),
                authorized_at: 1,
                generation: 0,
                proxy_worktree: None,
                rotation_recovery: None,
            },
        )
        .unwrap();
    }
    Some(Fixture {
        _state: state,
        keys,
        key_path,
        homes,
        old_body: key_body(&old_public),
    })
}

fn ledger() -> Vec<AuthorizedKeyRecord> {
    thegn_core::managed_ssh::list().unwrap()
}

fn recovery_pending() -> bool {
    ledger()
        .iter()
        .any(|record| record.rotation_recovery.is_some())
}

enum Run {
    Completed(Result<()>),
    Crashed,
}

/// One `rotate` pass over the scope as the CLI would make it: the ledger as it
/// stands, then the transaction or the recovery of an interrupted one.
fn run_rotation(remote: &FakeRemote, key_path: &Path) -> Run {
    let records = ledger();
    match std::panic::catch_unwind(AssertUnwindSafe(|| {
        rotate_scope(remote, key_path, &records)
    })) {
        Ok(result) => Run::Completed(result),
        Err(payload) if payload.is::<SimulatedCrash>() => Run::Crashed,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Re-run until no recovery state remains, as repeated `rotate` calls would.
fn settle(fixture: &Fixture) {
    for _ in 0..4 {
        if !recovery_pending() {
            return;
        }
        let remote = FakeRemote::new(&fixture.homes, Fault::None, 200);
        match run_rotation(&remote, &fixture.key_path) {
            Run::Completed(result) => {
                if let Err(error) = result {
                    panic!("resuming an interrupted rotation failed: {error:#}");
                }
            }
            Run::Crashed => unreachable!("no fault is armed while settling"),
        }
    }
    assert!(!recovery_pending(), "recovery did not settle");
}

/// Exactly one managed key on every instance, a ledger naming it without
/// recovery state, and a matching canonical pair. Returns whether the
/// replacement became canonical and any extra local files.
fn assert_consistent(fixture: &Fixture, context: &str) -> (bool, Vec<String>) {
    let canonical_public = std::fs::read_to_string(public_path(&fixture.key_path))
        .unwrap_or_else(|error| panic!("{context}: canonical public key missing: {error}"));
    let canonical_public = canonical_public.trim();
    let canonical_body = key_body(canonical_public);
    let private = std::fs::read_to_string(&fixture.key_path)
        .unwrap_or_else(|error| panic!("{context}: canonical private key missing: {error}"));
    assert_eq!(
        private.strip_prefix(PRIVATE_PREFIX).map(str::trim),
        Some(canonical_body.as_str()),
        "{context}: canonical pair halves disagree"
    );

    let rows = ledger();
    assert_eq!(
        rows.len(),
        INSTANCES.len(),
        "{context}: ledger rows: {rows:?}"
    );
    for row in &rows {
        assert!(
            row.rotation_recovery.is_none(),
            "{context}: recovery state left in {row:?}"
        );
        assert_eq!(row.key_path, fixture.key_path, "{context}: {row:?}");
        assert_eq!(
            row.key_fingerprint,
            thegn_core::managed_ssh::key_fingerprint(canonical_public),
            "{context}: ledger does not name the canonical key: {row:?}"
        );
    }

    for instance in INSTANCES {
        let bodies = authorized_bodies(&fixture.homes.join(instance));
        let managed = bodies
            .iter()
            .filter(|body| *body != OPERATOR_BODY)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            managed,
            vec![canonical_body.clone()],
            "{context}: {instance} must authorize exactly the canonical managed key"
        );
        assert_eq!(
            bodies.iter().filter(|body| *body == OPERATOR_BODY).count(),
            1,
            "{context}: {instance} lost or duplicated the operator's own key"
        );
    }

    let canonical_names = [
        fixture.key_path.file_name().unwrap().to_owned(),
        public_path(&fixture.key_path)
            .file_name()
            .unwrap()
            .to_owned(),
    ];
    let mut leftovers = std::fs::read_dir(&fixture.keys)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| !canonical_names.contains(name))
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    leftovers.sort();
    (canonical_body != fixture.old_body, leftovers)
}

/// Only a crash in an [`ORPHAN_PAIR_PHASES`] boundary may leave local files
/// behind, exactly one never-authorized pair (`assert_consistent` has already
/// proven no instance authorizes it); the next uninterrupted rotation must
/// then remove it. Returns whether an orphan was found and swept.
fn assert_orphans_only_from_orphan_phases_then_swept(
    fixture: &Fixture,
    context: &str,
    phases: &[&str],
    leftovers: &[String],
) -> bool {
    assert_no_leftovers_unless_orphan_phase(context, phases, leftovers);
    if leftovers.is_empty() {
        return false;
    }
    assert_next_rotation_sweeps_orphans(fixture, context);
    true
}

/// A second, uninterrupted `rotate` deletes every orphaned sibling and leaves
/// the scope consistent on the newly rotated key.
fn assert_next_rotation_sweeps_orphans(fixture: &Fixture, context: &str) {
    let remote = FakeRemote::new(&fixture.homes, Fault::None, 150);
    match run_rotation(&remote, &fixture.key_path) {
        Run::Completed(result) => {
            if let Err(error) = result {
                panic!("{context}: the sweeping rotation failed: {error:#}");
            }
        }
        Run::Crashed => unreachable!("no fault is armed for the sweeping rotation"),
    }
    let context = format!("{context}, then a sweeping rotation");
    let (rotated, leftovers) = assert_consistent(fixture, &context);
    assert!(rotated, "{context}: the sweeping rotation must rotate");
    assert!(
        leftovers.is_empty(),
        "{context}: orphaned pair survived the next rotation: {leftovers:?}"
    );
}

fn assert_no_leftovers_unless_orphan_phase(context: &str, phases: &[&str], leftovers: &[String]) {
    if leftovers.is_empty() {
        return;
    }
    assert!(
        phases
            .iter()
            .any(|phase| ORPHAN_PAIR_PHASES.contains(phase)),
        "{context}: stray local key files {leftovers:?}"
    );
    assert_eq!(
        leftovers.len(),
        2,
        "{context}: an orphan is exactly one retired or auxiliary pair: {leftovers:?}"
    );
}

#[test]
fn uninterrupted_rotation_retires_the_old_key_everywhere() {
    let Some(fixture) = fixture() else {
        return;
    };
    let remote = FakeRemote::new(&fixture.homes, Fault::None, 1);
    match run_rotation(&remote, &fixture.key_path) {
        Run::Completed(result) => result.unwrap(),
        Run::Crashed => unreachable!("no fault is armed"),
    }
    let (rotated, leftovers) = assert_consistent(&fixture, "uninterrupted");
    assert!(rotated, "the replacement must become canonical");
    assert!(leftovers.is_empty(), "stray local key files {leftovers:?}");
    assert_eq!(remote.phases.get(), 18, "two instances cross 18 boundaries");
    assert_eq!(
        remote.runs.get(),
        6,
        "authorize, verify, retire per instance"
    );
}

#[test]
fn a_crash_after_every_rotation_phase_resumes_to_exactly_one_authorized_key() {
    let mut seen = BTreeSet::new();
    let mut swept = BTreeSet::new();
    for target in 1.. {
        let Some(fixture) = fixture() else {
            return;
        };
        let remote = FakeRemote::new(&fixture.homes, Fault::CrashAtPhase(target), 1);
        match run_rotation(&remote, &fixture.key_path) {
            Run::Completed(result) => {
                result.unwrap();
                assert!(remote.fired().is_none());
                assert!(target > 1, "the rotation must report phases");
                break;
            }
            Run::Crashed => {}
        }
        let phase = remote.fired().expect("a crash names its phase");
        let context = format!("crash #{target} after {phase}");
        settle(&fixture);
        let (rotated, leftovers) = assert_consistent(&fixture, &context);
        assert_eq!(
            rotated,
            FORWARD_PHASES.contains(&phase.as_str()),
            "{context}: rolled the wrong way"
        );
        if assert_orphans_only_from_orphan_phases_then_swept(
            &fixture,
            &context,
            &[phase.as_str()],
            &leftovers,
        ) {
            swept.insert(phase.clone());
        }
        seen.insert(phase);
    }
    for phase in ROTATION_PHASES {
        assert!(seen.contains(phase), "phase {phase} was never interrupted");
    }
    assert!(
        swept.contains("custody-completed"),
        "a crash after the last custody completion must orphan the retired pair, \
         and the next rotation must sweep it; swept after {swept:?}"
    );
}

#[test]
fn a_crash_inside_the_recovery_of_a_crashed_rotation_still_settles() {
    let mut recovery_seen = BTreeSet::new();
    let mut swept = BTreeSet::new();
    'rotation: for first in 1.. {
        for second in 1.. {
            let Some(fixture) = fixture() else {
                return;
            };
            let remote = FakeRemote::new(&fixture.homes, Fault::CrashAtPhase(first), 1);
            if let Run::Completed(result) = run_rotation(&remote, &fixture.key_path) {
                result.unwrap();
                break 'rotation;
            }
            let first_phase = remote.fired().expect("a crash names its phase");
            if !recovery_pending() {
                let context = format!("crash after {first_phase}");
                let (_, leftovers) = assert_consistent(&fixture, &context);
                if assert_orphans_only_from_orphan_phases_then_swept(
                    &fixture,
                    &context,
                    &[first_phase.as_str()],
                    &leftovers,
                ) {
                    swept.insert(first_phase.clone());
                }
                continue 'rotation;
            }
            let resume = FakeRemote::new(&fixture.homes, Fault::CrashAtPhase(second), 100);
            let crashed_again = match run_rotation(&resume, &fixture.key_path) {
                Run::Completed(result) => {
                    if let Err(error) = result {
                        panic!("recovery after {first_phase} failed: {error:#}");
                    }
                    false
                }
                Run::Crashed => true,
            };
            let second_phase = resume.fired().unwrap_or_default();
            let context = format!(
                "crash #{first} after {first_phase}, then recovery crash #{second} after {second_phase}"
            );
            settle(&fixture);
            let (_, leftovers) = assert_consistent(&fixture, &context);
            if assert_orphans_only_from_orphan_phases_then_swept(
                &fixture,
                &context,
                &[first_phase.as_str(), second_phase.as_str()],
                &leftovers,
            ) {
                swept.insert(second_phase.clone());
            }
            if !crashed_again {
                continue 'rotation;
            }
            recovery_seen.insert(second_phase);
        }
    }
    for phase in RECOVERY_PHASES {
        assert!(
            recovery_seen.contains(phase),
            "recovery phase {phase} was never interrupted"
        );
    }
    for phase in ["recovery-cleared", "recovery-completed-forward"] {
        assert!(
            swept.contains(phase),
            "a recovery crash after the last {phase} must orphan a pair that the next \
             rotation sweeps; swept after {swept:?}"
        );
    }
}

#[test]
fn a_remote_failure_at_every_step_rolls_back_to_exactly_one_authorized_key() {
    for lost_response in [false, true] {
        for target in 1.. {
            let Some(fixture) = fixture() else {
                return;
            };
            let fault = if lost_response {
                Fault::FailAfterRun(target)
            } else {
                Fault::FailBeforeRun(target)
            };
            let remote = FakeRemote::new(&fixture.homes, fault, 1);
            let result = match run_rotation(&remote, &fixture.key_path) {
                Run::Completed(result) => result,
                Run::Crashed => unreachable!("no crash is armed"),
            };
            let Some(operation) = remote.fired() else {
                result.unwrap();
                assert!(target > 6, "every happy-path command must be faulted");
                break;
            };
            let context = format!("{fault:?} during {operation}");
            let error = match result {
                Ok(()) => panic!("{context}: an injected failure must surface"),
                Err(error) => format!("{error:#}"),
            };
            assert!(
                error.contains("fully rolled back"),
                "{context}: in-process recovery must finish: {error}"
            );
            assert!(
                !recovery_pending(),
                "{context}: a single failure must not need a later resume"
            );
            let (rotated, leftovers) = assert_consistent(&fixture, &context);
            assert!(!rotated, "{context}: a failed rotation keeps the old key");
            assert!(leftovers.is_empty(), "{context}: stray files {leftovers:?}");
        }
    }
}

#[test]
fn recovery_fails_closed_when_neither_key_can_prove_the_primary_is_authorized() {
    let Some(fixture) = fixture() else {
        return;
    };
    // Crash right after local promotion: recovery must restore the old key
    // through the replacement before removing the replacement.
    let promoted = 1 + 4 * INSTANCES.len() + INSTANCES.len() + 1;
    let remote = FakeRemote::new(&fixture.homes, Fault::CrashAtPhase(promoted), 1);
    assert!(matches!(
        run_rotation(&remote, &fixture.key_path),
        Run::Crashed
    ));
    assert_eq!(remote.fired().as_deref(), Some("pair-promoted"));
    // Someone strips every managed key from one instance out of band.
    std::fs::write(
        fixture
            .homes
            .join(INSTANCES[0])
            .join(".ssh")
            .join("authorized_keys"),
        format!("{OPERATOR_KEY}\n"),
    )
    .unwrap();

    let resume = FakeRemote::new(&fixture.homes, Fault::None, 100);
    let error = match run_rotation(&resume, &fixture.key_path) {
        Run::Completed(Err(error)) => format!("{error:#}"),
        Run::Completed(Ok(())) => panic!("recovery must not report success"),
        Run::Crashed => unreachable!("no crash is armed"),
    };
    assert!(
        error.contains("not provably authorized"),
        "unexpected error: {error}"
    );
    assert!(
        recovery_pending(),
        "custody keeps the recovery plan when recovery cannot finish"
    );
}
