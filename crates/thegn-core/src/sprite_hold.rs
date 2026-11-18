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
hold_loop() {
    nap=
    trap 'kill "$nap" 2>/dev/null; exit 0' TERM
    while :; do
        sleep 60 &
        nap=$!
        wait "$nap"
        [ "$(date +%s)" -lt "$end" ] || exit 0
        kill -0 "$parent" 2>/dev/null || exit 0
        hold_put
    done
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
        wait "$child"
        rc=$?
        child=
    else
        rc=$2
    fi
    cleanup
    exit "$rc"
}
trap 'on_signal HUP 129' HUP
trap 'on_signal INT 130' INT
trap 'on_signal TERM 143' TERM
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
        sock: PathBuf,
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
        let sock = dir.path().join("api.sock");
        let path = if fixture.curl {
            let curl = bin.join("curl");
            std::fs::write(
                &curl,
                format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
            )
            .expect("fake curl");
            if crate::fsperm::make_executable_for_test(&curl).is_err() {
                return None;
            }
            // The fake shadows any real curl; `date`/`sleep` come from the host.
            let host = std::env::var("PATH").unwrap_or_default();
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn /bin/sh");
        let out = child.wait_with_output().expect("wait for the wrapper");
        let curl_log = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        Some(Ran {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            curl_log,
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
        // wrapper must TERM for the run to end promptly.
        let Some(ran) = run_wrapped(
            HELD,
            &["/bin/sh", "-c", "kill -TERM \"$PPID\"; exec sleep 30"],
        ) else {
            return;
        };
        assert_eq!(ran.code, Some(128 + 15), "stderr: {}", ran.stderr);
        assert_put_then_delete(&ran);
    }
}
