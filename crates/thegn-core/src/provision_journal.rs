//! Durable state for prompt-first provisioning.
//!
//! A journal is stored at `$XDG_STATE_HOME/thegn/provider/deferred/<id>.json`
//! with version `1` and the fields `worktree`, `env`, `state`, `step`,
//! `detail`, and `updated_ms`. `state` is `pending`, `failed`, or `cancelled`;
//! details are control-free and at most 240 characters. Writes are atomic and
//! owner-only. The sibling `<id>.lock` is never removed. Callers acquire the
//! run lease before the sandbox lock; a lease is only tried while the sandbox
//! lock is held, and blocking acquisition happens only after that lock drops.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::envplan::MarkerState;

const REL: &str = "provider/deferred";
const MAX_JOURNAL_BYTES: usize = 16 * 1024;
const MAX_DETAIL_CHARS: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredState {
    Pending,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub v: u32,
    pub worktree: String,
    pub env: String,
    pub state: DeferredState,
    pub step: String,
    pub detail: String,
    pub updated_ms: i64,
}

impl Journal {
    pub fn pending(worktree: &str, env: &str) -> Self {
        Journal {
            v: 1,
            worktree: worktree.to_string(),
            env: env.to_string(),
            state: DeferredState::Pending,
            step: String::new(),
            detail: String::new(),
            updated_ms: crate::util::now_ms(),
        }
    }

    pub fn failed(worktree: &str, env: &str, step: &str, detail: &str) -> Self {
        Journal {
            v: 1,
            worktree: worktree.to_string(),
            env: env.to_string(),
            state: DeferredState::Failed,
            step: strip_controls(step),
            detail: strip_controls(detail)
                .chars()
                .take(MAX_DETAIL_CHARS)
                .collect(),
            updated_ms: crate::util::now_ms(),
        }
    }
}

fn strip_controls(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn invalid_id() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "deferred provisioning id must match [A-Za-z0-9._-]{1,200} and not start with '.'",
    )
}

fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 200
        && !bytes.starts_with(b".")
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn default_dir() -> PathBuf {
    crate::util::xdg_state_home().join("thegn").join(REL)
}

fn open_default_dir() -> io::Result<crate::fsperm::PrivateDir> {
    let anchor = crate::util::xdg_state_home().join("thegn");
    std::fs::create_dir_all(&anchor)?;
    crate::fsperm::ensure_private_dir_beneath(&anchor, Path::new(REL))
}

fn open_existing(dir: &Path) -> io::Result<Option<crate::fsperm::PrivateDir>> {
    match crate::fsperm::open_private_dir(dir) {
        Ok(private) => Ok(Some(private)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn unreadable_journal() -> Journal {
    let mut journal = Journal::pending("", "");
    journal.detail = "unreadable journal".into();
    journal
}

fn read_private(private: &crate::fsperm::PrivateDir, id: &str) -> Option<Journal> {
    let mut file = match private.open_regular_nofollow(&format!("{id}.json")) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(_) => return Some(unreadable_journal()),
    };
    let mut bytes = Vec::new();
    if file
        .by_ref()
        .take((MAX_JOURNAL_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > MAX_JOURNAL_BYTES
    {
        return Some(unreadable_journal());
    }
    match serde_json::from_slice::<Journal>(&bytes) {
        Ok(journal) if journal.v == 1 => Some(journal),
        Ok(_) | Err(_) => Some(unreadable_journal()),
    }
}

pub fn read(id: &str) -> Option<Journal> {
    read_in(&default_dir(), id)
}

pub fn read_in(dir: &Path, id: &str) -> Option<Journal> {
    if !valid_id(id) {
        return None;
    }
    match open_existing(dir) {
        Ok(Some(private)) => read_private(&private, id),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(
                target: "thegn::provision",
                path = %dir.display(),
                %error,
                "ignoring deferred provisioning journals in non-private directory"
            );
            None
        }
    }
}

fn write_private(
    private: &crate::fsperm::PrivateDir,
    id: &str,
    journal: &Journal,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(journal).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serialize journal: {error}"),
        )
    })?;
    private
        .write_owner_only_atomic(&format!("{id}.json"), &bytes)
        .map(|_| ())
}

pub fn write(id: &str, journal: &Journal) -> io::Result<()> {
    if !valid_id(id) {
        return Err(invalid_id());
    }
    write_private(&open_default_dir()?, id, journal)
}

pub fn write_in(dir: &Path, id: &str, journal: &Journal) -> io::Result<()> {
    if !valid_id(id) {
        return Err(invalid_id());
    }
    crate::fsperm::ensure_private_dir(dir)?;
    write_private(&crate::fsperm::open_private_dir(dir)?, id, journal)
}

pub fn clear(id: &str) -> io::Result<bool> {
    clear_in(&default_dir(), id)
}

pub fn clear_in(dir: &Path, id: &str) -> io::Result<bool> {
    if !valid_id(id) {
        return Err(invalid_id());
    }
    let Some(private) = open_existing(dir)? else {
        return Ok(false);
    };
    private.remove_file(&format!("{id}.json"))
}

pub fn cancel(id: &str) -> io::Result<bool> {
    cancel_in(&default_dir(), id)
}

pub fn cancel_in(dir: &Path, id: &str) -> io::Result<bool> {
    if !valid_id(id) {
        return Err(invalid_id());
    }
    let Some(mut journal) = read_in(dir, id) else {
        return Ok(false);
    };
    if journal.state != DeferredState::Cancelled {
        journal.state = DeferredState::Cancelled;
        journal.updated_ms = crate::util::now_ms();
        write_in(dir, id, &journal)?;
    }
    Ok(true)
}

#[derive(Debug)]
pub struct RunLease {
    _file: std::fs::File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseWait {
    Try,
    Block,
}

fn lease_private(
    private: &crate::fsperm::PrivateDir,
    id: &str,
    wait: LeaseWait,
) -> io::Result<Option<RunLease>> {
    let file = private.open_or_create_regular_nofollow(&format!("{id}.lock"))?;
    match wait {
        LeaseWait::Try => match file.try_lock() {
            Ok(()) => Ok(Some(RunLease { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        },
        LeaseWait::Block => {
            file.lock()?;
            Ok(Some(RunLease { _file: file }))
        }
    }
}

pub fn run_lease(id: &str, wait: LeaseWait) -> io::Result<Option<RunLease>> {
    if !valid_id(id) {
        return Err(invalid_id());
    }
    lease_private(&open_default_dir()?, id, wait)
}

pub fn run_lease_in(dir: &Path, id: &str, wait: LeaseWait) -> io::Result<Option<RunLease>> {
    if !valid_id(id) {
        return Err(invalid_id());
    }
    crate::fsperm::ensure_private_dir(dir)?;
    lease_private(&crate::fsperm::open_private_dir(dir)?, id, wait)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentGate {
    Go,
    Wait,
    GoWithWarning,
}

/// None → Go; Pending | Cancelled → Wait; Failed → GoWithWarning.
pub fn agent_gate(journal: Option<&Journal>) -> AgentGate {
    match journal.map(|journal| journal.state) {
        None => AgentGate::Go,
        Some(DeferredState::Pending | DeferredState::Cancelled) => AgentGate::Wait,
        Some(DeferredState::Failed) => AgentGate::GoWithWarning,
    }
}

pub fn is_agent_choice(cfg: &crate::config::Config, choice: &str) -> bool {
    choice != "shell"
        && cfg
            .agent_command(choice)
            .is_some_and(|command| command.trim() != "__shell__")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    All,
    Foreground,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseAction {
    ShortCircuit,
    RunAll,
    RunForeground,
    RunDeferred,
    WaitThenRunDeferred,
    ReportPending,
    AlreadyDone,
    Abandon,
}

pub fn phase_action(phase: Phase, marker: Option<MarkerState>) -> PhaseAction {
    match (phase, marker) {
        (Phase::All, None) => PhaseAction::RunAll,
        (Phase::All, Some(MarkerState::Done)) => PhaseAction::ShortCircuit,
        (Phase::All, Some(MarkerState::Deferred)) => PhaseAction::WaitThenRunDeferred,
        (Phase::Foreground, None) => PhaseAction::RunForeground,
        (Phase::Foreground, Some(MarkerState::Done)) => PhaseAction::ShortCircuit,
        (Phase::Foreground, Some(MarkerState::Deferred)) => PhaseAction::ReportPending,
        (Phase::Deferred, None) => PhaseAction::Abandon,
        (Phase::Deferred, Some(MarkerState::Done)) => PhaseAction::AlreadyDone,
        (Phase::Deferred, Some(MarkerState::Deferred)) => PhaseAction::RunDeferred,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeferredOutcome {
    pub cancelled: bool,
    pub failed: bool,
    pub checkpoint_planned: bool,
    pub checkpoint_ok: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalAction {
    Clear,
    MarkFailed,
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TailAction {
    pub reconcile_credential: bool,
    pub write_done_marker: bool,
    pub journal: JournalAction,
}

pub fn deferred_tail(outcome: DeferredOutcome) -> TailAction {
    if outcome.cancelled {
        return TailAction {
            reconcile_credential: false,
            write_done_marker: false,
            journal: JournalAction::Keep,
        };
    }
    if outcome.failed {
        return TailAction {
            reconcile_credential: outcome.checkpoint_planned && outcome.checkpoint_ok,
            write_done_marker: false,
            journal: JournalAction::MarkFailed,
        };
    }
    TailAction {
        reconcile_credential: outcome.checkpoint_planned,
        write_done_marker: true,
        journal: JournalAction::Clear,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn journal_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("journal test directory");
        crate::fsperm::ensure_private_dir(dir.path()).expect("private journal test directory");
        dir
    }

    fn pending() -> Journal {
        Journal::pending("/workspace", "dev")
    }

    #[test]
    fn journal_write_read_round_trip() {
        let dir = journal_dir();
        let journal = Journal::failed("/workspace", "dev", "agents_install", "bad\nthing");
        write_in(dir.path(), "sandbox-1", &journal).unwrap();
        assert_eq!(read_in(dir.path(), "sandbox-1"), Some(journal));
    }

    #[test]
    fn clear_leaves_lock_file() {
        let dir = journal_dir();
        write_in(dir.path(), "sandbox-1", &pending()).unwrap();
        run_lease_in(dir.path(), "sandbox-1", LeaseWait::Try)
            .unwrap()
            .expect("lease");
        assert!(clear_in(dir.path(), "sandbox-1").unwrap());
        assert!(dir.path().join("sandbox-1.lock").exists());
        assert!(!dir.path().join("sandbox-1.json").exists());
    }

    #[test]
    fn cancel_flips_pending_and_failed_and_absent_is_false() {
        let dir = journal_dir();
        assert!(read_in(dir.path(), "sandbox-1").is_none());
        assert!(!cancel_in(dir.path(), "sandbox-1").unwrap());
        write_in(dir.path(), "sandbox-1", &pending()).unwrap();
        assert!(cancel_in(dir.path(), "sandbox-1").unwrap());
        assert_eq!(
            read_in(dir.path(), "sandbox-1").unwrap().state,
            DeferredState::Cancelled
        );
        let failed = Journal::failed("/workspace", "dev", "step", "detail");
        write_in(dir.path(), "sandbox-1", &failed).unwrap();
        assert!(cancel_in(dir.path(), "sandbox-1").unwrap());
        assert_eq!(
            read_in(dir.path(), "sandbox-1").unwrap().state,
            DeferredState::Cancelled
        );
    }

    #[test]
    fn corrupt_journal_reads_as_pending() {
        let dir = journal_dir();
        std::fs::write(dir.path().join("sandbox-1.json"), b"not json").unwrap();
        let journal = read_in(dir.path(), "sandbox-1").unwrap();
        assert_eq!(journal.state, DeferredState::Pending);
        assert_eq!(journal.detail, "unreadable journal");
    }

    #[test]
    fn non_private_directory_reads_as_absent_and_cancel_is_false() {
        let dir = journal_dir();
        // Platforms with no mode bits report Unsupported: nothing to check there.
        if crate::fsperm::set_mode_for_test(dir.path(), 0o755).is_err() {
            return;
        }
        assert!(read_in(dir.path(), "sandbox-1").is_none());
        assert!(!cancel_in(dir.path(), "sandbox-1").unwrap());
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let dir = journal_dir();
        assert!(read_in(dir.path(), ".hidden").is_none());
        assert_eq!(
            write_in(dir.path(), "bad/id", &pending())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            run_lease_in(dir.path(), &"x".repeat(201), LeaseWait::Try)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn try_lease_refuses_second_holder_in_same_process() {
        let dir = journal_dir();
        let holder = run_lease_in(dir.path(), "sandbox-1", LeaseWait::Try)
            .unwrap()
            .expect("first lease");
        assert!(
            run_lease_in(dir.path(), "sandbox-1", LeaseWait::Try)
                .unwrap()
                .is_none()
        );
        drop(holder);
    }

    #[test]
    fn blocking_lease_waits_for_holder() {
        let dir = journal_dir();
        let holder = run_lease_in(dir.path(), "sandbox-1", LeaseWait::Try)
            .unwrap()
            .expect("first lease");
        let path = dir.path().to_path_buf();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(holder);
        });
        assert!(
            run_lease_in(&path, "sandbox-1", LeaseWait::Block)
                .unwrap()
                .is_some()
        );
        thread.join().unwrap();
    }

    #[test]
    fn agent_gate_table() {
        assert_eq!(agent_gate(None), AgentGate::Go);
        assert_eq!(agent_gate(Some(&pending())), AgentGate::Wait);
        let mut cancelled = pending();
        cancelled.state = DeferredState::Cancelled;
        assert_eq!(agent_gate(Some(&cancelled)), AgentGate::Wait);
        let mut failed = pending();
        failed.state = DeferredState::Failed;
        assert_eq!(agent_gate(Some(&failed)), AgentGate::GoWithWarning);
    }

    #[test]
    fn phase_action_covers_all_nine_cells() {
        assert_eq!(phase_action(Phase::All, None), PhaseAction::RunAll);
        assert_eq!(
            phase_action(Phase::All, Some(MarkerState::Done)),
            PhaseAction::ShortCircuit
        );
        assert_eq!(
            phase_action(Phase::All, Some(MarkerState::Deferred)),
            PhaseAction::WaitThenRunDeferred
        );
        assert_eq!(
            phase_action(Phase::Foreground, None),
            PhaseAction::RunForeground
        );
        assert_eq!(
            phase_action(Phase::Foreground, Some(MarkerState::Done)),
            PhaseAction::ShortCircuit
        );
        assert_eq!(
            phase_action(Phase::Foreground, Some(MarkerState::Deferred)),
            PhaseAction::ReportPending
        );
        assert_eq!(phase_action(Phase::Deferred, None), PhaseAction::Abandon);
        assert_eq!(
            phase_action(Phase::Deferred, Some(MarkerState::Done)),
            PhaseAction::AlreadyDone
        );
        assert_eq!(
            phase_action(Phase::Deferred, Some(MarkerState::Deferred)),
            PhaseAction::RunDeferred
        );
    }

    #[test]
    fn deferred_tail_covers_cancelled_failed_and_success() {
        let cases = [
            (
                DeferredOutcome {
                    cancelled: true,
                    failed: false,
                    checkpoint_planned: true,
                    checkpoint_ok: true,
                },
                TailAction {
                    reconcile_credential: false,
                    write_done_marker: false,
                    journal: JournalAction::Keep,
                },
            ),
            (
                DeferredOutcome {
                    cancelled: false,
                    failed: true,
                    checkpoint_planned: false,
                    checkpoint_ok: false,
                },
                TailAction {
                    reconcile_credential: false,
                    write_done_marker: false,
                    journal: JournalAction::MarkFailed,
                },
            ),
            (
                DeferredOutcome {
                    cancelled: false,
                    failed: true,
                    checkpoint_planned: true,
                    checkpoint_ok: true,
                },
                TailAction {
                    reconcile_credential: true,
                    write_done_marker: false,
                    journal: JournalAction::MarkFailed,
                },
            ),
            (
                DeferredOutcome {
                    cancelled: false,
                    failed: false,
                    checkpoint_planned: false,
                    checkpoint_ok: false,
                },
                TailAction {
                    reconcile_credential: false,
                    write_done_marker: true,
                    journal: JournalAction::Clear,
                },
            ),
            (
                DeferredOutcome {
                    cancelled: false,
                    failed: false,
                    checkpoint_planned: true,
                    checkpoint_ok: true,
                },
                TailAction {
                    reconcile_credential: true,
                    write_done_marker: true,
                    journal: JournalAction::Clear,
                },
            ),
        ];
        for (outcome, expected) in cases {
            assert_eq!(deferred_tail(outcome), expected);
        }
    }

    #[test]
    fn is_agent_choice_table() {
        let mut cfg = crate::config::Config::default();
        cfg.agents.push(crate::config::NamedCommand {
            backends: Vec::new(),
            name: "__shell__".into(),
            command: "__shell__".into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
            resume: false,
            route_via_proxy: false,
            drawer_scope: None,
            drawer_cwd: None,
        });
        cfg.agents.push(crate::config::NamedCommand {
            backends: Vec::new(),
            name: "codex".into(),
            command: "codex".into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
            resume: false,
            route_via_proxy: false,
            drawer_scope: None,
            drawer_cwd: None,
        });
        assert!(!is_agent_choice(&cfg, "shell"));
        assert!(!is_agent_choice(&cfg, "__shell__"));
        assert!(is_agent_choice(&cfg, "codex"));
        assert!(!is_agent_choice(&cfg, "unknown"));
    }
}
