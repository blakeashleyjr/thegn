//! Process-level resource proof for the bounded runner (THE-538).
//!
//! The in-crate unit tests share one test binary and run on parallel threads, so
//! they cannot count this process's fds or threads. This integration-test binary
//! owns its process: the two tests below serialize on one lock, and nothing else
//! spawns threads or opens files here, so `/proc/self` reflects only the runner.
//!
//! No assertion is about wall-clock time; timeouts are bounds the runner enforces.
#![cfg(target_os = "linux")]

use std::{
    fs,
    process::Command,
    sync::{Mutex, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

use thegn_proc::{End, GroupState, Leader, Limits, Run, Stream, Which, run};

/// Serializes the tests in this binary so one's children are never the other's
/// observation (libtest runs tests on parallel threads by default).
static PROCESS_LOCK: Mutex<()> = Mutex::new(());

/// Iterations of the five-kind rotation for the growth proof.
const ITERATIONS: usize = 40;

/// Slack on the fd count: a lazily opened std/libtest handle (for example the
/// first `/proc` read buffer or a cached `/dev/null`) may appear once after the
/// baseline. A per-run leak would add at least one fd per leaking run, which is
/// far above this bound after 40 runs.
const FD_SLACK: usize = 2;

/// Slack on the thread count: one in-flight `thegn-proc-reap` thread (owner of
/// a `Leader::Detached` child) may still be exiting when the drain poll ends.
const THREAD_SLACK: usize = 1;

/// Upper bound on waiting for detached reap threads to exit after the loop.
const DRAIN_BOUND: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Clean,
    Hung,
    Oversized,
    Failed,
    PipeHolder,
}

const ROTATION: [Kind; 5] = [
    Kind::Clean,
    Kind::Hung,
    Kind::Oversized,
    Kind::Failed,
    Kind::PipeHolder,
];

fn shell(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", script]);
    command
}

/// Generous settle bounds so a loaded machine still reaps inside them; they are
/// ceilings, not waits.
fn base(timeout: Duration) -> Limits {
    Limits {
        kill_settle: Duration::from_secs(2),
        ..Limits::silent(timeout)
    }
}

fn run_kind(kind: Kind) -> Run {
    let cancel = AtomicBool::new(false);
    match kind {
        Kind::Clean => run(
            &mut Command::new("true"),
            &base(Duration::from_secs(10)),
            &cancel,
        ),
        Kind::Hung => run(
            &mut shell("exec sleep 30"),
            &base(Duration::from_millis(100)),
            &cancel,
        ),
        Kind::Oversized => run(
            &mut shell("exec yes"),
            &Limits {
                stdout: Stream::HeadOrFail { cap: 1024 },
                ..base(Duration::from_secs(10))
            },
            &cancel,
        ),
        Kind::Failed => run(
            &mut shell("printf oops >&2; exit 3"),
            &Limits {
                stderr: Stream::Tail { cap: 256 },
                ..base(Duration::from_secs(10))
            },
            &cancel,
        ),
        Kind::PipeHolder => run(
            &mut shell("sleep 30 & exit 0"),
            &Limits {
                stdout: Stream::Tail { cap: 128 },
                quiesce_after_leader: Some(Duration::from_millis(50)),
                reader_grace: Duration::from_millis(100),
                ..base(Duration::from_secs(10))
            },
            &cancel,
        ),
    }
}

/// Every run, whatever its end, must leave a reaped leader and a group that is
/// observed empty (drained on its own or terminated), never survivors.
fn assert_outcome(kind: Kind, result: &Run) {
    assert!(
        matches!(result.leader, Leader::Reaped(_)),
        "{kind:?}: leader not reaped: {result:?}"
    );
    assert!(
        matches!(result.group, GroupState::Drained | GroupState::Terminated),
        "{kind:?}: group not settled: {result:?}"
    );
    match kind {
        Kind::Clean => assert!(result.success(), "{kind:?}: {result:?}"),
        Kind::Hung => {
            assert!(matches!(result.end, End::TimedOut), "{kind:?}: {result:?}");
            assert_eq!(result.group, GroupState::Terminated, "{kind:?}");
        }
        Kind::Oversized => {
            assert!(
                matches!(result.end, End::OutputLimit(Which::Stdout)),
                "{kind:?}: {result:?}"
            );
            assert_eq!(result.group, GroupState::Terminated, "{kind:?}");
            assert!(result.stdout.truncated, "{kind:?}");
            assert!(result.stdout.bytes.len() <= 1024, "{kind:?}");
        }
        Kind::Failed => {
            assert!(
                matches!(result.end, End::LeaderExited),
                "{kind:?}: {result:?}"
            );
            assert_eq!(
                result.exit_status().and_then(|status| status.code()),
                Some(3),
                "{kind:?}"
            );
            assert_eq!(result.stderr.bytes, b"oops".to_vec(), "{kind:?}");
        }
        Kind::PipeHolder => {
            assert!(
                matches!(result.end, End::LeaderExited),
                "{kind:?}: {result:?}"
            );
            assert!(result.leader_exited_first, "{kind:?}");
            assert_eq!(result.group, GroupState::Terminated, "{kind:?}");
        }
    }
}

fn run_rotation(iterations: usize) {
    for index in 0..iterations {
        let kind = ROTATION[index % ROTATION.len()];
        let result = run_kind(kind);
        assert_outcome(kind, &result);
    }
}

fn fd_count() -> usize {
    fs::read_dir("/proc/self/fd")
        .expect("read /proc/self/fd")
        .count()
}

fn thread_count() -> usize {
    let status = fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .expect("Threads: line in /proc/self/status")
}

/// `(state, ppid)` from a `/proc/<pid>/stat` body; the comm field may contain
/// spaces and parentheses, so parse after the last `)`.
fn stat_state_ppid(stat: &str) -> Option<(char, u32)> {
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse::<u32>().ok()?;
    Some((state, ppid))
}

/// Pids whose parent is this process and whose state is zombie.
fn zombie_children() -> Vec<u32> {
    let me = std::process::id();
    let mut zombies = Vec::new();
    for entry in fs::read_dir("/proc").expect("read /proc").flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // A process that exited between read_dir and this read is gone, not a zombie.
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if let Some(('Z', ppid)) = stat_state_ppid(&stat)
            && ppid == me
        {
            zombies.push(pid);
        }
    }
    zombies
}

#[test]
fn repeated_hung_oversized_and_failed_runs_do_not_grow_fds_or_threads() {
    let _guard = PROCESS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // Warm-up: one pass of every kind so lazily initialised std state (signal
    // handling, /dev/null, pipe buffers) is in the baseline, not the delta.
    run_rotation(ROTATION.len());
    let fds_before = fd_count();
    let threads_before = thread_count();

    run_rotation(ITERATIONS);

    // Detached reap threads exit once their child is reaped; give them a
    // bounded window. This is a drain bound, not a timing assertion.
    let deadline = Instant::now() + DRAIN_BOUND;
    while (thread_count() > threads_before || fd_count() > fds_before) && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(20));
    }
    let fds_after = fd_count();
    let threads_after = thread_count();

    assert!(
        fds_after <= fds_before + FD_SLACK,
        "fds grew: before {fds_before}, after {fds_after} over {ITERATIONS} runs"
    );
    assert!(
        threads_after <= threads_before + THREAD_SLACK,
        "threads grew: before {threads_before}, after {threads_after} over {ITERATIONS} runs"
    );
}

#[test]
fn no_zombie_children_remain_after_the_loop() {
    let _guard = PROCESS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    run_rotation(ROTATION.len() * 4);

    let zombies = zombie_children();
    assert!(
        zombies.is_empty(),
        "zombie children of {} remain: {zombies:?}",
        std::process::id()
    );
}

#[test]
fn stat_parser_handles_spaces_and_parentheses_in_comm() {
    assert_eq!(
        stat_state_ppid("42 (a ) b) Z 7 42 0"),
        Some(('Z', 7)),
        "comm with a space and a close paren"
    );
    assert_eq!(stat_state_ppid("42 (sh) S"), None, "truncated after state");
    assert_eq!(stat_state_ppid("no paren"), None);
}
