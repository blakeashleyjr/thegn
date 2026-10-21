//! SSH-backed exec + file transport for VPS providers — the shim that satisfies
//! the provisioning pipeline's `caps().files` gate (a stock VPS has no
//! provider fs/exec API; sshd is the only channel).
//!
//! Design constraints, inherited from earlier incidents:
//! - **Secrets stay off every command line** (host and remote `ps`): the remote
//!   command is a bare `/bin/sh -s` and the actual script — env exports
//!   included — streams over **stdin**.
//! - **One connection for the whole pipeline when custody is valid**:
//!   `ControlMaster=auto` + `ControlPersist` let the pipeline's dozens of small
//!   execs share a single TCP/auth handshake. A refused socket directory or an
//!   overlong socket path disables multiplexing for that connection.
//! - **Timeout-cancellable**: `tokio::process` with `kill_on_drop`, so the
//!   caller's `tokio::time::timeout` around `run_exec` actually kills a hung
//!   ssh instead of leaking it (the provision watchdog contract).
//! - **Fresh-host-key churn**: `StrictHostKeyChecking=accept-new` against a
//!   per-instance known_hosts file ([`super::registry::known_hosts_path`]),
//!   deleted with the instance.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
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
/// Exports (not `env` prefixes) so secrets ride stdin, never an argv. A missing
/// cwd exits with the shared status and never runs the body elsewhere. Pure.
pub fn stdin_script(script: &str, cwd: Option<&str>, env: &[(String, String)]) -> Result<String> {
    let mut out = String::new();
    for (k, v) in env {
        // Keys are caller-controlled config (env_passthrough names); values are
        // quoted so arbitrary secret bytes survive.
        if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !k.is_empty() {
            out.push_str(&format!("export {k}={}\n", thegn_core::util::sh_quote(v)));
        }
    }
    let cwd = cwd.map(str::trim).filter(|d| !d.is_empty());
    if let Some(d) = cwd
        && (!d.starts_with('/') || d.chars().any(|c| matches!(c, '\0' | '\r' | '\n')))
    {
        return Err(anyhow!(
            "ssh exec: cwd must be an absolute path without control characters"
        ));
    }
    if let Some(d) = cwd {
        out.push_str(&format!(
            "cd -- {} || {{ printf '%s\\n' 'thegn: remote working directory is missing or not accessible' >&2; exit {}; }}\n",
            thegn_core::util::sh_quote(d),
            crate::provider::EXEC_CWD_FAILED_STATUS
        ));
    }
    out.push_str(script);
    out.push('\n');
    Ok(out)
}

impl SshShim {
    /// Ensure OpenSSH can persist this instance's first-contact host key.
    pub fn prepare_known_hosts(&self) -> Result<()> {
        registry::checked_name(&self.name)?;
        registry::ensure_known_hosts_dir()
            .context("vps ssh: cannot create the per-instance known_hosts directory")?;
        Ok(())
    }

    /// The shared ssh option set (control plane): batch, safe multiplexing,
    /// pinned per-instance host key, quiet. Pure over `self` (unit-tested).
    pub fn base_argv(&self) -> Vec<String> {
        let control = control_socket_path_for(&self.key_path);
        self.base_argv_with(control.as_deref())
    }

    fn base_argv_with(&self, control: Option<&std::path::Path>) -> Vec<String> {
        let kh = registry::known_hosts_path(&self.name);
        // The identity is part of the multiplexing namespace. This is
        // load-bearing for managed-key rotation: a connection made with the
        // old key must never satisfy the replacement-key connectivity proof by
        // reusing the old key's already-authenticated ControlMaster.
        let mut v: Vec<String> = vec![
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
        ];
        // Host-key policy comes from the one chokepoint: a managed fresh instance
        // pins accept-new into its per-instance known_hosts (deleted with the
        // instance). See `thegn_core::hostkey` and the host-key ratchet.
        v.extend(thegn_core::hostkey::host_key_args(
            thegn_core::hostkey::HostKeyClass::ManagedFresh,
            &thegn_core::hostkey::HostKeyContext {
                known_hosts: Some(kh.display().to_string()),
                ..Default::default()
            },
        ));
        if let Some(control) = control {
            v.extend([
                "-o".into(),
                "ControlMaster=auto".into(),
                "-o".into(),
                format!("ControlPath={}", control.display()),
                "-o".into(),
                "ControlPersist=90".into(),
            ]);
        } else {
            v.extend([
                "-o".into(),
                "ControlMaster=no".into(),
                "-o".into(),
                "ControlPath=none".into(),
            ]);
        }
        v.extend([
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-i".into(),
            self.key_path.to_string_lossy().into_owned(),
            format!("{}@{}", self.user, self.ip),
        ]);
        v
    }

    /// Run `full_script` remotely by streaming it to `/bin/sh -s` on **stdin**
    /// — the single primitive under exec and file ops. The script rides stdin
    /// (never argv): ssh space-joins any remote-command argv and the remote shell
    /// re-splits it, so a multi-word `sh -c <script>` would mangle (e.g. `mkdir`
    /// loses its operand). File writes embed their payload *inside* the script as
    /// base64 (see [`write`](Self::write)), so there is never a second stdin
    /// channel to multiplex.
    async fn run_raw(&self, full_script: &str) -> Result<(i32, Vec<u8>)> {
        self.prepare_known_hosts()?;
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
            let _ = stdin.write_all(&payload).await; // best-effort: ssh may have exited early; wait_with_output reports it
            let _ = stdin.shutdown().await; // best-effort: same
        });
        let out = child.wait_with_output().await.context("ssh wait")?;
        let _ = feeder.await; // best-effort: feeder has no panic path worth surfacing here
        let mut combined = out.stdout;
        if !out.stderr.is_empty() {
            combined.extend_from_slice(&out.stderr);
        }
        Ok((out.status.code().unwrap_or(-1), combined))
    }

    /// One-shot exec: `(exit_code, combined output)` — the pipeline's
    /// `run_exec` shape. Secrets ride the stdin-streamed exports preamble. A
    /// missing cwd exits with the shared status and never runs the body elsewhere.
    pub async fn run_exec(
        &self,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<(i32, String)> {
        let script = stdin_script(&script_from_argv(argv), cwd, env)?;
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
/// then a per-user temp dir. If a nested `TMPDIR` makes that too long, use the
/// short per-user `/tmp/tg-ssh-<uid>-<digest>` fallback. The socket is ephemeral
/// (ControlPersist), so location is irrelevant beyond length + privacy. The
/// directory is checked for ownership, type and private permissions before
/// OpenSSH can use it.
pub fn control_socket_path() -> Option<PathBuf> {
    control_socket_path_for(PathBuf::from("managed").as_path())
}

/// Control socket template scoped to one SSH identity. OpenSSH's `%C` expands
/// from local-host/remote-host/port/user, but deliberately does not include the
/// identity file; without this suffix, key rotation could appear verified by
/// riding an old key's existing master connection.
pub fn control_socket_path_for(key_path: &std::path::Path) -> Option<PathBuf> {
    let dir = control_dir_candidate(
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::temp_dir(),
        std::env::var_os("HOME"),
        crate::ipc::effective_uid(),
        std::path::Path::new("/tmp"),
    );
    control_socket_path_in(&dir, key_path)
}

fn control_identity(key_path: &std::path::Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"thegn/ssh-control-identity/v1\0");
    hasher.update(key_path.as_os_str().as_encoded_bytes());
    hasher.finalize()[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn control_dir_candidate(
    runtime_dir: Option<std::ffi::OsString>,
    temp_dir: PathBuf,
    home: Option<std::ffi::OsString>,
    uid: Option<u32>,
    short_base: &std::path::Path,
) -> PathBuf {
    if let Some(runtime_dir) = runtime_dir {
        let runtime_dir = PathBuf::from(runtime_dir);
        if runtime_dir.is_absolute() {
            let candidate = runtime_dir.join("tg-ssh");
            if control_dir_fits(&candidate) {
                return candidate;
            }
        }
    }
    let digest = control_home_digest(home);
    let temp_candidate = match &digest {
        Some(digest) => temp_dir.join(format!("tg-ssh-{}", &digest[..12])),
        None => temp_dir.join("tg-ssh-nohome"),
    };
    if control_dir_fits(&temp_candidate) {
        return temp_candidate;
    }
    match uid {
        Some(uid) => match digest {
            Some(digest) => short_base.join(format!("tg-ssh-{uid}-{}", &digest[..8])),
            None => short_base.join(format!("tg-ssh-{uid}-nohome")),
        },
        None => temp_candidate,
    }
}

fn control_home_digest(home: Option<std::ffi::OsString>) -> Option<String> {
    home.filter(|home| !home.is_empty()).map(|home| {
        let mut hasher = Sha256::new();
        hasher.update(b"thegn/ssh-control-home/v1\0");
        hasher.update(home.as_encoded_bytes());
        hasher.finalize()[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    })
}

fn control_dir_fits(dir: &std::path::Path) -> bool {
    control_template(dir, std::path::Path::new("fit-probe")).is_some()
}

pub fn control_socket_path_in(
    dir: &std::path::Path,
    key_path: &std::path::Path,
) -> Option<PathBuf> {
    if let Err(error) = thegn_core::fsperm::ensure_private_dir(dir) {
        static WARN_ONCE: std::sync::Once = std::sync::Once::new();
        WARN_ONCE.call_once(|| {
            tracing::warn!(target: "thegn::ssh", dir = %dir.display(), error = %error, "ControlMaster directory refused; connecting without multiplexing");
        });
        return None;
    }
    let Some(template) = control_template(dir, key_path) else {
        static WARN_ONCE: std::sync::Once = std::sync::Once::new();
        WARN_ONCE.call_once(|| {
            tracing::warn!(target: "thegn::ssh", dir = %dir.display(), "ControlMaster socket path too long; connecting without multiplexing");
        });
        return None;
    };
    Some(template)
}

fn control_template(dir: &std::path::Path, key_path: &std::path::Path) -> Option<PathBuf> {
    let template = dir.join(format!("cm-{}-%C", control_identity(key_path)));
    if template.as_os_str().len() - 2 + 40 > 100 {
        return None;
    }
    Some(template)
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
    fn prepare_known_hosts_refuses_an_invalid_name() {
        let shim = SshShim {
            name: "../x".into(),
            ip: "203.0.113.7".into(),
            user: "root".into(),
            key_path: "/state/ssh/key".into(),
        };

        assert!(shim.prepare_known_hosts().is_err());
    }

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
        let s = stdin_script("echo hi", Some("/workspace"), &env).unwrap();
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
        assert_eq!(stdin_script("true", None, &[]).unwrap(), "true\n");
    }

    #[test]
    fn stdin_script_cwd_failure_exits_before_body() {
        let s = stdin_script("echo BODY", Some("/work/a b"), &[]).unwrap();
        let cd = s.find("cd -- '/work/a b' ||").expect("checked cd present");
        let body = s.find("echo BODY").expect("body present");
        assert!(cd < body, "cd check precedes the body");
        assert!(s.contains(&format!(
            "exit {};",
            crate::provider::EXEC_CWD_FAILED_STATUS
        )));
        let cd_line = s.lines().find(|line| line.starts_with("cd ")).unwrap();
        assert!(!cd_line.contains("2>/dev/null"));
    }

    #[test]
    fn stdin_script_refuses_relative_or_control_cwd() {
        for cwd in [
            Some("relative"),
            Some("/a\nb"),
            Some("/a\0b"),
            Some("/a\rb"),
        ] {
            assert!(stdin_script("true", cwd, &[]).is_err(), "accepted {cwd:?}");
        }
        for cwd in [None, Some("  ")] {
            let script = stdin_script("true", cwd, &[]).unwrap();
            assert!(!script.contains("cd "));
        }
    }

    #[test]
    fn missing_cwd_never_runs_the_body() {
        use std::io::Write;

        if !cfg!(unix) || !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let temp = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut builder = tempfile::Builder::new();
                builder.permissions(std::fs::Permissions::from_mode(0o700));
                builder.tempdir().unwrap()
            }
            #[cfg(not(unix))]
            {
                tempfile::Builder::new().tempdir().unwrap()
            }
        };
        let missing = temp.path().join("missing");
        let script = stdin_script("echo RAN", Some(missing.to_str().unwrap()), &[]).unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(crate::provider::EXEC_CWD_FAILED_STATUS)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("RAN"));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("thegn: remote working directory is missing or not accessible")
        );

        let existing = temp.path().canonicalize().unwrap();
        let script = stdin_script("pwd", Some(existing.to_str().unwrap()), &[]).unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            existing.to_str().unwrap()
        );
    }

    #[test]
    fn base_argv_pins_per_instance_known_hosts_and_multiplexes() {
        let shim = SshShim {
            name: "tg-dev-x1".into(),
            ip: "203.0.113.7".into(),
            user: "root".into(),
            key_path: "/state/ssh/sprite_ed25519".into(),
        };
        let control = control_template(std::path::Path::new("/r/tg-ssh"), &shim.key_path).unwrap();
        let argv = shim.base_argv_with(Some(&control));
        assert_eq!(argv[0], "ssh");
        assert!(argv.contains(&"BatchMode=yes".to_string()));
        assert!(argv.contains(&"ControlMaster=auto".to_string()));
        assert!(argv.contains(&"IdentitiesOnly=yes".to_string()));
        // The host-key policy is the ManagedFresh chokepoint's, keyed by this
        // instance's per-instance known_hosts. Assert against ITS output rather
        // than a literal here (keeps host-key literals out of every file but
        // `hostkey.rs` — the ratchet).
        let kh = registry::known_hosts_path("tg-dev-x1");
        let expected = thegn_core::hostkey::host_key_args(
            thegn_core::hostkey::HostKeyClass::ManagedFresh,
            &thegn_core::hostkey::HostKeyContext {
                known_hosts: Some(kh.display().to_string()),
                ..Default::default()
            },
        );
        for tok in &expected {
            assert!(argv.contains(tok), "missing host-key arg {tok}: {argv:?}");
        }
        assert!(
            expected.iter().any(|a| a.ends_with("tg-dev-x1")),
            "per-instance known_hosts pinned: {expected:?}"
        );
        assert_eq!(argv.last().unwrap(), "root@203.0.113.7");
    }

    #[test]
    fn control_master_namespace_includes_the_identity_path() {
        let dir = std::path::Path::new("/r/tg-ssh");
        let old = control_template(dir, std::path::Path::new("/state/ssh/key")).unwrap();
        let replacement =
            control_template(dir, std::path::Path::new("/state/ssh/key.next-123")).unwrap();
        assert_ne!(old, replacement);
        assert_eq!(
            old,
            control_template(dir, std::path::Path::new("/state/ssh/key")).unwrap()
        );
        assert!(old.to_string_lossy().ends_with("-%C"));
    }

    #[cfg(unix)]
    #[test]
    fn control_identity_distinguishes_non_utf8_aliases() {
        use std::os::unix::ffi::OsStrExt;

        let first = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/k/\xff"));
        let second = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/k/\xfe"));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(control_identity(first), control_identity(second));
    }

    #[cfg(unix)]
    #[test]
    fn refused_dir_disables_multiplexing() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let shim = SshShim {
            name: "tg-dev-x1".into(),
            ip: "203.0.113.7".into(),
            user: "root".into(),
            key_path: "/state/ssh/key".into(),
        };

        assert!(
            control_socket_path_in(&link, &shim.key_path).is_none(),
            "a symlinked control directory must be refused"
        );
        let argv = shim.base_argv_with(None);
        assert!(argv.contains(&"ControlMaster=no".to_string()));
        assert!(argv.contains(&"ControlPath=none".to_string()));
        assert!(!argv.contains(&"ControlMaster=auto".to_string()));
        assert!(!argv.contains(&"ControlPersist=90".to_string()));
        assert_eq!(argv.last().unwrap(), "root@203.0.113.7");
    }

    #[test]
    fn control_dir_is_private_and_template_is_stable() {
        // A short base: sockets are capped near 100 bytes, and a nested TMPDIR
        // (nix-shell, nextest) can push the template past that cap.
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let dir = temp.path().join("sock");
        std::fs::create_dir(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let key = std::path::Path::new("/state/ssh/key");
        let first = control_template(std::path::Path::new("/r/tg-ssh"), key).unwrap();
        let second = control_template(std::path::Path::new("/r/tg-ssh"), key).unwrap();
        let other = control_template(
            std::path::Path::new("/r/tg-ssh"),
            std::path::Path::new("/state/ssh/other"),
        )
        .unwrap();
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert!(first.to_string_lossy().ends_with("-%C"));
        assert!(control_socket_path_in(&dir, key).is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn overlong_socket_template_is_refused() {
        let dir = PathBuf::from("d".repeat(90));
        assert!(control_template(&dir, std::path::Path::new("/key")).is_none());
    }

    #[test]
    fn control_dir_candidate_prefers_an_absolute_runtime_dir() {
        let temp = PathBuf::from("/tmp/test-temp");
        let short_base = std::path::Path::new("/tmp");
        assert_eq!(
            control_dir_candidate(
                Some("/run/user/1000".into()),
                temp.clone(),
                None,
                None,
                short_base,
            ),
            PathBuf::from("/run/user/1000/tg-ssh")
        );
        let relative = control_dir_candidate(
            Some("relative".into()),
            temp.clone(),
            Some("/home/a".into()),
            None,
            short_base,
        );
        let second_home =
            control_dir_candidate(None, temp.clone(), Some("/home/b".into()), None, short_base);
        assert!(relative.starts_with(&temp));
        assert_ne!(relative, second_home);
        assert_eq!(
            control_dir_candidate(None, temp.clone(), None, None, short_base),
            temp.join("tg-ssh-nohome")
        );
        assert_eq!(
            control_dir_candidate(None, temp.clone(), Some("".into()), None, short_base),
            temp.join("tg-ssh-nohome")
        );
    }

    #[test]
    fn control_dir_candidate_uses_a_short_tmp_fallback_under_a_long_temp_dir() {
        let temp = PathBuf::from(format!("/tmp/{}", "n".repeat(80)));
        let candidate = control_dir_candidate(
            None,
            temp.clone(),
            Some("/home/a".into()),
            Some(1000),
            std::path::Path::new("/tmp"),
        );
        assert!(candidate.to_string_lossy().starts_with("/tmp/tg-ssh-1000-"));
        assert!(!candidate.starts_with(&temp));
        assert!(control_template(&candidate, std::path::Path::new("/state/ssh/key")).is_some());
    }

    #[test]
    fn control_dir_short_fallback_is_per_user_and_per_home() {
        let temp = PathBuf::from(format!("/tmp/{}", "n".repeat(80)));
        let short_base = std::path::Path::new("/tmp");
        let user_a_home_a = control_dir_candidate(
            None,
            temp.clone(),
            Some("/home/a".into()),
            Some(1000),
            short_base,
        );
        let user_b_home_a = control_dir_candidate(
            None,
            temp.clone(),
            Some("/home/a".into()),
            Some(1001),
            short_base,
        );
        let user_a_home_b = control_dir_candidate(
            None,
            temp.clone(),
            Some("/home/b".into()),
            Some(1000),
            short_base,
        );
        let user_b_home_b = control_dir_candidate(
            None,
            temp.clone(),
            Some("/home/b".into()),
            Some(1001),
            short_base,
        );
        assert_ne!(user_a_home_a, user_b_home_a);
        assert_ne!(user_a_home_a, user_a_home_b);
        assert_ne!(user_a_home_a, user_b_home_b);
        assert_ne!(user_b_home_a, user_a_home_b);
        assert_ne!(user_b_home_a, user_b_home_b);
        assert_ne!(user_a_home_b, user_b_home_b);
        assert_eq!(
            control_dir_candidate(None, temp, None, Some(1000), short_base),
            PathBuf::from("/tmp/tg-ssh-1000-nohome")
        );
    }

    #[test]
    fn control_dir_candidate_skips_an_overlong_runtime_dir() {
        let temp = PathBuf::from("/tmp/test-temp");
        let runtime = format!("/{}", "r".repeat(60));
        let fallback = control_dir_candidate(
            Some(runtime.into()),
            temp.clone(),
            Some("/home/a".into()),
            Some(1000),
            std::path::Path::new("/tmp"),
        );
        assert!(fallback.to_string_lossy().starts_with("/tmp/tg-ssh-1000-"));
        assert!(control_dir_fits(&fallback));

        let runtime_candidate = control_dir_candidate(
            Some("/run/user/1000".into()),
            temp,
            Some("/home/a".into()),
            Some(1000),
            std::path::Path::new("/tmp"),
        );
        assert_eq!(runtime_candidate, PathBuf::from("/run/user/1000/tg-ssh"));
    }

    #[test]
    fn control_dir_candidate_keeps_a_short_temp_dir() {
        // The candidate needs at most 31 bytes, including the digest suffix.
        let temp = PathBuf::from("/t");
        let home = Some("/home/a".into());
        let digest = control_home_digest(home.clone()).unwrap();
        let candidate = control_dir_candidate(
            None,
            temp.clone(),
            home,
            Some(1000),
            std::path::Path::new("/tmp"),
        );
        assert_eq!(candidate, temp.join(format!("tg-ssh-{}", &digest[..12])));
        assert!(control_dir_fits(&candidate));
    }

    #[test]
    fn control_dir_candidate_without_uid_keeps_the_temp_dir() {
        let temp = PathBuf::from(format!("/tmp/{}", "n".repeat(80)));
        let candidate = control_dir_candidate(
            None,
            temp.clone(),
            Some("/home/a".into()),
            None,
            std::path::Path::new("/tmp"),
        );
        assert!(candidate.starts_with(&temp));
        assert!(!control_dir_fits(&candidate));
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
