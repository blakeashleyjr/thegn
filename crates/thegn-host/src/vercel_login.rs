//! `model_credentials = "upload"`: read the host's claude/codex login files
//! (and only those) for the `vercel-exec` TTY bridge to upload.
//!
//! Only [`creds::login_files`] are read, from the agent's relocated home
//! (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`) when set, else `~/.claude` /
//! `~/.codex`. A symlink, a non-regular file or a file over [`MAX_LOGIN_FILE`]
//! is refused. Never settings, hooks, MCP config, `[[accounts]]` trees or
//! anything else in the home.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use thegn_core::vercel::creds;
use thegn_svc::vercel::login::LoginPayload;

/// The largest login file read (a credentials JSON is a few KiB).
pub(crate) const MAX_LOGIN_FILE: u64 = 64 * 1024;

/// `(relocation env var, default dir under $HOME)` for an upload agent. The
/// same pairs `agent_configs`' relocation uses (its helper is private there).
fn agent_home_source(agent: &str) -> Option<(&'static str, &'static str)> {
    match agent {
        "claude" | "claude-code" => Some(("CLAUDE_CONFIG_DIR", ".claude")),
        "codex" => Some(("CODEX_HOME", ".codex")),
        _ => None,
    }
}

/// The agent's host home: the relocation var (tilde-expanded) if set and
/// non-empty, else `<home>/<default dir>`.
fn agent_home(agent: &str, env: &dyn Fn(&str) -> Option<String>, home: &Path) -> Option<PathBuf> {
    let (var, default_dir) = agent_home_source(agent)?;
    Some(
        env(var)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(|v| PathBuf::from(thegn_core::util::expand_tilde_in(&v, home)))
            .unwrap_or_else(|| home.join(default_dir)),
    )
}

/// Read one login file: no-follow, regular, at most [`MAX_LOGIN_FILE`] bytes.
/// Errors carry the file name and a reason, never content.
fn read_login_file(path: &Path, name: &str) -> Result<Vec<u8>, String> {
    let file = thegn_core::fsperm::open_regular_file_nofollow(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("{name} not found")
        } else {
            format!("{name} is not a readable regular file (symlinks are refused)")
        }
    })?;
    let len = file
        .metadata()
        .map_err(|_| format!("{name} could not be inspected"))?
        .len();
    if len > MAX_LOGIN_FILE {
        return Err(format!("{name} is larger than {MAX_LOGIN_FILE} bytes"));
    }
    // Sized once for the cap, so the read never reallocates (no stray copies).
    let mut bytes = Vec::with_capacity(MAX_LOGIN_FILE as usize + 1);
    if file
        .take(MAX_LOGIN_FILE + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        wipe(bytes);
        return Err(format!("{name} could not be read"));
    }
    if bytes.len() as u64 > MAX_LOGIN_FILE {
        wipe(bytes);
        return Err(format!("{name} is larger than {MAX_LOGIN_FILE} bytes"));
    }
    Ok(bytes)
}

/// Overwrite a credential buffer that will not be uploaded. (The uploaded
/// ones move into [`LoginPayload`]'s zeroizing buffers.)
fn wipe(mut bytes: Vec<u8>) {
    bytes.fill(0);
    std::hint::black_box(&bytes);
}

/// [`read_host_logins`] over an explicit env lookup and `$HOME` (testable
/// without touching process env).
pub(crate) fn read_host_logins_in(
    agents: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    home: &Path,
) -> (Vec<LoginPayload>, Vec<String>) {
    let mut payloads = Vec::new();
    let mut skipped = Vec::new();
    for agent in agents {
        let agent = agent.trim();
        let (Some(files), Some(dir)) = (creds::login_files(agent), agent_home(agent, env, home))
        else {
            skipped.push(format!(
                "{agent}: model_credentials = upload supports claude and codex"
            ));
            continue;
        };
        let mut read = Vec::new();
        let mut failed = None;
        for file in files {
            match read_login_file(&dir.join(file.host_rel), file.host_rel) {
                Ok(bytes) => read.push((file.vm_name.to_string(), bytes)),
                Err(why) => {
                    failed = Some(why);
                    break;
                }
            }
        }
        if let Some(why) = failed {
            for (_, bytes) in read {
                wipe(bytes);
            }
            skipped.push(format!("{agent}: {why}"));
            continue;
        }
        read.extend(
            creds::synthesized_files(agent)
                .into_iter()
                .map(|(name, bytes)| (name.to_string(), bytes)),
        );
        match LoginPayload::new(agent, read) {
            Ok(payload) => payloads.push(payload),
            Err(e) => skipped.push(format!("{agent}: {e}")),
        }
    }
    (payloads, skipped)
}

/// The login payloads for `agents`, plus one reason line per skipped agent.
pub(crate) fn read_host_logins(agents: &[String]) -> (Vec<LoginPayload>, Vec<String>) {
    let home = thegn_core::util::home();
    read_host_logins_in(agents, &|key| std::env::var(key).ok(), &home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn agents(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reads_from_the_relocated_home_and_never_settings() {
        let state = thegn_core::testenv::IsolatedState::new();
        let home = state.root().to_path_buf();
        // The default home holds a fossil; the relocated profile is live.
        write(&home.join(".claude/.credentials.json"), b"FOSSIL");
        write(&home.join(".claude/settings.json"), b"SETTINGS-MARKER");
        let profile = home.join("profiles/work");
        write(&profile.join(".credentials.json"), b"LIVE-CREDS");
        write(&profile.join("settings.json"), b"SETTINGS-MARKER");
        let env = |key: &str| (key == "CLAUDE_CONFIG_DIR").then(|| profile.display().to_string());

        let (payloads, skipped) = read_host_logins_in(&agents(&["claude"]), &env, &home);
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(payloads.len(), 1);
        let names = payloads[0].file_names();
        assert!(names.contains(&".credentials.json"), "{names:?}");
        assert!(!names.contains(&"settings.json"), "{names:?}");
        // The relocated profile's file (10 bytes), not the 6-byte fossil.
        assert_eq!(
            payloads[0].file_len(".credentials.json"),
            Some(b"LIVE-CREDS".len())
        );
    }

    #[test]
    fn falls_back_to_the_default_home() {
        let state = thegn_core::testenv::IsolatedState::new();
        let home = state.root().to_path_buf();
        write(&home.join(".codex/auth.json"), b"{\"k\":1}");
        let (payloads, skipped) = read_host_logins_in(&agents(&["codex"]), &|_| None, &home);
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(payloads[0].file_names(), ["auth.json"]);
    }

    #[test]
    fn refuses_a_symlink() {
        let state = thegn_core::testenv::IsolatedState::new();
        let home = state.root().to_path_buf();
        write(&home.join("elsewhere/auth.json"), b"{}");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        if thegn_core::fsperm::create_symlink(
            &home.join("elsewhere/auth.json"),
            &home.join(".codex/auth.json"),
            false,
        )
        .is_err()
        {
            return; // symlinks unavailable on this platform
        }
        let (payloads, skipped) = read_host_logins_in(&agents(&["codex"]), &|_| None, &home);
        assert!(payloads.is_empty());
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].contains("symlinks are refused"), "{skipped:?}");
    }

    #[test]
    fn refuses_an_oversize_file() {
        let state = thegn_core::testenv::IsolatedState::new();
        let home = state.root().to_path_buf();
        write(
            &home.join(".codex/auth.json"),
            &vec![b'x'; MAX_LOGIN_FILE as usize + 1],
        );
        let (payloads, skipped) = read_host_logins_in(&agents(&["codex"]), &|_| None, &home);
        assert!(payloads.is_empty());
        assert!(skipped[0].contains("larger than"), "{skipped:?}");
    }

    #[test]
    fn unsupported_and_missing_agents_are_skipped() {
        let state = thegn_core::testenv::IsolatedState::new();
        let home = state.root().to_path_buf();
        let (payloads, skipped) = read_host_logins_in(&agents(&["pi", "codex"]), &|_| None, &home);
        assert!(payloads.is_empty());
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped[0].starts_with("pi: "), "{skipped:?}");
        assert!(skipped[1].contains("not found"), "{skipped:?}");
    }
}
