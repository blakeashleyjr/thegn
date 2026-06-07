//! Remote-exec seam (the control plane). The native impl (Phase 5) uses russh
//! with an owned connection pool (explicit channel multiplexing, replacing ssh
//! ControlMaster) and agent/key auth. The `Cli` fallback wraps core's
//! `ssh`-subprocess code and stays the permanent default for targets whose
//! `~/.ssh/config` uses ProxyJump/Match (russh-keys doesn't read ssh_config).
//!
//! Scope note: this is *control plane only* — the interactive remote pane is
//! still `mosh`/`ssh -t` spawned via `superzej_core::sandbox::enter_argv` into a
//! PTY, untouched by this seam.

use anyhow::{Context, Result};
use superzej_core::remote::{SshTarget, remote_home, ssh_base};

#[derive(Debug, Clone)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Build the full control-plane ssh argv for `cmd` on `target`. Reuses core's
/// `ssh_base` (BatchMode + ControlMaster multiplexing — so the CLI fallback
/// already gets the connection reuse the russh pool would provide).
fn ssh_argv(target: &SshTarget, cmd: &str) -> Vec<String> {
    let mut argv = ssh_base(target.port, target.forward_agent, true);
    argv.push(target.host.clone());
    argv.push(cmd.to_string());
    argv
}

/// The permanent fallback: control-plane exec via the `ssh` subprocess. This is
/// also the forced path for hosts whose `~/.ssh/config` uses ProxyJump/Match
/// (see [`config_forces_cli`]) — russh-keys can't read those directives.
pub struct CliSsh;

impl RemoteExec for CliSsh {
    async fn exec(&self, target: &SshTarget, cmd: &str) -> Result<Output> {
        let argv = ssh_argv(target, cmd);
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .with_context(|| format!("ssh {} -- {cmd}", target.host))?;
        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    async fn home(&self, target: &SshTarget) -> Result<String> {
        remote_home(target).context("resolve remote $HOME over ssh")
    }
}

#[allow(async_fn_in_trait)]
pub trait RemoteExec: Send + Sync {
    /// Run a command on the remote host (a fresh channel on a pooled connection).
    async fn exec(&self, target: &SshTarget, cmd: &str) -> Result<Output>;
    /// Resolve the remote `$HOME` (used for default worktree paths).
    async fn home(&self, target: &SshTarget) -> Result<String>;
}

/// Decide whether a host must use the `ssh`-subprocess fallback rather than the
/// native russh backend. russh-keys does NOT read `~/.ssh/config`, so any host
/// whose effective config relies on `ProxyJump`/`ProxyCommand` (jump hosts) or a
/// `Match` block can't be reproduced natively — route it to the CLI. This is the
/// permanent fallback gate from the plan.
///
/// Pure over the config *text* so it's unit-testable; the runtime reads
/// `~/.ssh/config` and calls this.
pub fn config_forces_cli(config: &str, host: &str) -> bool {
    // Any Match block at all means host-resolution depends on logic russh can't
    // see — be conservative and fall back.
    let mut in_matching_host = false;
    for raw in config.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let (kw, rest) = lower
            .split_once(char::is_whitespace)
            .unwrap_or((lower.as_str(), ""));
        match kw {
            "match" => return true,
            "host" => {
                // Patterns on the `Host` line; does any match our host?
                in_matching_host = rest
                    .split_whitespace()
                    .any(|pat| host_pattern_matches(pat, host));
            }
            "proxyjump" | "proxycommand" if in_matching_host => return true,
            _ => {}
        }
    }
    false
}

/// Minimal ssh_config Host-pattern match (`*` and `?` wildcards, `!` negation).
fn host_pattern_matches(pattern: &str, host: &str) -> bool {
    if let Some(neg) = pattern.strip_prefix('!') {
        return !glob_match(neg, host);
    }
    glob_match(pattern, host)
}

fn glob_match(pat: &str, s: &str) -> bool {
    // Tiny glob: `*` any run, `?` one char. Sufficient for ssh Host patterns.
    fn m(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') => m(&p[1..], s) || (!s.is_empty() && m(p, &s[1..])),
            Some(b'?') => !s.is_empty() && m(&p[1..], &s[1..]),
            Some(&c) => !s.is_empty() && s[0] == c && m(&p[1..], &s[1..]),
        }
    }
    m(pat.as_bytes(), s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_host_does_not_force_cli() {
        let cfg = "Host build\n  HostName 10.0.0.5\n  User ci\n";
        assert!(!config_forces_cli(cfg, "build"));
    }

    #[test]
    fn proxyjump_under_a_matching_host_forces_cli() {
        let cfg = "Host build\n  ProxyJump bastion\n  User ci\n";
        assert!(config_forces_cli(cfg, "build"));
        // A different host, unaffected by that block.
        assert!(!config_forces_cli(cfg, "other"));
    }

    #[test]
    fn wildcard_host_pattern_matches() {
        let cfg = "Host *.internal\n  ProxyCommand nc -x proxy %h %p\n";
        assert!(config_forces_cli(cfg, "db.internal"));
        assert!(!config_forces_cli(cfg, "db.external"));
    }

    #[test]
    fn any_match_block_forces_cli() {
        let cfg = "Match host build exec \"true\"\n  ProxyJump bastion\n";
        assert!(config_forces_cli(cfg, "build"));
        assert!(config_forces_cli(cfg, "anything"));
    }

    #[test]
    fn ssh_argv_appends_host_and_cmd_and_handles_port() {
        let t = SshTarget {
            host: "build".into(),
            port: 22,
            forward_agent: false,
        };
        let argv = ssh_argv(&t, "git status");
        assert_eq!(argv[0], "ssh");
        assert!(!argv.contains(&"-p".to_string()), "no -p for default port");
        assert_eq!(argv[argv.len() - 2], "build");
        assert_eq!(argv[argv.len() - 1], "git status");
        // BatchMode (control plane) and ControlMaster multiplexing present.
        assert!(argv.iter().any(|a| a == "BatchMode=yes"));
        assert!(argv.iter().any(|a| a == "ControlMaster=auto"));

        let t2 = SshTarget {
            host: "h".into(),
            port: 2222,
            forward_agent: true,
        };
        let argv = ssh_argv(&t2, "true");
        assert!(
            argv.windows(2).any(|w| w == ["-p", "2222"]),
            "custom port flag"
        );
        assert!(argv.contains(&"-A".to_string()), "forward agent");
    }
}
