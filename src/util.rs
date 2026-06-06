//! Small shared helpers: XDG paths, tilde expansion, slugify, age formatting,
//! and thin subprocess wrappers (git / generic commands).

use crate::msg;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn xdg_config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

pub fn xdg_state_home() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
}

/// superzej's own home — config, worktrees, zellij socket/cache, activity all
/// live under here (`~/.superzej`). `SUPERZEJ_DIR` relocates it so a dev/test
/// instance can run on a fully separate root (its own session namespace, cache,
/// config and worktrees) without touching your daily-driver superzej. Pair it
/// with `XDG_STATE_HOME` to also isolate the DB (see `just start-term`).
pub fn superzej_dir() -> PathBuf {
    std::env::var_os("SUPERZEJ_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".superzej"))
}

/// Where superzej's WASM plugins are deployed. Kept as a literal `~/.local/share`
/// path (not `$XDG_DATA_HOME`) so it always matches the `file:~/.local/share/...`
/// references in the session layout — and thus the zellij permission-cache keys.
pub fn plugin_dir() -> PathBuf {
    home().join(".local/share/superzej")
}

/// Expand a leading `~` to `$HOME` (config values may contain it literally).
pub fn expand_tilde(p: &str) -> String {
    if p == "~" {
        home().to_string_lossy().into_owned()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest).to_string_lossy().into_owned()
    } else {
        p.to_string()
    }
}

/// lowercase, non-alnum -> '-', collapse repeats, trim.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Human-friendly age from an epoch-seconds value (e.g. 2h, 3d, 10m, 5s).
pub fn age(then: i64) -> String {
    let diff = (now() - then).max(0);
    if diff < 60 {
        format!("{diff}s")
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86400 {
        format!("{}h", diff / 3600)
    } else {
        format!("{}d", diff / 86400)
    }
}

pub fn have(cmd: &str) -> bool {
    // `command -v` semantics: search PATH for an executable.
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(cmd);
            if p.is_file() {
                return true;
            }
        }
    }
    false
}

/// Run `git -C <dir> <args...>`, returning trimmed stdout on success (None on
/// failure or empty output).
pub fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// The last path component of a string (no trailing-slash handling needed here).
pub fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

pub fn shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

/// The user's preferred editor command (program plus any args), honoring
/// `$VISUAL` then `$EDITOR`, falling back to `vi`. Blank/whitespace values are
/// skipped so an exported-but-empty var doesn't shadow the next choice.
pub fn editor() -> String {
    ["VISUAL", "EDITOR"]
        .into_iter()
        .find_map(|k| {
            std::env::var(k)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "vi".to_string())
}

/// Whether an editor command launches a graphical (windowed) editor that should
/// be spawned detached rather than run inside a terminal pane. Matches on the
/// program's basename (first whitespace-delimited word), so `code --wait` and
/// `/usr/bin/code` both resolve to `code`.
pub fn is_gui_editor(cmd: &str) -> bool {
    let prog = cmd.split_whitespace().next().unwrap_or(cmd);
    let base = basename(prog);
    let base = base.strip_suffix(".exe").unwrap_or(base);
    matches!(
        base,
        "code"
            | "code-insiders"
            | "codium"
            | "vscodium"
            | "cursor"
            | "windsurf"
            | "subl"
            | "sublime_text"
            | "zed"
            | "zeditor"
            | "gvim"
            | "mvim"
            | "gedit"
            | "kate"
            | "idea"
            | "pycharm"
            | "webstorm"
            | "rider"
    )
}

/// Spawn `cmd` via the login shell, fully detached (no controlling pane, output
/// discarded). For GUI apps launched from a pane that is about to close.
pub fn spawn_detached(cmd: &str, cwd: &Path) {
    use std::process::Stdio;
    let _ = Command::new(shell())
        .args(["-lc", cmd])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Spawn `prog args...` as a detached background daemon: its own process group
/// (so a Ctrl-C / SIGINT in the launching terminal doesn't reach it) and null
/// stdio. Used to start the `watch` daemon just before we exec into zellij — it
/// is reparented to init when the launching client exits.
pub fn spawn_daemon(prog: &str, args: &[&str]) {
    use std::process::Stdio;
    let _ = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
}

/// Set the terminal (pane) window title via OSC. zellij shows it as the pane's
/// frame title; any program run afterwards (vim, lazygit, …) overrides it as
/// usual, so this just seeds a sensible default (the branch/worktree name).
pub fn set_terminal_title(title: &str) {
    use std::io::Write;
    print!("\u{1b}]0;{title}\u{07}");
    let _ = std::io::stdout().flush();
}

/// Replace this process with an interactive login shell.
pub fn exec_shell() -> ! {
    let sh = shell();
    let err = Command::new(&sh).arg("-l").exec();
    msg::die(&format!("exec {sh} failed: {err}"));
}

/// Replace this process with `$SHELL -lc <cmd>` (so env/PATH expansions work).
pub fn exec_shell_cmd(cmd: &str) -> ! {
    let sh = shell();
    let err = Command::new(&sh).arg("-lc").arg(cmd).exec();
    msg::die(&format!("exec {sh} failed: {err}"));
}

/// Replace this process with `prog args...`.
pub fn exec_command(prog: &str, args: &[&str]) -> ! {
    let err = Command::new(prog).args(args).exec();
    msg::die(&format!("exec {prog} failed: {err}"));
}

/// Single-quote a string for POSIX `sh -c` / ssh remote commands so paths with
/// spaces or specials survive. Bare words (alnum + a few safe punctuation) pass
/// through unquoted for readability.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Join an argv into a single shell-quoted command string (for `sh -lc` bodies
/// and ssh/mosh remote commands).
pub fn sh_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run `git -C <dir> <args...>`, returning success (stdout/stderr discarded).
pub fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Fix Login!!"), "fix-login");
        assert_eq!(slugify("  a  b  "), "a-b");
        assert_eq!(slugify("sz/Brisk_Otter"), "sz-brisk-otter");
    }

    #[test]
    fn basename_last_component() {
        assert_eq!(basename("/home/x/repo"), "repo");
        assert_eq!(basename("repo"), "repo");
    }

    #[test]
    fn age_buckets() {
        let n = now();
        assert_eq!(age(n), "0s");
        assert_eq!(age(n - 120), "2m");
        assert_eq!(age(n - 7200), "2h");
    }
}
