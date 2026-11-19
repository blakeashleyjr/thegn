//! Sprites keep-awake holds: the pure policy shared by the host bridge
//! (`thegn sprite-exec --hold`) and the svc Tasks-API client.
//!
//! A headless command on a sprite is wrapped in a POSIX `sh` heartbeat that
//! registers a task named `thegn-hold-<16 hex>` with a [`HOLD_EXPIRE_SECS`]
//! expiry, refreshes it every [`HOLD_REFRESH_SECS`] while the command runs, and
//! stops refreshing at a ceiling. Interactive panes get no hold.
//!
//! The command rides the script as positional arguments (`"$@"`), so no argv
//! byte is ever re-parsed by a shell. Worst-case spend after a crash is the
//! ceiling plus [`HOLD_EXPIRE_SECS`]; nothing on the host polls.

/// Expiry of one hold registration: worst-case spend after a crash is the
/// ceiling plus this.
pub const HOLD_EXPIRE_SECS: u64 = 300;
/// How often the in-sprite heartbeat refreshes the hold.
pub const HOLD_REFRESH_SECS: u64 = 60;
/// The ceiling when `[env.<n>.provider] max_lifetime_secs` is unset.
pub const HOLD_DEFAULT_CEILING_SECS: u64 = 4 * 3600;
/// The largest ceiling `--hold` may request.
pub const HOLD_MAX_CEILING_SECS: u64 = 24 * 3600;
/// Host-side only: the hold duration `sprite-exec` reads from its environment.
pub const HOLD_ENV: &str = "THEGN_SPRITE_HOLD_SECS";

/// The task name for a hold nonce: `thegn-hold-<16 lowercase hex>`. `None` when
/// the nonce is not exactly 16 lowercase hex digits.
pub fn hold_name(nonce_hex: &str) -> Option<String> {
    let valid = nonce_hex.len() == 16
        && nonce_hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    valid.then(|| format!("thegn-hold-{nonce_hex}"))
}

/// The hold ceiling: an explicit request (capped at [`HOLD_MAX_CEILING_SECS`]),
/// else `max_lifetime_secs` when set (same cap), else
/// [`HOLD_DEFAULT_CEILING_SECS`].
pub fn ceiling_secs(requested: Option<u64>, max_lifetime_secs: u64) -> u64 {
    match requested {
        Some(secs) => secs.min(HOLD_MAX_CEILING_SECS),
        None if max_lifetime_secs > 0 => max_lifetime_secs.min(HOLD_MAX_CEILING_SECS),
        None => HOLD_DEFAULT_CEILING_SECS,
    }
}

/// `$0` the heartbeat script runs under (`sh -c SCRIPT $0 task ceiling argv…`).
pub const HOLD_ARG0: &str = "thegn-hold";

/// The POSIX `sh` heartbeat (no bashisms). Positional arguments:
/// `$1` the task name, `$2` the ceiling in seconds, `$3…` the command.
///
/// - When `curl` is on `PATH` and the sprite API socket exists, it `PUT`s the
///   task with a 300 s expiry ([`HOLD_EXPIRE_SECS`]) once, then a background
///   loop re-`PUT`s it every 60 s ([`HOLD_REFRESH_SECS`]) until the ceiling
///   passes or this shell is gone. Otherwise it prints one line to stderr and
///   runs the command unheld.
/// - The command runs as a child; its exit code is passed through. On exit
///   (and on HUP/INT/TERM, which are forwarded to the child first) the loop is
///   stopped and the task is `DELETE`d, best-effort.
/// - The loop and the command are forked before any trap is installed, so a
///   signal sent to either before it first runs cannot be lost to an inherited
///   handler. A signal reaching the wrapper in that short startup window ends
///   it without cleanup: the loop sees its parent gone within 60 s and the
///   hold expires 300 s later (the crash bound).
/// - The loop never holds the command's stdout/stderr (it runs on
///   `/dev/null`), and it kills and reaps its own `sleep` before exiting.
/// - `THEGN_HOLD_SOCK` overrides the socket path. It exists for thegn's own
///   tests (a fake socket); nothing in production sets it.
pub const HOLD_SCRIPT: &str = r#"task=$1
ceiling=$2
shift 2
# THEGN_HOLD_SOCK: test-only override of the sprite API socket path.
sock=${THEGN_HOLD_SOCK:-/.sprite/api.sock}
case $ceiling in '' | *[!0-9]*) ceiling=0 ;; esac
case $task in '' | *[!a-z0-9-]*) valid= ;; *) valid=1 ;; esac
loop=
held=
child=
rc=0
hold_put() {
    curl -fsS --max-time 10 --unix-socket "$sock" -X PUT \
        -H 'content-type: application/json' -d '{"expire":"300s"}' \
        "http://sprite/v1/tasks/$task" >/dev/null
}
hold_drop() {
    curl -fsS --max-time 10 --unix-socket "$sock" -X DELETE \
        "http://sprite/v1/tasks/$task" >/dev/null 2>&1
}
# The heartbeat. It is forked before this shell installs any trap, so a TERM
# reaching it before its own trap exists kills it (default action) instead of
# being swallowed by an inherited handler. The trap only records the stop and
# ends the current nap; every step is bounded, so a TERM at any point ends the
# loop promptly, and the nap is always killed and reaped before it exits.
hold_loop() {
    stop=
    nap=
    trap 'stop=1; [ -z "$nap" ] || kill "$nap" 2>/dev/null' TERM
    while [ -z "$stop" ]; do
        nap=
        sleep 60 &
        nap=$!
        [ -n "$stop" ] || wait "$nap"
        [ -z "$stop" ] || break
        [ "$(date +%s)" -lt "$end" ] || break
        kill -0 "$parent" 2>/dev/null || break
        hold_put
    done
    if [ -n "$stop" ] && [ -n "$nap" ]; then
        kill "$nap" 2>/dev/null
        wait "$nap" 2>/dev/null
    fi
    exit 0
}
cleanup() {
    if [ -n "$loop" ]; then
        kill "$loop" 2>/dev/null
        wait "$loop" 2>/dev/null
        loop=
    fi
    if [ -n "$held" ]; then
        hold_drop
        held=
    fi
}
on_signal() {
    if [ -n "$child" ]; then
        kill -s "$1" "$child" 2>/dev/null
        # An async child ignores INT (POSIX); escalate so an interrupt ends it.
        if [ "$1" = INT ]; then kill -s TERM "$child" 2>/dev/null; fi
        wait "$child"
        rc=$?
        child=
    else
        rc=$2
    fi
    cleanup
    exit "$rc"
}
if [ -n "$valid" ] && command -v curl >/dev/null 2>&1 && [ -S "$sock" ]; then
    held=1
    hold_put
    parent=$$
    end=$(( $(date +%s) + ceiling ))
    hold_loop </dev/null >/dev/null 2>&1 &
    loop=$!
else
    echo 'thegn: sprite keep-awake unavailable (no curl or task socket); running unheld' >&2
fi
exec 3<&0
"$@" <&3 3<&- &
child=$!
# Traps go in only now, with both pids recorded: a process forked while a trap
# was installed could lose a signal sent to it before it first ran.
trap 'on_signal HUP 129' HUP
trap 'on_signal INT 130' INT
trap 'on_signal TERM 143' TERM
wait "$child"
rc=$?
child=
cleanup
exit "$rc"
"#;

/// Wrap `argv` in the heartbeat script:
/// `["/bin/sh","-c",HOLD_SCRIPT,"thegn-hold",task,ceiling,argv...]`.
///
/// `argv` is appended byte-for-byte as positional arguments: the script only
/// ever expands it as `"$@"`, so spaces, quotes and `$` reach the command
/// unchanged. `task` should come from [`hold_name`]; the script refuses
/// (runs unheld) a name outside `[a-z0-9-]`.
pub fn wrap_argv(argv: &[String], task: &str, ceiling_secs: u64) -> Vec<String> {
    let mut wrapped = Vec::with_capacity(argv.len() + 6);
    wrapped.extend([
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        HOLD_SCRIPT.to_owned(),
        HOLD_ARG0.to_owned(),
        task.to_owned(),
        ceiling_secs.to_string(),
    ]);
    wrapped.extend_from_slice(argv);
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    const TASK: &str = "thegn-hold-0123456789abcdef";

    #[test]
    fn hold_name_accepts_only_sixteen_lowercase_hex() {
        assert_eq!(hold_name("0123456789abcdef").as_deref(), Some(TASK));
        assert_eq!(hold_name("0123456789ABCDEF"), None);
        assert_eq!(hold_name("0123"), None);
        assert_eq!(hold_name("0123456789abcdeg"), None);
        assert_eq!(hold_name("0123456789abcdef0"), None);
        assert_eq!(hold_name(""), None);
        assert_eq!(hold_name("0123456789abcde "), None);
        assert_eq!(hold_name("01234567/9abcdef"), None);
    }

    #[test]
    fn ceiling_prefers_request_then_lifetime_then_default() {
        let table = [
            (Some(60), 10, 60),
            (Some(0), 900, 0),
            (Some(u64::MAX), 0, HOLD_MAX_CEILING_SECS),
            (Some(HOLD_MAX_CEILING_SECS + 1), 0, HOLD_MAX_CEILING_SECS),
            (None, 900, 900),
            (None, u64::MAX, HOLD_MAX_CEILING_SECS),
            (None, 0, HOLD_DEFAULT_CEILING_SECS),
        ];
        for (requested, lifetime, want) in table {
            assert_eq!(
                ceiling_secs(requested, lifetime),
                want,
                "ceiling_secs({requested:?}, {lifetime})"
            );
        }
    }

    #[test]
    fn script_expands_argv_only_as_quoted_positionals() {
        // The command reaches the shell only as "$@": never `$*`, a bare
        // `$@`, a numbered positional past the two the script owns, or eval.
        assert_eq!(HOLD_SCRIPT.matches("\"$@\"").count(), 1);
        assert_eq!(HOLD_SCRIPT.matches("$@").count(), 1);
        assert!(!HOLD_SCRIPT.contains("$*"));
        assert!(!HOLD_SCRIPT.contains("eval"));
        for n in 3..=9 {
            assert!(!HOLD_SCRIPT.contains(&format!("${n}")), "${n} in script");
        }
        // Every `$(` is a fixed command, never one built from argv.
        for (at, _) in HOLD_SCRIPT.match_indices("$(") {
            let rest = &HOLD_SCRIPT[at..];
            assert!(
                rest.starts_with("$(date +%s)") || rest.starts_with("$(( $(date +%s)"),
                "unexpected command substitution: {}",
                &rest[..rest.len().min(30)]
            );
        }
    }

    #[test]
    fn script_carries_the_policy_constants() {
        assert!(HOLD_SCRIPT.contains(&format!("sleep {HOLD_REFRESH_SECS} &")));
        assert!(HOLD_SCRIPT.contains(&format!("{{\"expire\":\"{HOLD_EXPIRE_SECS}s\"}}")));
        assert!(HOLD_SCRIPT.contains("${THEGN_HOLD_SOCK:-/.sprite/api.sock}"));
        assert!(HOLD_SCRIPT.contains("http://sprite/v1/tasks/$task"));
        // POSIX sh: no bashisms.
        for bashism in ["[[", "function ", "local ", "$'", "<<<", "pipefail"] {
            assert!(!HOLD_SCRIPT.contains(bashism), "bashism {bashism:?}");
        }
    }

    #[test]
    fn wrap_argv_keeps_argv_order_and_bytes() {
        let argv: Vec<String> = [
            "claude",
            "-p",
            "two words",
            "it's \"quoted\"",
            "$HOME and $(rm -rf /)",
            "",
            "back\\slash",
        ]
        .map(String::from)
        .to_vec();
        let wrapped = wrap_argv(&argv, TASK, 900);
        assert_eq!(
            &wrapped[..6],
            &[
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                HOLD_SCRIPT.to_owned(),
                HOLD_ARG0.to_owned(),
                TASK.to_owned(),
                "900".to_owned(),
            ]
        );
        assert_eq!(&wrapped[6..], argv.as_slice());
    }

    // -----------------------------------------------------------------------
    // The script itself, under /bin/sh, with a fake `curl` that logs its argv.
    // -----------------------------------------------------------------------

    struct Ran {
        code: Option<i32>,
        stdout: String,
        stderr: String,
        curl_log: Vec<String>,
        /// `<pid> <args>` per `sleep` run under the held fixture.
        sleep_log: Vec<String>,
        sock: PathBuf,
    }

    /// A wrapper run that takes longer than this is a hang, not slowness: the
    /// runs here finish in well under a second.
    const RUN_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// The host's real `sleep`, so the fixture can log every nap and exec it.
    fn host_sleep(host_path: &str) -> Option<PathBuf> {
        std::env::split_paths(host_path)
            .map(|dir| dir.join("sleep"))
            .find(|candidate| candidate.is_file())
    }

    #[derive(Clone, Copy)]
    struct Fixture {
        curl: bool,
        socket: bool,
    }

    /// Run `wrap_argv(argv)` under `/bin/sh`. `None` on a runner with no Unix
    /// shell (the fixture is Unix-only by design).
    fn run_wrapped(fixture: Fixture, argv: &[&str]) -> Option<Ran> {
        if !Path::new("/bin/sh").exists() {
            return None;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).expect("bin dir");
        let log = dir.path().join("curl.log");
        let naps = dir.path().join("sleep.log");
        let sock = dir.path().join("api.sock");
        let path = if fixture.curl {
            // A logging `sleep` so tests can see the heartbeat's nap pids.
            let host = std::env::var("PATH").unwrap_or_default();
            let real_sleep = host_sleep(&host)?;
            let sleep = bin.join("sleep");
            std::fs::write(
                &sleep,
                format!(
                    "#!/bin/sh\nprintf '%s %s\\n' \"$$\" \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
                    naps.display(),
                    real_sleep.display()
                ),
            )
            .expect("fake sleep");
            if crate::fsperm::make_executable_for_test(&sleep).is_err() {
                return None;
            }
            let curl = bin.join("curl");
            std::fs::write(
                &curl,
                format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
            )
            .expect("fake curl");
            if crate::fsperm::make_executable_for_test(&curl).is_err() {
                return None;
            }
            // The fakes shadow any real curl/sleep; `date` comes from the host.
            format!("{}:{host}", bin.display())
        } else {
            // Nothing on PATH at all: `command -v curl` must fail.
            bin.display().to_string()
        };
        let _listener = if fixture.socket {
            match crate::testenv::bind_unix_socket_for_test(&sock) {
                Ok(guard) => Some(guard),
                Err(_) => return None,
            }
        } else {
            None
        };
        let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_owned()).collect();
        let wrapped = wrap_argv(&argv, TASK, 900);
        let child = Command::new(&wrapped[0])
            .args(&wrapped[1..])
            .env("PATH", path)
            .env("THEGN_HOLD_SOCK", &sock)
            .env("NAPS", &naps)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn /bin/sh");
        let pid = child.id().to_string();
        // Bounded: a wrapper that never exits, or leaves something holding its
        // output open, fails here with a message instead of hanging the run.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tx.send(child.wait_with_output()).unwrap_or_default(); // best-effort: the receiver may have timed out
        });
        let out = match rx.recv_timeout(RUN_LIMIT) {
            Ok(out) => out.expect("wait for the wrapper"),
            Err(_) => {
                // best-effort: the wrapper may already be gone; the panic below reports the hang
                Command::new("/bin/sh")
                    .args(["-c", "kill -KILL \"$1\" 2>/dev/null", "sh", &pid])
                    .status()
                    .unwrap_or_default();
                panic!(
                    "hold wrapper for {argv:?} did not exit (or kept its output open) \
                     within {RUN_LIMIT:?}: the heartbeat wrapper hung"
                );
            }
        };
        let read_lines = |file: &Path| -> Vec<String> {
            std::fs::read_to_string(file)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        };
        Some(Ran {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            curl_log: read_lines(&log),
            sleep_log: read_lines(&naps),
            sock,
        })
    }

    const HELD: Fixture = Fixture {
        curl: true,
        socket: true,
    };
    const ECHO_ARGS: &str = "printf '<%s>' \"$@\"; exit 7";
    const WARNING: &str =
        "thegn: sprite keep-awake unavailable (no curl or task socket); running unheld";

    fn assert_put_then_delete(ran: &Ran) {
        let sock = ran.sock.display().to_string();
        assert_eq!(ran.curl_log.len(), 2, "curl calls: {:?}", ran.curl_log);
        let put = &ran.curl_log[0];
        assert!(put.contains("-X PUT"), "{put}");
        assert!(put.contains(&format!("--unix-socket {sock}")), "{put}");
        assert!(
            put.contains(&format!("{{\"expire\":\"{HOLD_EXPIRE_SECS}s\"}}")),
            "{put}"
        );
        assert!(
            put.ends_with(&format!("http://sprite/v1/tasks/{TASK}")),
            "{put}"
        );
        let delete = &ran.curl_log[1];
        assert!(delete.contains("-X DELETE"), "{delete}");
        assert!(
            delete.ends_with(&format!("http://sprite/v1/tasks/{TASK}")),
            "{delete}"
        );
    }

    /// Every heartbeat nap (`sleep 60`) is gone once the wrapper has exited:
    /// the loop kills and reaps its own sleep. (Checked through `/proc`; where
    /// there is none the check is vacuous.)
    fn assert_heartbeat_naps_ended(ran: &Ran) {
        let refresh = HOLD_REFRESH_SECS.to_string();
        for line in &ran.sleep_log {
            let Some((pid, args)) = line.split_once(' ') else {
                continue;
            };
            if args != refresh {
                continue;
            }
            let alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|stat| !stat.contains(") Z "));
            assert!(!alive, "heartbeat nap {pid} outlived the wrapper");
        }
    }

    #[test]
    fn held_run_survives_a_command_that_exits_immediately() {
        // The command ends before the heartbeat has necessarily even run; the
        // wrapper must still stop it and exit. Repeated to cover interleavings.
        for _ in 0..10 {
            let Some(ran) = run_wrapped(HELD, &["true"]) else {
                return;
            };
            assert_eq!(ran.code, Some(0), "stderr: {}", ran.stderr);
            assert_put_then_delete(&ran);
            assert_heartbeat_naps_ended(&ran);
        }
    }

    #[test]
    fn held_run_ends_while_the_heartbeat_is_mid_sleep() {
        // The command waits until the heartbeat's nap has started, then exits:
        // the wrapper must end the nap rather than wait out the refresh.
        let Some(ran) = run_wrapped(
            HELD,
            &[
                "/bin/sh",
                "-c",
                "until grep -q ' 60$' \"$NAPS\" 2>/dev/null; do sleep 1; done",
            ],
        ) else {
            return;
        };
        assert_eq!(ran.code, Some(0), "stderr: {}", ran.stderr);
        assert_put_then_delete(&ran);
        assert!(
            ran.sleep_log
                .iter()
                .any(|line| line.ends_with(&format!(" {HOLD_REFRESH_SECS}"))),
            "no heartbeat nap seen: {:?}",
            ran.sleep_log
        );
        assert_heartbeat_naps_ended(&ran);
    }

    #[test]
    fn held_run_puts_then_deletes_and_passes_the_exit_code() {
        let Some(ran) = run_wrapped(
            HELD,
            &[
                "/bin/sh", "-c", ECHO_ARGS, "cmd", "a b", "q\"x'y", "$HOME", "",
            ],
        ) else {
            return;
        };
        assert_eq!(ran.code, Some(7), "stderr: {}", ran.stderr);
        assert_eq!(ran.stdout, "<a b><q\"x'y><$HOME><>");
        assert!(!ran.stderr.contains(WARNING), "{}", ran.stderr);
        assert_put_then_delete(&ran);
    }

    #[test]
    fn held_run_passes_success_through() {
        let Some(ran) = run_wrapped(HELD, &["/bin/sh", "-c", "exit 0"]) else {
            return;
        };
        assert_eq!(ran.code, Some(0), "stderr: {}", ran.stderr);
        assert_put_then_delete(&ran);
        assert_heartbeat_naps_ended(&ran);
    }

    #[test]
    fn no_curl_warns_once_and_still_runs_the_command() {
        let fixture = Fixture {
            curl: false,
            socket: true,
        };
        let Some(ran) = run_wrapped(fixture, &["/bin/sh", "-c", ECHO_ARGS, "cmd", "x y"]) else {
            return;
        };
        assert_eq!(ran.code, Some(7));
        assert_eq!(ran.stdout, "<x y>");
        assert_eq!(ran.stderr.matches(WARNING).count(), 1, "{}", ran.stderr);
        assert_eq!(ran.stderr.trim_end(), WARNING);
        assert!(ran.curl_log.is_empty());
    }

    #[test]
    fn no_socket_warns_once_and_never_calls_curl() {
        let fixture = Fixture {
            curl: true,
            socket: false,
        };
        let Some(ran) = run_wrapped(fixture, &["/bin/sh", "-c", "exit 3"]) else {
            return;
        };
        assert_eq!(ran.code, Some(3));
        assert_eq!(ran.stderr.matches(WARNING).count(), 1, "{}", ran.stderr);
        assert!(ran.curl_log.is_empty(), "{:?}", ran.curl_log);
    }

    #[test]
    fn term_is_forwarded_to_the_child_and_the_hold_dropped() {
        // The child signals its parent (the wrapper), then becomes a sleep the
        // wrapper must TERM for the run to end promptly. The leading pause
        // lets the wrapper install its traps (it does so just after the fork).
        let Some(ran) = run_wrapped(
            HELD,
            &[
                "/bin/sh",
                "-c",
                "sleep 1; kill -TERM \"$PPID\"; exec sleep 30",
            ],
        ) else {
            return;
        };
        assert_eq!(ran.code, Some(128 + 15), "stderr: {}", ran.stderr);
        assert_put_then_delete(&ran);
        assert_heartbeat_naps_ended(&ran);
    }

    #[test]
    fn int_ends_the_child_and_drops_the_hold() {
        // An async child ignores INT, so the wrapper escalates to TERM. The
        // leading pause lets the wrapper install its traps first.
        let Some(ran) = run_wrapped(
            HELD,
            &[
                "/bin/sh",
                "-c",
                "sleep 1; kill -INT \"$PPID\"; exec sleep 30",
            ],
        ) else {
            return;
        };
        assert_eq!(ran.code, Some(143), "stderr: {}", ran.stderr);
        assert_put_then_delete(&ran);
        assert_heartbeat_naps_ended(&ran);
    }
}
