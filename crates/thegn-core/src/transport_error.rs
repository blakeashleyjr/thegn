//! Pure classification of control-plane exec failures: transient (network
//! flap — worth retrying) vs permanent (auth/config/command error — retrying
//! is futile).
//!
//! The problem this solves: ssh exits 255 for *every* client-side failure and
//! often says nothing on stderr when the transport drops mid-stream. The old
//! `err_tail` rendered that as the useless literal `"(no output)"`, and a
//! single flap was treated exactly like a permanent auth failure. This module
//! is the one place that reads the tea leaves — exit code + timeout flag +
//! stderr patterns — and every retry ladder (host bring-up, delivery, probes)
//! keys off [`ErrorClass`].

/// Whether a failed exec is worth retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Network flap / timeout — retry with backoff.
    Transient,
    /// Auth, DNS, host key, missing binary, or the remote command itself
    /// failed — retrying without operator action won't help.
    Permanent,
}

/// A failure message carrying its retry classification — the error type of
/// the host-runner seam, so retry ladders upstream know whether another
/// attempt is worth it without re-parsing strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedErr {
    pub class: ErrorClass,
    pub msg: String,
}

impl ClassifiedErr {
    pub fn transient(msg: impl Into<String>) -> Self {
        ClassifiedErr {
            class: ErrorClass::Transient,
            msg: msg.into(),
        }
    }

    pub fn permanent(msg: impl Into<String>) -> Self {
        ClassifiedErr {
            class: ErrorClass::Permanent,
            msg: msg.into(),
        }
    }

    pub fn is_transient(&self) -> bool {
        self.class == ErrorClass::Transient
    }
}

impl std::fmt::Display for ClassifiedErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

/// A bare string error has no transport evidence — treat it as permanent
/// (config/parse/logic errors), so `?` on legacy `Result<_, String>` helpers
/// composes without accidentally making them retryable.
impl From<String> for ClassifiedErr {
    fn from(msg: String) -> Self {
        ClassifiedErr::permanent(msg)
    }
}

impl From<&str> for ClassifiedErr {
    fn from(msg: &str) -> Self {
        ClassifiedErr::permanent(msg)
    }
}

/// stderr substrings (lowercased match) that make an exit-255 failure
/// permanent: the transport *worked* — the refusal is durable.
const PERMANENT_255_PATTERNS: &[&str] = &[
    "permission denied",
    "host key verification failed",
    "could not resolve hostname",
    "no matching host key",
    "too many authentication failures",
    "no route to host", // routing misconfig outlives a retry ladder
    "name or service not known",
];

/// Classify a finished control-plane exec.
///
/// * `exit_code` — `None` means the process never ran (spawn failure).
/// * `timed_out` — the caller's deadline killed it.
/// * `stderr` — captured stderr (may be empty; mid-stream drops usually are).
pub fn classify_exec(exit_code: Option<i32>, timed_out: bool, stderr: &str) -> ErrorClass {
    if timed_out {
        return ErrorClass::Transient;
    }
    match exit_code {
        // ssh's catch-all client failure: transient unless stderr names a
        // durable refusal.
        Some(255) => {
            let low = stderr.to_lowercase();
            if PERMANENT_255_PATTERNS.iter().any(|p| low.contains(p)) {
                ErrorClass::Permanent
            } else {
                ErrorClass::Transient
            }
        }
        // Spawn failure (ssh binary missing / exec error): permanent.
        None => ErrorClass::Permanent,
        // Any other exit means the remote command actually ran and failed —
        // that's a real answer, not a flap.
        Some(_) => ErrorClass::Permanent,
    }
}

/// One-line human error for a failed exec, naming the real cause. Replaces the
/// information `err_tail` threw away: exit code, timeout, and transport
/// classification. `label` is the step ("connect", "probe", "offset query").
pub fn describe_exec_failure(
    label: &str,
    exit_code: Option<i32>,
    timed_out: bool,
    stderr: &str,
) -> String {
    let tail = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(str::trim)
        .unwrap_or("");
    if timed_out {
        return if tail.is_empty() {
            format!("{label}: ssh timed out — slow or lossy link?")
        } else {
            format!("{label}: ssh timed out — {tail}")
        };
    }
    match exit_code {
        Some(255) if tail.is_empty() => {
            format!("{label}: ssh transport dropped (exit 255, no stderr) — network flap?")
        }
        Some(255) => match classify_exec(exit_code, false, stderr) {
            ErrorClass::Permanent => format!("{label}: ssh refused — {tail}"),
            ErrorClass::Transient => format!("{label}: ssh transport error (exit 255) — {tail}"),
        },
        None => {
            if tail.is_empty() {
                format!("{label}: could not spawn ssh")
            } else {
                format!("{label}: could not spawn ssh — {tail}")
            }
        }
        Some(code) if tail.is_empty() => {
            format!("{label}: remote command failed (exit {code}, no stderr)")
        }
        Some(code) => format!("{label}: remote command failed (exit {code}) — {tail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_table() {
        use ErrorClass::*;
        // (exit, timed_out, stderr) → class
        let table: &[(Option<i32>, bool, &str, ErrorClass)] = &[
            // Timeouts are always transient, whatever else is going on.
            (Some(255), true, "", Transient),
            (Some(1), true, "boom", Transient),
            (None, true, "", Transient),
            // 255 + silence or generic noise = flap.
            (Some(255), false, "", Transient),
            (
                Some(255),
                false,
                "Connection closed by 1.2.3.4 port 22",
                Transient,
            ),
            (
                Some(255),
                false,
                "kex_exchange_identification: read: Connection reset",
                Transient,
            ),
            (
                Some(255),
                false,
                "Timeout, server not responding",
                Transient,
            ),
            // 255 + durable refusal = permanent.
            (
                Some(255),
                false,
                "user@host: Permission denied (publickey).",
                Permanent,
            ),
            (Some(255), false, "Host key verification failed.", Permanent),
            (
                Some(255),
                false,
                "ssh: Could not resolve hostname bogus",
                Permanent,
            ),
            (
                Some(255),
                false,
                "Too many authentication failures",
                Permanent,
            ),
            (
                Some(255),
                false,
                "connect to host x: No route to host",
                Permanent,
            ),
            (
                Some(255),
                false,
                "ssh: x: Name or service not known",
                Permanent,
            ),
            // Spawn failure = permanent (ssh missing).
            (None, false, "", Permanent),
            // Remote command ran and failed = permanent (a real answer).
            (Some(1), false, "some error", Permanent),
            (Some(127), false, "sh: podman: not found", Permanent),
        ];
        for (code, to, err, want) in table {
            assert_eq!(
                classify_exec(*code, *to, err),
                *want,
                "exit={code:?} timed_out={to} stderr={err:?}"
            );
        }
    }

    #[test]
    fn describe_flap_names_the_drop() {
        let s = describe_exec_failure("connect", Some(255), false, "");
        assert!(s.contains("transport dropped"), "{s}");
        assert!(s.contains("exit 255"), "{s}");
        assert!(!s.contains("(no output)"), "{s}");
    }

    #[test]
    fn describe_timeout() {
        let s = describe_exec_failure("probe", None, true, "");
        assert!(s.starts_with("probe: ssh timed out"), "{s}");
        let s2 = describe_exec_failure("probe", Some(255), true, "partial write");
        assert!(s2.contains("partial write"), "{s2}");
    }

    #[test]
    fn describe_permanent_refusal() {
        let s = describe_exec_failure(
            "connect",
            Some(255),
            false,
            "Permission denied (publickey).",
        );
        assert!(s.contains("ssh refused"), "{s}");
        assert!(s.contains("Permission denied"), "{s}");
    }

    #[test]
    fn describe_transient_with_stderr() {
        let s = describe_exec_failure(
            "connect",
            Some(255),
            false,
            "Connection closed by 100.104.99.124 port 22",
        );
        assert!(s.contains("ssh transport error (exit 255)"), "{s}");
        assert!(s.contains("Connection closed"), "{s}");
    }

    #[test]
    fn describe_remote_command_failure() {
        let s = describe_exec_failure("probe", Some(3), false, "line1\nreal cause here\n\n");
        assert!(s.contains("exit 3"), "{s}");
        assert!(s.contains("real cause here"), "{s}");
        let quiet = describe_exec_failure("probe", Some(4), false, "");
        assert!(quiet.contains("exit 4, no stderr"), "{quiet}");
    }

    #[test]
    fn describe_spawn_failure() {
        let s = describe_exec_failure("connect", None, false, "");
        assert!(s.contains("could not spawn ssh"), "{s}");
        let s2 = describe_exec_failure("connect", None, false, "No such file or directory");
        assert!(s2.contains("No such file"), "{s2}");
    }

    #[test]
    fn classified_err_construction_and_from() {
        let t = ClassifiedErr::transient("flap");
        assert!(t.is_transient());
        assert_eq!(t.to_string(), "flap");
        let p = ClassifiedErr::permanent("denied");
        assert!(!p.is_transient());
        // Bare strings (legacy `Result<_, String>` helpers) are permanent.
        let from: ClassifiedErr = String::from("parse error").into();
        assert_eq!(from.class, ErrorClass::Permanent);
        let from: ClassifiedErr = "static".into();
        assert_eq!(from.class, ErrorClass::Permanent);
    }

    #[test]
    fn describe_takes_last_nonempty_stderr_line() {
        let s = describe_exec_failure("x", Some(1), false, "warning: a\nerror: the cause\n  \n");
        assert!(s.contains("error: the cause"), "{s}");
        assert!(!s.contains("warning"), "{s}");
    }
}
