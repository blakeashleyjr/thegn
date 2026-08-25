//! The log/diagnostics redaction chokepoint.
//!
//! Every argv or environment map that reaches a log line, a crash report, or a
//! debug bundle passes through here first: values whose key is on the sensitive
//! list — or the value half of a secret-bearing command-line flag — become a
//! fixed placeholder. Redaction is best-effort pattern matching; the primary
//! rule enforced at the call sites is *don't log secret-bearing shapes at all*
//! (a spawned command is logged by program name + argument count at DEBUG,
//! never full argv).
//!
//! The canonical config-key list, `is_sensitive` predicate and placeholder now
//! live in [`crate::redact`]; this module reuses them and layers on only the
//! extra concerns of the log/argv domain (CLI `-` separators, auth-header
//! keywords, argv/env shapes).

/// The placeholder a redacted value is replaced with — the single
/// [`crate::redact::PLACEHOLDER`] marker, so every leak surface reads alike.
pub const REDACTED: &str = crate::redact::PLACEHOLDER;

/// Log/argv-domain sensitive keywords *in addition to* the canonical config-key
/// list ([`crate::redact::SENSITIVE`]): auth-header shapes that ride on command
/// lines and environment variables but are not themselves config-key names.
const LOG_EXTRA_SENSITIVE: &[&str] = &["bearer", "auth"];

/// Does this key name look like it holds a secret value? Delegates to the
/// canonical [`crate::redact::is_sensitive`] (the shared config-key list plus
/// the `_key` suffix rule) after normalizing CLI `-` separators to `_`, then
/// adds the log-domain extras in [`LOG_EXTRA_SENSITIVE`].
pub fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase().replace('-', "_");
    crate::redact::is_sensitive(&k) || LOG_EXTRA_SENSITIVE.iter().any(|s| k.contains(s))
}

/// Redact the values of an environment map (`(name, value)` pairs), returning a
/// new vector safe to log. A value under a sensitive key becomes [`REDACTED`].
pub fn redact_env_pairs(env: &[(String, String)]) -> Vec<(String, String)> {
    env.iter()
        .map(|(k, v)| {
            if is_sensitive_key(k) {
                (k.clone(), REDACTED.to_string())
            } else {
                (k.clone(), v.clone())
            }
        })
        .collect()
}

/// Redact an argv for logging. Handles three secret-bearing shapes:
///
/// * `--token VALUE` / `--api-key VALUE` — the value in the *following* token
///   is redacted (only when it does not itself look like a flag),
/// * `--token=VALUE` — the value after `=` is redacted,
/// * `FOO_TOKEN=VALUE` — an inline env-assignment whose key is sensitive.
///
/// The program name (`argv[0]`) is never treated as a value. Best-effort: a
/// secret passed as a bare positional (`mytool sk-abcd`) cannot be recognized
/// by shape and is *not* redacted — which is exactly why the DEBUG log site
/// records only program name + arg count, never the full argv.
pub fn redact_argv(argv: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(argv.len());
    let mut redact_next = false;
    for (i, tok) in argv.iter().enumerate() {
        if redact_next {
            // The previous token was a bare sensitive flag; this is its value —
            // unless it is itself a flag (the previous flag took no value).
            redact_next = false;
            if !tok.starts_with('-') {
                out.push(REDACTED.to_string());
                continue;
            }
        }

        // `argv[0]` is the program; never a value or an assignment to redact.
        if i == 0 {
            out.push(tok.clone());
            continue;
        }

        if let Some((lhs, _rhs)) = tok.split_once('=') {
            // `--token=VALUE`, `-token=VALUE`, or `FOO_TOKEN=VALUE`.
            let key = lhs.trim_start_matches('-');
            if is_sensitive_key(key) {
                out.push(format!("{lhs}={REDACTED}"));
                continue;
            }
            out.push(tok.clone());
            continue;
        }

        if tok.starts_with('-') {
            let key = tok.trim_start_matches('-');
            if is_sensitive_key(key) {
                // Value (if any) is in the next token.
                redact_next = true;
            }
        }
        out.push(tok.clone());
    }
    out
}

/// A one-line, secret-free summary of a spawned command: the program's base
/// name and the number of arguments — the shape logged at DEBUG. Never renders
/// argument values.
pub fn command_summary(argv: &[String]) -> String {
    let prog = argv.first().map(String::as_str).unwrap_or("<empty>");
    // Base name only — a full path can carry a home dir / user name.
    let base = std::path::Path::new(prog)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| prog.to_string());
    let argc = argv.len().saturating_sub(1);
    format!("{base} ({argc} args)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_key_matches_the_shapes() {
        assert!(is_sensitive_key("token"));
        assert!(is_sensitive_key("GITHUB_TOKEN"));
        assert!(is_sensitive_key("api_key"));
        assert!(is_sensitive_key("api-key")); // dash normalized
        assert!(is_sensitive_key("openai_api_key"));
        assert!(is_sensitive_key("PASSWORD"));
        assert!(is_sensitive_key("db_password"));
        assert!(is_sensitive_key("private_key"));
        assert!(is_sensitive_key("some_key")); // _key suffix
        assert!(is_sensitive_key("BEARER"));
        assert!(!is_sensitive_key("path"));
        assert!(!is_sensitive_key("cwd"));
        assert!(!is_sensitive_key("keyboard")); // contains "key" but not _key suffix, no sensitive substr
    }

    #[test]
    fn env_pairs_redacts_only_sensitive() {
        let env = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("GITHUB_TOKEN".to_string(), "ghp_secret".to_string()),
            ("HOME".to_string(), "/home/x".to_string()),
        ];
        let out = redact_env_pairs(&env);
        assert_eq!(out[0], ("PATH".into(), "/usr/bin".into()));
        assert_eq!(out[1], ("GITHUB_TOKEN".into(), REDACTED.into()));
        assert_eq!(out[2], ("HOME".into(), "/home/x".into()));
    }

    #[test]
    fn argv_redacts_space_separated_flag_value() {
        let argv = vec![
            "mytool".to_string(),
            "--token".to_string(),
            "sk-abcd".to_string(),
            "--verbose".to_string(),
        ];
        let out = redact_argv(&argv);
        assert_eq!(out, vec!["mytool", "--token", REDACTED, "--verbose"]);
    }

    #[test]
    fn argv_redacts_equals_flag_value() {
        let argv = vec![
            "mytool".to_string(),
            "--api-key=sk-abcd".to_string(),
            "positional".to_string(),
        ];
        let out = redact_argv(&argv);
        assert_eq!(
            out,
            vec!["mytool", "--api-key=***redacted***", "positional"]
        );
    }

    #[test]
    fn argv_redacts_inline_env_assignment() {
        let argv = vec![
            "env".to_string(),
            "FOO_TOKEN=sk-abcd".to_string(),
            "PATH=/usr/bin".to_string(),
            "run".to_string(),
        ];
        let out = redact_argv(&argv);
        assert_eq!(
            out,
            vec!["env", "FOO_TOKEN=***redacted***", "PATH=/usr/bin", "run"]
        );
    }

    #[test]
    fn argv_flag_with_no_value_does_not_eat_next_flag() {
        // `--token` immediately followed by another flag: no value to redact.
        let argv = vec![
            "mytool".to_string(),
            "--token".to_string(),
            "--other".to_string(),
        ];
        let out = redact_argv(&argv);
        assert_eq!(out, vec!["mytool", "--token", "--other"]);
    }

    #[test]
    fn argv0_is_never_redacted_even_if_it_looks_sensitive() {
        let argv = vec!["/opt/token=x".to_string(), "run".to_string()];
        let out = redact_argv(&argv);
        assert_eq!(out[0], "/opt/token=x");
    }

    #[test]
    fn command_summary_is_base_name_and_count() {
        let argv = vec![
            "/usr/local/bin/mytool".to_string(),
            "--token".to_string(),
            "sk-abcd".to_string(),
        ];
        assert_eq!(command_summary(&argv), "mytool (2 args)");
        assert_eq!(command_summary(&[]), "<empty> (0 args)");
    }
}
