//! SSH-backed exec + file transport for VPS providers — the shim that satisfies
//! the provisioning pipeline's `caps().files` gate (a stock VPS has no
//! provider fs/exec API; sshd is the only channel).
//!
//! Design constraints, inherited from earlier incidents:
//! - **Secrets stay off every command line** (host and remote `ps`): the remote
//!   command is a bare `/bin/sh -s` and the actual script — env exports
//!   included — streams over **stdin**.
//! - **One connection for the whole pipeline**: `ControlMaster=auto` +
//!   `ControlPersist` so the pipeline's dozens of small execs share a single
//!   TCP/auth handshake.
//! - **Timeout-cancellable**: `tokio::process` with `kill_on_drop`, so the
//!   caller's `tokio::time::timeout` around `run_exec` actually kills a hung
//!   ssh instead of leaking it (the provision watchdog contract).
//! - **Fresh-host-key churn**: `StrictHostKeyChecking=accept-new` against a
//!   per-instance known_hosts file ([`super::registry::known_hosts_path`]),
//!   deleted with the instance.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use tokio::io::AsyncWriteExt;

use super::registry;

/// A resolved ssh endpoint for one instance.
#[derive(Debug, Clone)]
pub struct SshShim {
    pub name: String,
    pub ip: String,
    pub user: String,
    pub key_path: PathBuf,
}

/// Normalize a provider-exec argv into one shell script. The provisioning
/// pipeline always sends `["/bin/sh", "-lc", script]`; anything else is
/// quote-joined verbatim. Pure (unit-tested).
pub fn script_from_argv(argv: &[String]) -> String {
    match argv {
        [sh, flag, script]
            if (sh.ends_with("sh") || sh == "sh") && (flag == "-lc" || flag == "-c") =>
        {
            script.clone()
        }
        _ => argv
            .iter()
            .map(|a| thegn_core::util::sh_quote(a))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// The full stdin-streamed script: env exports + optional `cd` + the body.
/// Exports (not `env` prefixes) so secrets ride stdin, never an argv. Pure.
pub fn stdin_script(script: &str, cwd: Option<&str>, env: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in env {
        // Keys are caller-controlled config (env_passthrough names); values are
        // quoted so arbitrary secret bytes survive.
        if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !k.is_empty() {
            out.push_str(&format!("export {k}={}\n", thegn_core::util::sh_quote(v)));
        }
    }
    if let Some(d) = cwd.map(str::trim).filter(|d| !d.is_empty()) {
        out.push_str(&format!(
            "cd {} 2>/dev/null\n",
            thegn_core::util::sh_quote(d)
        ));
    }
    out.push_str(script);
    out.push('\n');
    out
}

impl SshShim {
    /// The shared ssh option set (control plane): batch, multiplexed, pinned
    /// per-instance host key, quiet. Pure over `self` (unit-tested).
    pub fn base_argv(&self) -> Vec<String> {
        let kh = registry::known_hosts_path(&self.name);
        let control = control_socket_path();
        vec![
            "ssh".into(),
            // Hermetic: a thegn-managed remote pins its own identity, known_hosts,
            // and options below — never read the user's personal ~/.ssh/config
            // (its Host rules don't apply to a direct-IP managed connection, and a
            // home-manager/nix-store config file is root-owned, which OpenSSH
            // rejects as "Bad owner or permissions", breaking every managed ssh).
            "-F".into(),
            "/dev/null".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            format!("UserKnownHostsFile={}", kh.display()),
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}", control.display()),
            "-o".into(),
            "ControlPersist=90".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-i".into(),
            self.key_path.to_string_lossy().into_owned(),
            format!("{}@{}", self.user, self.ip),
        ]
    }

    /// Run `full_script` remotely by streaming it to `/bin/sh -s` on **stdin**
    /// — the single primitive under exec and file ops. The script rides stdin
    /// (never argv): ssh space-joins any remote-command argv and the remote shell
    /// re-splits it, so a multi-word `sh -c <script>` would mangle (e.g. `mkdir`
    /// loses its operand). File writes embed their payload *inside* the script as
    /// base64 (see [`write`](Self::write)), so there is never a second stdin
    /// channel to multiplex.
    async fn run_raw(&self, full_script: &str) -> Result<(i32, Vec<u8>)> {
        let mut argv = self.base_argv();
        argv.push("--".into());
        argv.push("/bin/sh".into());
        argv.push("-s".into());
        let mut child = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawn ssh")?;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("ssh stdin"))?;
        let payload = full_script.as_bytes().to_vec();
        // Feed stdin concurrently with output collection (a large script must
        // not deadlock against a filling stdout pipe).
        let feeder = tokio::spawn(async move {
            let _ = stdin.write_all(&payload).await;
            let _ = stdin.shutdown().await;
        });
        let out = child.wait_with_output().await.context("ssh wait")?;
        let _ = feeder.await;
        let mut combined = out.stdout;
        if !out.stderr.is_empty() {
            combined.extend_from_slice(&out.stderr);
        }
        Ok((out.status.code().unwrap_or(-1), combined))
    }

    /// One-shot exec: `(exit_code, combined output)` — the pipeline's
    /// `run_exec` shape. Secrets ride the stdin-streamed exports preamble.
    pub async fn run_exec(
        &self,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<(i32, String)> {
        let script = stdin_script(&script_from_argv(argv), cwd, env);
        let (code, out) = self.run_raw(&script).await?;
        Ok((code, String::from_utf8_lossy(&out).into_owned()))
    }

    /// Read a remote file's bytes; `Err` when missing (the provision-marker
    /// contract: a missing marker must be an error, not empty bytes).
    pub async fn read(&self, path: &str) -> Result<Vec<u8>> {
        let p = thegn_core::util::sh_quote(path);
        let (code, out) = self.run_raw(&format!("cat {p}\n")).await?;
        if code == 0 {
            Ok(out)
        } else {
            Err(anyhow!("read {path}: exit {code}"))
        }
    }

    /// Write `data` to a remote path (parents created), with `mode`. The payload
    /// is base64-embedded *in the script* (decoded remotely with `base64 -d`), so
    /// it rides the single stdin channel to `sh -s` — binary-safe, and never on
    /// argv (which ssh would space-join + re-split).
    pub async fn write(&self, path: &str, data: &[u8], mode: &str) -> Result<()> {
        use base64::Engine;
        let p = thegn_core::util::sh_quote(path);
        let dir = thegn_core::util::sh_quote(
            std::path::Path::new(path)
                .parent()
                .map(|d| d.to_string_lossy().into_owned())
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| ".".into())
                .as_str(),
        );
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        let script =
            format!("mkdir -p {dir} && printf %s {b64} | base64 -d > {p} && chmod {mode} {p}");
        let (code, out) = self.run_raw(&script).await?;
        if code == 0 {
            Ok(())
        } else {
            Err(anyhow!(
                "write {path}: exit {code}: {}",
                String::from_utf8_lossy(&out)
            ))
        }
    }

    /// List a remote directory (GNU `find`, one level).
    pub async fn list_dir(&self, path: &str) -> Result<Vec<crate::provider::FileEntry>> {
        let p = thegn_core::util::sh_quote(path);
        let script = format!("find {p} -maxdepth 1 -mindepth 1 -printf '%y\\t%s\\t%f\\n'");
        let (code, out) = self.run_raw(&format!("{script}\n")).await?;
        if code != 0 {
            return Err(anyhow!("list {path}: exit {code}"));
        }
        Ok(parse_find_listing(&String::from_utf8_lossy(&out)))
    }

    /// Delete a remote path recursively (idempotent).
    pub async fn delete(&self, path: &str) -> Result<()> {
        let p = thegn_core::util::sh_quote(path);
        let (code, out) = self.run_raw(&format!("rm -rf {p}\n")).await?;
        if code == 0 {
            Ok(())
        } else {
            Err(anyhow!(
                "delete {path}: exit {code}: {}",
                String::from_utf8_lossy(&out)
            ))
        }
    }
}

/// The ControlMaster multiplex socket template (`…/cm-%C`, `%C` = OpenSSH's
/// per-connection hash). A Unix-domain socket path is capped at ~104 bytes, so
/// the base dir MUST be short — a deep `$XDG_STATE_HOME` (e.g. under a profile
/// dir) blows the limit and every ssh fails with "path too long for Unix domain
/// socket". Prefer the short, private `$XDG_RUNTIME_DIR` (`/run/user/<uid>`),
/// falling back to a per-user temp dir; the socket is ephemeral (ControlPersist),
/// so location is irrelevant beyond length + privacy. The `tg-ssh` dir is created
/// 0700 best-effort.
pub fn control_socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
        .join("tg-ssh");
    // best-effort: if the mkdir fails, ssh reports the bind error as before.
    let _ = std::fs::create_dir_all(&base);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700));
    }
    base.join("cm-%C")
}

/// Parse `find -printf '%y\t%s\t%f\n'` output into entries. Pure (unit-tested).
pub fn parse_find_listing(out: &str) -> Vec<crate::provider::FileEntry> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '\t');
            let ty = it.next()?;
            let size = it.next()?.parse::<u64>().unwrap_or(0);
            let name = it.next()?.to_string();
            (!name.is_empty()).then_some(crate::provider::FileEntry {
                name,
                is_dir: ty == "d",
                size,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_from_argv_unwraps_shell_invocations() {
        let argv = vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "echo hi".to_string(),
        ];
        assert_eq!(script_from_argv(&argv), "echo hi");
        let argv = vec!["sh".to_string(), "-c".to_string(), "true".to_string()];
        assert_eq!(script_from_argv(&argv), "true");
        // Anything else is quote-joined so spaces survive.
        let argv = vec!["git".to_string(), "log".to_string(), "a b".to_string()];
        assert_eq!(script_from_argv(&argv), "git log 'a b'");
    }

    #[test]
    fn stdin_script_exports_env_and_cds_before_body() {
        let env = vec![
            ("GH_TOKEN".to_string(), "s3cr'et".to_string()),
            ("weird-key".to_string(), "dropped".to_string()),
        ];
        let s = stdin_script("echo hi", Some("/workspace"), &env);
        // Secrets are quoted exports INSIDE the streamed script — never argv.
        assert!(s.contains("export GH_TOKEN="));
        assert!(s.contains("s3cr"));
        assert!(!s.contains("weird-key"), "non-identifier keys are dropped");
        let cd = s
            .find("cd ")
            .filter(|_| s.contains("/workspace"))
            .expect("cd present");
        let body = s.find("echo hi").expect("body present");
        assert!(cd < body, "cd precedes the body");
        // No cwd, no env ⇒ just the body.
        assert_eq!(stdin_script("true", None, &[]), "true\n");
    }

    #[test]
    fn base_argv_pins_per_instance_known_hosts_and_multiplexes() {
        let shim = SshShim {
            name: "sz-dev-x1".into(),
            ip: "203.0.113.7".into(),
            user: "root".into(),
            key_path: "/state/ssh/sprite_ed25519".into(),
        };
        let argv = shim.base_argv();
        assert_eq!(argv[0], "ssh");
        assert!(argv.contains(&"BatchMode=yes".to_string()));
        assert!(argv.contains(&"StrictHostKeyChecking=accept-new".to_string()));
        assert!(argv.contains(&"ControlMaster=auto".to_string()));
        assert!(argv.contains(&"IdentitiesOnly=yes".to_string()));
        assert!(
            argv.iter()
                .any(|a| a.starts_with("UserKnownHostsFile=") && a.ends_with("sz-dev-x1")),
            "per-instance known_hosts: {argv:?}"
        );
        assert_eq!(argv.last().unwrap(), "root@203.0.113.7");
    }

    #[test]
    fn find_listing_parses_types_sizes_names() {
        let out = "d\t4096\tsrc\nf\t120\tmain.rs\nl\t10\tlink\nbogus\n";
        let entries = parse_find_listing(out);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].name, "src");
        assert!(!entries[1].is_dir);
        assert_eq!(entries[1].size, 120);
        assert!(!entries[2].is_dir, "symlink is not a dir");
        assert!(parse_find_listing("").is_empty());
    }
}
