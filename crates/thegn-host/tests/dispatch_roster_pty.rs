#![cfg(unix)]

use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use thegn_core::db::Db;
use thegn_core::testenv::{IsolatedState, state_env_all};

// Match security_e2e.rs's PROCESS_TIMEOUT; every isolated CLI child is waited
// within this monotonic deadline.
const PTY_DEADLINE: Duration = Duration::from_secs(10);

fn seed(state_root: &Path) -> i64 {
    let db_path = state_root.join("state/thegn/thegn.db");
    drop(Db::open_at(&db_path).expect("initialize isolated state database"));
    let connection = rusqlite::Connection::open(&db_path).expect("open initialized database");
    connection
        .execute(
            "INSERT INTO agent_dispatches
             (issue_id, worktree_path, agent_name, dispatched_at_ms, status, stage, note, report, chunk_path)
             VALUES (?1, ?2, ?3, 1, 'running', ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "linear:EVIL-1\u{9b}31m\u{202e}",
                "/tmp/evil\u{1b}]52;c;ZXZpbA==\u{7}",
                "codex\u{1b}[2J",
                "code\u{2028}\u{200b}",
                "retry\u{7f}\u{1b}]0;forged\u{7}",
                "verdict: PASS\u{202e}\u{e0041}\u{1b}[2J",
                "chunks/c\u{9b}2J.md",
            ],
        )
        .expect("insert hostile legacy dispatch row");
    let id = connection.last_insert_rowid();
    connection
        .execute(
            "INSERT INTO agent_dispatch_notes (dispatch_id, created_at_ms, text)
             VALUES (?1, 2, ?2)",
            rusqlite::params![id, "note\u{1b}]52;c;ZXZpbA==\u{7}\u{2066}"],
        )
        .expect("insert hostile legacy dispatch note");
    id
}

fn run_in_pty(root: &Path, runtime: &Path, args: &[&str]) -> Vec<u8> {
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 50,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open CLI PTY");
    let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_thegn"));
    command.args(args);
    command.cwd(root);
    for (key, value) in state_env_all(root) {
        command.env(key, value);
    }
    command.env("XDG_RUNTIME_DIR", runtime);
    command.env("TERM", "xterm-256color");
    for key in [
        "THEGN_LOG",
        "THEGN_DATABASE_MIGRATION_EXECUTABLE",
        "THEGN_DATABASE_MIGRATION_AUTHORITY",
        "THEGN_DISPATCH_CAPABILITY",
        "THEGN_WORKTREE",
    ] {
        command.env_remove(key);
    }

    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn isolated CLI");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_done = Arc::new(AtomicBool::new(false));
    let captured_by_reader = Arc::clone(&captured);
    let done_by_reader = Arc::clone(&reader_done);
    let reader_thread = thread::Builder::new()
        .name("thegn-pty-read".into())
        .spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => captured_by_reader
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .extend_from_slice(&buffer[..read]),
                }
            }
            done_by_reader.store(true, Ordering::Release);
        })
        .expect("start PTY reader");

    let deadline = Instant::now() + PTY_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let kill_result = child.kill();
                let waited = child.wait().expect("wait for timed-out CLI");
                while !reader_done.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(5));
                }
                reader_thread.join().expect("join timed-out PTY reader");
                panic!(
                    "CLI timed out (exit code {}); kill result: {kill_result:?}; output: {}",
                    waited.exit_code(),
                    pty_output(&captured)
                );
            }
            Err(error) => {
                let kill_result = child.kill();
                let wait_result = child.wait();
                while !reader_done.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(5));
                }
                reader_thread
                    .join()
                    .expect("join PTY reader after poll failure");
                panic!(
                    "could not poll CLI: {error}; kill: {kill_result:?}; wait: {wait_result:?}; output: {}",
                    pty_output(&captured)
                );
            }
        }
    };

    while !reader_done.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        reader_done.load(Ordering::Acquire),
        "PTY reader exceeded {PTY_DEADLINE:?}; output: {}",
        pty_output(&captured)
    );
    reader_thread.join().expect("join PTY reader");
    assert!(
        status.success(),
        "CLI exited with code {}; output: {}",
        status.exit_code(),
        pty_output(&captured)
    );
    pty_output(&captured).into_bytes()
}

fn pty_output(captured: &Mutex<Vec<u8>>) -> String {
    String::from_utf8_lossy(
        &captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_owned()
}

fn assert_terminal_safe(label: &str, bytes: &[u8]) {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    for hue in [
        thegn_core::theme::DIM,
        thegn_core::theme::AMBER,
        thegn_core::theme::RED,
        thegn_core::theme::MAGENTA,
    ] {
        text = text.replace(&format!("\x1b[38;2;{hue}m"), "");
    }
    text = text.replace("\x1b[0m", "");
    let chars = text.chars().collect::<Vec<_>>();
    for (index, character) in chars.iter().copied().enumerate() {
        if character == '\x1b'
            || character == '\x07'
            || character == '\x7f'
            || ('\u{80}'..='\u{9f}').contains(&character)
            || thegn_core::pipeline_report::is_format_control(character)
            || (character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        {
            let start = index.saturating_sub(12);
            let end = (index + 13).min(chars.len());
            let window = chars[start..end].iter().copied().collect::<String>();
            panic!(
                "{label}: terminal-unsafe U+{:04X} near {window:?}",
                character as u32
            );
        }
    }
}

fn runtime_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("tg-pty-")
        .tempdir_in("/tmp")
        .expect("create short private runtime directory")
}

#[test]
// The test owns and waits on the direct PTY child; it does not signal a process
// group. Its seeded DB is private, read commands need no lease, and the running
// status keeps the row active independent of clock age.
fn hostile_roster_json_output_emits_no_control_sequence() {
    let state = IsolatedState::new();
    let runtime = runtime_dir();
    let id = seed(state.root());
    let id_text = id.to_string();

    for args in [
        &["dispatch", "list", "--json"][..],
        &["dispatch", "status", "--json"][..],
        &["dispatch", "status", &id_text, "--json"][..],
    ] {
        let output = run_in_pty(state.root(), runtime.path(), &args);
        assert_terminal_safe(&args.join(" "), &output);
    }
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "process integration test synchronously waits for the isolated CLI under test"
)]
// Each Command child is a read-only CLI invocation over this test's private
// seeded DB; no lease is involved. The running row is selected regardless of
// clock age, and PTY_DEADLINE bounds each child wait.
fn hostile_roster_json_round_trips_the_stored_text() {
    let state = IsolatedState::new();
    let runtime = runtime_dir();
    let id = seed(state.root());
    let id_text = id.to_string();
    let expected = [
        ("issue_id", "linear:EVIL-1\u{9b}31m\u{202e}"),
        ("worktree_path", "/tmp/evil\u{1b}]52;c;ZXZpbA==\u{7}"),
        ("agent_name", "codex\u{1b}[2J"),
        ("stage", "code\u{2028}\u{200b}"),
        ("note", "retry\u{7f}\u{1b}]0;forged\u{7}"),
        ("chunk_path", "chunks/c\u{9b}2J.md"),
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_thegn"))
        .args(["dispatch", "list", "--json"])
        .current_dir(state.root())
        .envs(state_env_all(state.root()))
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env_remove("THEGN_LOG")
        .env_remove("THEGN_DATABASE_MIGRATION_EXECUTABLE")
        .env_remove("THEGN_DATABASE_MIGRATION_AUTHORITY")
        .env_remove("THEGN_DISPATCH_CAPABILITY")
        .env_remove("THEGN_WORKTREE")
        .output()
        .expect("run isolated JSON CLI");
    assert!(
        output.status.success(),
        "dispatch list --json failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_terminal_safe("dispatch list --json stdout", &output.stdout);
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON list");
    let rows = document.as_array().expect("JSON list is an array");
    let row = rows
        .iter()
        .find(|row| row.get("id").and_then(serde_json::Value::as_i64) == Some(id))
        .expect("seeded row is present in JSON list");
    for (field, value) in expected {
        assert_eq!(
            row.get(field).and_then(serde_json::Value::as_str),
            Some(value)
        );
    }

    let output = Command::new(env!("CARGO_BIN_EXE_thegn"))
        .args(["dispatch", "status", &id_text, "--json"])
        .current_dir(state.root())
        .envs(state_env_all(state.root()))
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env_remove("THEGN_LOG")
        .env_remove("THEGN_DATABASE_MIGRATION_EXECUTABLE")
        .env_remove("THEGN_DATABASE_MIGRATION_AUTHORITY")
        .env_remove("THEGN_DISPATCH_CAPABILITY")
        .env_remove("THEGN_WORKTREE")
        .output()
        .expect("run isolated row-status JSON CLI");
    assert!(
        output.status.success(),
        "dispatch status row failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_terminal_safe("dispatch status row --json stdout", &output.stdout);
    let status: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse row-status JSON");
    assert_eq!(
        status.get("report").and_then(serde_json::Value::as_str),
        Some("verdict: PASS\u{202e}\u{e0041}\u{1b}[2J")
    );
    let notes = status
        .get("notes")
        .and_then(serde_json::Value::as_array)
        .expect("row status notes");
    assert_eq!(
        notes
            .first()
            .expect("seeded dispatch note is present")
            .get("text")
            .and_then(serde_json::Value::as_str),
        Some("note\u{1b}]52;c;ZXZpbA==\u{7}\u{2066}")
    );
}

// Positive control: human dispatch rendering was sanitized before this lane;
// these commands must keep that behavior while proving the hostile row prints.
#[test]
// The test owns and waits on the direct PTY child; it does not signal a process
// group. Its row and DB are private, no lease is needed, and clock age cannot
// remove the running row from active status output.
fn hostile_roster_human_output_emits_no_control_sequence() {
    let state = IsolatedState::new();
    let runtime = runtime_dir();
    let id = seed(state.root());
    let id_text = id.to_string();

    for args in [
        &["dispatch", "list"][..],
        &["dispatch", "status"][..],
        &["dispatch", "status", &id_text][..],
    ] {
        let output = run_in_pty(state.root(), runtime.path(), &args);
        assert_terminal_safe(&args.join(" "), &output);
        if args.len() == 3 {
            let text = String::from_utf8_lossy(&output);
            assert!(text.contains(&id_text), "row id missing from {text:?}");
            assert!(
                text.contains("report:"),
                "report marker missing from {text:?}"
            );
        }
    }
}
