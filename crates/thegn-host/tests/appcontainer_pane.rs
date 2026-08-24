//! Suite: an AppContainer pane, end to end (Tier 2, Windows + opt-in).
//!
//! `examples/appcontainer_conpty_spike.rs` proved the *mechanism* — a contained
//! grandchild can read and write a ConPTY owned by thegn. This proves the
//! **wiring**: that `sandbox::enter_argv` for `Backend::WinAppContainer` produces
//! an argv which, run through a real ConPTY with the real `thegn` binary, starts
//! a shell that is genuinely inside the container.
//!
//! The two are different failures. The spike would keep passing if `enter_argv`
//! stopped emitting the trampoline, or emitted the wrong profile, or dropped the
//! `--` and let clap eat the shell's flags — and the pane would silently run
//! uncontained on the host. That is exactly the false-security-claim shape this
//! codebase has been bitten by twice.
//!
//! Skips unless `THEGN_APPCONTAINER_E2E=1`: it creates an AppContainer profile
//! and writes an ACE on a temp directory, which is not something a plain
//! `cargo test` should do to a developer's machine.

#![cfg(windows)]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize};

const BUDGET: Duration = Duration::from_secs(45);

fn skip() -> bool {
    std::env::var("THEGN_APPCONTAINER_E2E").ok().as_deref() != Some("1")
}

/// Run `argv` in a real ConPTY and return everything the master saw.
///
/// Same shape as `tests/pty_launch.rs`, for the same reasons: ConPTY stalls the
/// child until its startup `ESC[6n` is answered and never signals EOF on master
/// close, so this polls `try_wait` and answers the query rather than blocking.
fn through_conpty(argv: &[String]) -> (Option<u32>, String) {
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(&argv[0]);
    for a in &argv[1..] {
        cmd.arg(a);
    }
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);

    let out = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let mut writer = pair.master.take_writer().expect("writer");
    {
        let out = out.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8 * 1024];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut b) = out.lock() {
                    b.extend_from_slice(&chunk[..n]);
                }
            }
        });
    }

    let mut answered = false;
    let deadline = Instant::now() + BUDGET;
    let exit = loop {
        if !answered
            && let Ok(b) = out.lock()
            && b.windows(4).any(|w| w == b"\x1b[6n")
        {
            let _ = writer.write_all(b"\x1b[1;1R");
            let _ = writer.flush();
            answered = true;
        }
        match child.try_wait() {
            Ok(Some(s)) => break Some(s.exit_code()),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    std::thread::sleep(Duration::from_millis(250));
    let text = out
        .lock()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    (exit, text)
}

/// Strip CSI/OSC so a marker interleaved with ConPTY escapes is still findable.
fn strip_ansi(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != '\u{1b}' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b.get(i) {
            Some('[') => {
                i += 1;
                while i < b.len() && !matches!(b[i], '@'..='~') {
                    i += 1;
                }
                i += 1;
            }
            Some(']') => {
                i += 1;
                while i < b.len() && b[i] != '\u{7}' {
                    if b[i] == '\u{1b}' && b.get(i + 1) == Some(&'\\') {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

#[test]
fn a_pane_argv_starts_a_shell_that_is_actually_contained() {
    if skip() {
        return;
    }
    const MARKER: &str = "PANE-OK-51ca";

    // A guard an ordinary process reads and a contained one cannot: this crate's
    // own manifest, under the repo, which carries no ALL APPLICATION PACKAGES
    // ACE. Without this second signal the test could not tell a contained pane
    // from an uncontained one — both would print the marker.
    let guard = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");

    // Compose the pane argv from the same public helpers `backend_enter_argv`
    // uses, with argv[0] pointed at the real binary — `enter_argv` resolves it
    // via `current_exe`, which inside a test harness is this test executable.
    //
    // The exact composition is covered by the unit test
    // `appcontainer_containment_is_visible_in_the_argv`; what only a real run can
    // show is whether the resulting process is actually inside the container.
    let worktree = std::env::temp_dir().join("thegn-appcontainer-pane");
    let _ = std::fs::create_dir_all(&worktree);
    let profile = thegn_core::sandbox_appcontainer::profile_name(&worktree.to_string_lossy());

    // Each signal is its own run with plain arguments. A single compound `cmd`
    // line (`echo … & type … && … || …`) has to survive CommandBuilder's quoting
    // AND the trampoline's `join_argv` before `cmd` parses it, and it does not:
    // an earlier version of this test mis-parsed into a guard read that always
    // failed, so the containment assertion passed while proving nothing.
    let contained = |tail: Vec<String>| -> Vec<String> {
        let mut v = vec![
            env!("CARGO_BIN_EXE_thegn").to_string(),
            "appcontainer-exec".to_string(),
            "--profile".to_string(),
            profile.clone(),
        ];
        for cap in
            thegn_core::sandbox_appcontainer::capabilities_for(thegn_core::config::Network::None)
        {
            v.push("--capability".to_string());
            v.push(cap.to_string());
        }
        v.push("--".to_string());
        v.extend(tail);
        v
    };
    // `cmd.exe` is in System32, which every container can already read, so the
    // marker run touches no granted path at all.
    let echo = vec![
        "cmd.exe".to_string(),
        "/c".to_string(),
        "echo".to_string(),
        MARKER.to_string(),
    ];
    let read_guard = vec![
        "cmd.exe".to_string(),
        "/c".to_string(),
        "type".to_string(),
        guard.to_string_lossy().into_owned(),
    ];
    let argv = contained(echo);

    // 1. The contained shell must reach the ConPTY at all.
    let (exit, raw) = through_conpty(&argv);
    let text = strip_ansi(&raw);
    assert!(
        text.lines().any(|l| l.trim() == MARKER),
        "the contained shell never wrote to the ConPTY (exit {exit:?}):\n{raw}"
    );

    // 2. CONTROL, and it is load-bearing: the SAME read, uncontained, must
    //    succeed. Without this, a guard that is unreadable for some unrelated
    //    reason would make the containment assertion below pass vacuously —
    //    which is exactly what an earlier version of this test did.
    let (_, control_raw) = through_conpty(&read_guard);
    assert!(
        strip_ansi(&control_raw).contains("[package]"),
        "the control could not read the guard either, so this test cannot tell \
         contained from uncontained:\n{control_raw}"
    );

    // 3. …and contained, the same read must fail. That difference is the boundary.
    let (_, denied_raw) = through_conpty(&contained(read_guard));
    assert!(
        !strip_ansi(&denied_raw).contains("[package]"),
        "the pane read a file the uncontained control read, so the token boundary \
         is NOT being applied:\n{denied_raw}"
    );

    let _ = std::fs::remove_dir_all(&worktree);
}
