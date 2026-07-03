//! Small shared helpers: XDG paths, tilde expansion, slugify, age formatting,
//! and thin subprocess wrappers (git / generic commands).

use crate::msg;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(not(windows))]
use std::os::unix::process::CommandExt;

#[cfg(not(windows))]
pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(windows)]
pub fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\"))
}

#[cfg(not(windows))]
pub fn xdg_config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

#[cfg(windows)]
pub fn xdg_config_home() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData").join("Roaming"))
}

#[cfg(not(windows))]
pub fn xdg_state_home() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
}

#[cfg(windows)]
pub fn xdg_state_home() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData").join("Local"))
}

/// superzej's own home — config, worktrees, cache, activity all live under here
/// (`~/.superzej`). `SUPERZEJ_DIR` relocates it so a dev/test instance can run on
/// a fully separate root (its own cache, config and worktrees) without touching
/// your daily-driver superzej. Pair it with `XDG_STATE_HOME` to also isolate the
/// DB (see `just start-term`).
pub fn superzej_dir() -> PathBuf {
    std::env::var_os("SUPERZEJ_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".superzej"))
}

/// The superzej-MANAGED pi install root (`~/.superzej/pi`): a pinned pi binary
/// (`node_modules/.bin/pi`) plus the managed agent dir below. Self-contained +
/// reproducible — owned by superzej, not the host's global `pi`/`~/.pi`.
pub fn managed_pi_dir() -> PathBuf {
    superzej_dir().join("pi")
}

/// The managed pi's `PI_CODING_AGENT_DIR` (`~/.superzej/pi/agent`) — its config,
/// settings, and the seeded `superzej-acp` extension package live here.
pub fn managed_pi_agent_dir() -> PathBuf {
    managed_pi_dir().join("agent")
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

/// A short, STABLE alphanumeric digest of a string — deterministic across runs,
/// processes, platforms, and Rust versions (unlike `DefaultHasher`, whose output
/// is explicitly not stable). Used to give per-worktree sandbox names a
/// collision-defusing suffix derived from the worktree's full path, so two
/// worktrees whose human-readable parts (repo + branch) coincide still map to
/// distinct sandboxes. FNV-1a 64-bit → fixed `len` base36 chars (36^6 ≈ 2.2e9 of
/// space at the default length — ample for disambiguating worktrees).
pub fn short_hash(s: &str, len: usize) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = vec![b'0'; len.max(1)];
    for slot in buf.iter_mut().rev() {
        *slot = ALPHABET[(h % 36) as usize];
        h /= 36;
    }
    String::from_utf8(buf).unwrap()
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
    which_path(cmd).is_some()
}

/// Return the absolute path of `cmd` found on `PATH`, or `None` if not found.
pub fn which_path(cmd: &str) -> Option<String> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let p = dir.join(cmd);
        if p.is_file() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

/// The repo-targeting env vars git exports into hook/child environments. Left
/// in place they silently retarget any plain `git` invocation at the OUTER
/// repo — the cause of the `core.worktree` pollution where an agent's
/// `git worktree add`, run with an inherited GIT_DIR/GIT_WORK_TREE, wrote a
/// stray `core.worktree` into the shared main `.git/config` and made every
/// subsequent read target the wrong tree. Scrubbed both per-invocation
/// ([`git_cmd`]) and process-wide at startup ([`scrub_git_env`]).
pub const GIT_ENV_VARS: [&str; 7] = [
    "GIT_DIR",
    "GIT_INDEX_FILE",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// Remove every repo-targeting git env var from the CURRENT process, so neither
/// our own git calls nor anything we spawn (pane shells, agents, sandboxes,
/// hooks) inherits a poisoned GIT_DIR/GIT_WORK_TREE. superzej always targets
/// git explicitly with `-C <dir>`, so it never needs an ambient GIT_DIR.
///
/// MUST be called at the very top of `main`, before the tokio runtime (or any
/// other thread) starts: mutating the environment is unsound while other
/// threads may be reading it.
///
/// # Safety
/// Single-threaded-startup invariant as above; `std::env::remove_var` is
/// `unsafe` under edition 2024 for exactly that reason.
pub fn scrub_git_env() {
    for var in GIT_ENV_VARS {
        unsafe { std::env::remove_var(var) };
    }
}

/// Exact env-var names carried from superzej's own process into a freshly
/// spawned pane. This is the *allowlist* half of the clear-then-allowlist pane
/// env firewall ([`crate`] consumers call [`host_base_env`]): a pane starts from
/// an EMPTY environment seeded only with these infrastructure vars, then the
/// caller layers the pane's identity (env-bundle / profile / agent env) on top.
///
/// Credential-shaped vars the launching shell exported (`GH_TOKEN`,
/// `ANTHROPIC_API_KEY`, `SSH_AUTH_SOCK`, `*_TOKEN`/`*_KEY`/`*_SECRET`/…) are
/// therefore NOT inherited by default — closing the "every pane sees every var
/// szhost inherited" leak that both env-bundles (AU) and process-profiles (H)
/// depend on. The list is generous on *infrastructure* (locale, terminal,
/// display) but carries no secrets; extra names are re-admitted via
/// [`set_host_env_allow_extra`].
pub const HOST_ENV_ALLOW_EXACT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PWD",
    "OLDPWD",
    "LANG",
    "LANGUAGE",
    "TERM",
    "COLORTERM",
    "TERMINFO",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TZ",
    "TMPDIR",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "SSH_TTY",
    // Credential *config-dir* vars (they name a dir/file, not a secret value):
    // safe to carry so a pane inherits the active profile's git/gh/gpg identity
    // (or, on the default profile, the user's own). The secret token vars
    // (`GH_TOKEN`/`*_KEY`/…) are deliberately NOT here — they stay firewalled.
    "GIT_CONFIG_GLOBAL",
    "GH_CONFIG_DIR",
    "GNUPGHOME",
    "GIT_SSH_COMMAND",
    "GPG_TTY",
];

/// Prefix families admitted alongside [`HOST_ENV_ALLOW_EXACT`]:
/// - `LC_*` — locale categories.
/// - `XDG_*` — base-dir spec, incl. `XDG_RUNTIME_DIR` (rootless podman needs it).
/// - `DBUS_*` — the session bus a rootless container runtime talks to.
/// - `NIX_*` — dev-shell plumbing (`NIX_PATH`, `NIX_PROFILES`, …).
/// - `SUPERZEJ_*` — our own non-secret context markers (profile, sandbox flag).
///
/// None of these families carry credentials; secrets ship under distinct names
/// (`*_TOKEN`/`*_KEY`/…) that no family matches.
pub const HOST_ENV_ALLOW_PREFIX: &[&str] = &["LC_", "XDG_", "DBUS_", "NIX_", "SUPERZEJ_"];

/// Process-global extra allowlist (config `[sandbox] host_env_allow`), set once
/// at startup. Same write-once holder pattern as the render-time caps/palette:
/// read cheaply from every pane spawn without threading config through
/// `spawn_with_env`. Empty until set.
static HOST_ENV_ALLOW_EXTRA: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Install the config-driven extra host-env allowlist. Idempotent-by-first-write
/// (`OnceLock`): call once at startup after config load. Later calls are no-ops.
pub fn set_host_env_allow_extra(extra: Vec<String>) {
    let _ = HOST_ENV_ALLOW_EXTRA.set(extra);
}

/// The configured extra host-env allowlist (empty if never set).
pub fn host_env_allow_extra() -> &'static [String] {
    HOST_ENV_ALLOW_EXTRA.get().map(Vec::as_slice).unwrap_or(&[])
}

/// Pure allowlist filter: keep only vars whose key is in
/// [`HOST_ENV_ALLOW_EXACT`], matches a [`HOST_ENV_ALLOW_PREFIX`] family, or is
/// explicitly re-admitted via `extra`. Order-preserving. The testable core of
/// the pane-env firewall (the I/O wrapper [`host_base_env`] just feeds it
/// `std::env::vars()`).
pub fn filter_host_env<I>(vars: I, extra: &[String]) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    vars.into_iter()
        .filter(|(k, _)| {
            HOST_ENV_ALLOW_EXACT.contains(&k.as_str())
                || HOST_ENV_ALLOW_PREFIX.iter().any(|p| k.starts_with(p))
                || extra.iter().any(|e| e == k)
        })
        .collect()
}

/// The current process environment filtered through [`filter_host_env`] with the
/// configured [`host_env_allow_extra`] — the seed env for a freshly spawned pane.
pub fn host_base_env() -> Vec<(String, String)> {
    filter_host_env(std::env::vars(), host_env_allow_extra())
}

/// A `git -C <dir>` command with the parent's repo-targeting env scrubbed.
/// When superzej (or its test suite, via a pre-commit hook) runs inside a git
/// hook, git exports GIT_DIR/GIT_INDEX_FILE/GIT_WORK_TREE — often as paths
/// RELATIVE to the outer repo — which would mis-target these explicit `-C`
/// invocations (`git worktree add` dies with "index file open failed").
pub fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    for var in GIT_ENV_VARS {
        c.env_remove(var);
    }
    // Read-side housekeeping must never touch `.git/index.lock`. superzej hydrates
    // the sidebar on a recurring schedule (the ~5s model ticker plus the
    // 500ms-debounced diff fs-watcher, which fires up to ~2 Hz during active
    // editing) via `git status`/`git diff`, and those reads otherwise take git's
    // *optional* lock to refresh the racy-git index stat cache — contending with
    // any concurrent git the user runs in the same worktree. `GIT_OPTIONAL_LOCKS=0`
    // suppresses only those optional sub-operations; operations that *require* the
    // lock (add/commit/merge/rebase/worktree add/stash) still take it, so the write
    // path is unchanged.
    c.env("GIT_OPTIONAL_LOCKS", "0");
    c
}

/// Defensive self-heal: a non-bare MAIN checkout must never carry
/// `core.worktree`. A stray GIT_DIR/GIT_WORK_TREE in some child `git`
/// invocation (an agent shell, a worktree op run inside a git hook) can leak it
/// into the shared `.git/config`, after which every read — superzej's diff
/// panel included — targets that other tree. If `root` is a main checkout (its
/// `.git` is a directory, not a linked-worktree `.git` file, which legitimately
/// uses core.worktree) and the key is set, strip it. Returns whether it healed.
pub fn heal_main_checkout_worktree(root: &Path) -> bool {
    // Only a main checkout has `.git` as a directory; linked worktrees have a
    // `.git` FILE and their per-worktree config rightly sets core.worktree.
    let cfg_path = root.join(".git/config");
    if !root.join(".git").is_dir() || !cfg_path.is_file() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(&cfg_path) else {
        return false;
    };
    let Some(cleaned) = strip_core_worktree(&text) else {
        return false; // nothing to do
    };
    // git itself can't be used to fix this: it canonicalizes the core.worktree
    // VALUE on every config read, so a stray entry pointing at a now-missing
    // path makes `git config --get/--unset/--list` all abort with "Invalid
    // path". A surgical text edit (drop the `worktree` line inside `[core]`,
    // everything else byte-for-byte) is the only reliable repair.
    if std::fs::write(&cfg_path, cleaned).is_ok() {
        tracing::warn!(
            target: "szhost::startup",
            root = %root.display(),
            "stripped stray core.worktree from main checkout config (was retargeting git at another worktree)"
        );
        return true;
    }
    false
}

/// Resolve the shared git-common dir (the canonical `.git`) for a worktree,
/// WITHOUT shelling out. A main checkout's `.git` is a directory; a linked
/// worktree's `.git` is a file `gitdir: <per-worktree-gitdir>`, whose
/// `commondir` file points at the shared `.git`. Used to key the per-repo git
/// lock so every worktree of a repo serializes on the same lock file.
pub fn git_common_dir(worktree: &Path) -> PathBuf {
    let dot_git = worktree.join(".git");
    if dot_git.is_dir() {
        return dot_git;
    }
    if let Ok(text) = std::fs::read_to_string(&dot_git)
        && let Some(p) = text
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("gitdir:"))
            .map(str::trim)
    {
        let gitdir = if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            worktree.join(p)
        };
        if let Ok(cd) = std::fs::read_to_string(gitdir.join("commondir")) {
            let cd = cd.trim();
            return if Path::new(cd).is_absolute() {
                PathBuf::from(cd)
            } else {
                gitdir.join(cd)
            };
        }
        // Fallback: `<gitdir>/../..` is `<canonical>/.git`.
        if let Some(parent2) = gitdir.parent().and_then(Path::parent) {
            return parent2.to_path_buf();
        }
    }
    dot_git
}

/// A held cross-process advisory lock around git MUTATIONS on a shared repo.
/// Multiple szhost/agent processes operating on the same canonical `.git` would
/// otherwise race it (concurrent `worktree add`/commit/rebase clobbering the
/// shared index/refs/config — the corruption behind the core.worktree saga).
/// `flock` is advisory and tied to the open fd, so the lock auto-releases on
/// `Drop` AND on process death — there are never stale locks. Reads stay
/// lock-free; only the svc write runners acquire this.
#[must_use = "the lock releases as soon as the guard is dropped"]
pub struct GitLock(std::fs::File);

/// Acquire the per-repo git-mutation lock (blocking) at
/// `<git-common>/superzej-git.lock`, serializing concurrent mutations on the
/// same `.git`. Best-effort: returns `None` (degrading to today's unlocked
/// behavior) if the lock file can't be opened/locked, so a permissions quirk
/// never wedges the user out of git. Call only on background threads — it blocks.
#[cfg(not(windows))]
pub fn lock_git_mutations(worktree: &Path) -> Option<GitLock> {
    use std::os::unix::io::AsRawFd;
    let path = git_common_dir(worktree).join("superzej-git.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .ok()?;
    // SAFETY: a plain flock(2) on a live fd we own; LOCK_EX blocks until granted.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    (rc == 0).then_some(GitLock(file))
}

#[cfg(windows)]
pub fn lock_git_mutations(worktree: &Path) -> Option<GitLock> {
    let path = git_common_dir(worktree).join("superzej-git.lock");
    // On Windows, opening a file with `share_read=false` and `share_write=false`
    // creates an exclusive lock at the filesystem level.
    // However, Rust's stdlib doesn't expose sharing modes directly in OpenOptions without `std::os::windows::fs::OpenOptionsExt`.
    use std::os::windows::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .share_mode(0) // 0 = exclusive lock
        .open(&path)
        .ok()?;
    Some(GitLock(file))
}

#[cfg(not(windows))]
impl Drop for GitLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: same fd we locked; explicit unlock (close would also release).
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(windows)]
impl Drop for GitLock {
    fn drop(&mut self) {
        // Just closing the file releases the share mode lock on Windows.
    }
}

/// Drop the `worktree = …` entry from the `[core]` section of a git config
/// file's text, returning the rewritten text — or `None` if there is no such
/// entry (so the caller can skip the write). Only the `[core]` section is
/// touched; subsections like `[core "x"]` and every other line are preserved
/// verbatim. core.worktree is the only key that retargets a checkout's tree.
fn strip_core_worktree(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut in_core = false;
    let mut removed = false;
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix('[') {
            // Section header: `[core]` enters the section; `[core "sub"]` or any
            // other header leaves it.
            let head = rest.split(']').next().unwrap_or("").trim();
            in_core = head.eq_ignore_ascii_case("core");
        } else if in_core {
            // A key line `worktree = …` (git keys are case-insensitive, '=' or
            // whitespace separated).
            let key = t.split(['=', ' ', '\t']).next().unwrap_or("").trim();
            if key.eq_ignore_ascii_case("worktree") {
                removed = true;
                continue; // drop this line
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    removed.then_some(out)
}

/// Run `git -C <dir> <args...>`, returning trimmed stdout on success (None on
/// failure or empty output).
pub fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_cmd(dir).args(args).output().ok()?;
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

#[cfg(not(windows))]
pub fn shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

#[cfg(windows)]
pub fn shell() -> String {
    if let Ok(pwsh) = which_path("pwsh.exe") {
        pwsh
    } else if let Ok(ps) = which_path("powershell.exe") {
        ps
    } else {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
    }
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

/// Set the terminal (pane) window title via OSC. Any program run afterwards
/// (vim, lazygit, …) overrides it as usual, so this just seeds a sensible
/// default (the branch/worktree name).
pub fn set_terminal_title(title: &str) {
    use std::io::Write;
    crate::out!("\u{1b}]0;{title}\u{07}");
    let _ = std::io::stdout().flush();
}

/// Replace this process with an interactive login shell.
#[cfg(not(windows))]
pub fn exec_shell() -> ! {
    let sh = shell();
    let err = Command::new(&sh).arg("-l").exec();
    msg::die(&format!("exec {sh} failed: {err}"));
}

#[cfg(windows)]
pub fn exec_shell() -> ! {
    let sh = shell();
    let mut cmd = Command::new(&sh);
    let err = if sh.ends_with("pwsh.exe") || sh.ends_with("powershell.exe") {
        cmd.arg("-NoLogo").spawn().and_then(|mut c| c.wait())
    } else {
        cmd.spawn().and_then(|mut c| c.wait())
    };
    msg::die(&format!("exec {sh} failed: {:?}", err));
}

#[cfg(not(windows))]
pub fn exec_shell_cmd(cmd: &str) -> ! {
    let sh = shell();
    let err = Command::new(&sh).arg("-lc").arg(cmd).exec();
    msg::die(&format!("exec {sh} failed: {err}"));
}

#[cfg(windows)]
pub fn exec_shell_cmd(cmd: &str) -> ! {
    let sh = shell();
    let mut c = Command::new(&sh);
    let err = if sh.ends_with("pwsh.exe") || sh.ends_with("powershell.exe") {
        c.args(["-NoProfile", "-Command", cmd])
            .spawn()
            .and_then(|mut proc| proc.wait())
    } else if sh.ends_with("cmd.exe") {
        c.args(["/C", cmd]).spawn().and_then(|mut proc| proc.wait())
    } else {
        c.arg("-c")
            .arg(cmd)
            .spawn()
            .and_then(|mut proc| proc.wait())
    };
    msg::die(&format!("exec {sh} failed: {:?}", err));
}

#[cfg(not(windows))]
pub fn exec_command(prog: &str, args: &[&str]) -> ! {
    let err = Command::new(prog).args(args).exec();
    msg::die(&format!("exec {prog} failed: {err}"));
}

#[cfg(windows)]
pub fn exec_command(prog: &str, args: &[&str]) -> ! {
    let err = Command::new(prog)
        .args(args)
        .spawn()
        .and_then(|mut proc| proc.wait());
    msg::die(&format!("exec {prog} failed: {:?}", err));
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
    git_cmd(dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_pi_dirs_nest_under_superzej_dir() {
        let base = superzej_dir();
        assert_eq!(managed_pi_dir(), base.join("pi"));
        assert_eq!(managed_pi_agent_dir(), base.join("pi").join("agent"));
        assert!(managed_pi_agent_dir().ends_with("pi/agent"));
    }

    #[test]
    fn git_common_dir_resolves_main_and_linked() {
        let tmp = std::env::temp_dir().join(format!("sz-gcd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // Main checkout: `.git` is a directory → it IS the common dir.
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        assert_eq!(git_common_dir(&main), main.join(".git"));
        // Linked worktree: `.git` is a file → per-worktree gitdir → commondir.
        let wt_gitdir = main.join(".git/worktrees/feat");
        std::fs::create_dir_all(&wt_gitdir).unwrap();
        std::fs::write(wt_gitdir.join("commondir"), "../..\n").unwrap();
        let wt = tmp.join("feat");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .unwrap();
        // `../..` from `<main>/.git/worktrees/feat` resolves to `<main>/.git`.
        assert_eq!(git_common_dir(&wt), wt_gitdir.join("../.."));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn git_lock_acquires_and_re_acquires_after_drop() {
        let tmp = std::env::temp_dir().join(format!("sz-glock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        let guard = lock_git_mutations(&tmp);
        assert!(guard.is_some(), "first acquire succeeds");
        drop(guard); // releases (flock auto-drops); no stale lock left behind
        assert!(
            lock_git_mutations(&tmp).is_some(),
            "re-acquire after drop succeeds"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

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

    #[test]
    fn host_env_filter_keeps_infra_drops_secrets() {
        let input = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("HOME".to_string(), "/home/x".to_string()),
            ("LC_ALL".to_string(), "C".to_string()),
            ("XDG_RUNTIME_DIR".to_string(), "/run/user/1000".to_string()),
            (
                "DBUS_SESSION_BUS_ADDRESS".to_string(),
                "unix:...".to_string(),
            ),
            ("NIX_PATH".to_string(), "nixpkgs=...".to_string()),
            ("SUPERZEJ_PROFILE".to_string(), "work".to_string()),
            // Secrets / launcher-shell leakage — must be dropped.
            ("GH_TOKEN".to_string(), "ghp_xxx".to_string()),
            ("GITHUB_TOKEN".to_string(), "ghp_yyy".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "sk-ant".to_string()),
            ("SSH_AUTH_SOCK".to_string(), "/tmp/agent".to_string()),
            ("MY_SECRET".to_string(), "hunter2".to_string()),
            ("AWS_SECRET_ACCESS_KEY".to_string(), "z".to_string()),
        ];
        let out = filter_host_env(input, &[]);
        let keys: std::collections::HashSet<&str> = out.iter().map(|(k, _)| k.as_str()).collect();
        for keep in [
            "PATH",
            "HOME",
            "LC_ALL",
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
            "NIX_PATH",
            "SUPERZEJ_PROFILE",
        ] {
            assert!(
                keys.contains(keep),
                "{keep} must survive the host-env filter"
            );
        }
        for drop in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "ANTHROPIC_API_KEY",
            "SSH_AUTH_SOCK",
            "MY_SECRET",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(
                !keys.contains(drop),
                "{drop} must NOT leak past the host-env filter"
            );
        }
    }

    #[test]
    fn host_env_filter_extra_readmits_named_var() {
        let input = vec![("SSH_AUTH_SOCK".to_string(), "/tmp/agent".to_string())];
        // Not admitted by default…
        assert!(filter_host_env(input.clone(), &[]).is_empty());
        // …but an explicit config re-admit lets it through.
        let out = filter_host_env(input, &["SSH_AUTH_SOCK".to_string()]);
        assert_eq!(
            out,
            vec![("SSH_AUTH_SOCK".to_string(), "/tmp/agent".to_string())]
        );
    }

    #[test]
    fn git_env_scrub_list_covers_dir_and_worktree() {
        // The two vars that actually retarget git at another tree must be in
        // the scrub set (the rest are belt-and-suspenders).
        assert!(GIT_ENV_VARS.contains(&"GIT_DIR"));
        assert!(GIT_ENV_VARS.contains(&"GIT_WORK_TREE"));
        assert!(GIT_ENV_VARS.contains(&"GIT_COMMON_DIR"));
    }

    #[test]
    fn git_cmd_marks_every_git_env_var_for_removal() {
        // Regression guard for the worktree/pre-commit-hook corruption class:
        // when the test suite (or any tool) is spawned by a git hook, git has
        // exported GIT_DIR/GIT_INDEX_FILE into the environment. `git_cmd` must
        // strip *every* GIT_ENV_VARS entry from the child so a `-C <dir>` call
        // operates on the intended repo, never the outer one. `env_remove`
        // surfaces in `get_envs()` as `(key, None)`; assert all are present.
        // (Thread-safe: inspects the Command's env overrides, never mutates the
        // process environment.)
        let cmd = git_cmd(std::path::Path::new("/tmp"));
        let removed: std::collections::HashSet<&std::ffi::OsStr> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k)
            .collect();
        for var in GIT_ENV_VARS {
            assert!(
                removed.contains(std::ffi::OsStr::new(var)),
                "git_cmd must scrub {var} from the child environment"
            );
        }
    }

    #[test]
    fn git_cmd_disables_optional_locks() {
        // Recurring read housekeeping (`git status`/`diff` hydration) must not
        // take `.git/index.lock`; `git_cmd` sets GIT_OPTIONAL_LOCKS=0 so the
        // optional stat-cache refresh is skipped. Required write locks are
        // unaffected. Inspects the Command's env overrides only — thread-safe.
        let cmd = git_cmd(std::path::Path::new("/tmp"));
        let set: std::collections::HashMap<&std::ffi::OsStr, &std::ffi::OsStr> = cmd
            .get_envs()
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect();
        assert_eq!(
            set.get(std::ffi::OsStr::new("GIT_OPTIONAL_LOCKS")).copied(),
            Some(std::ffi::OsStr::new("0")),
            "git_cmd must set GIT_OPTIONAL_LOCKS=0 so read housekeeping never locks the index"
        );
    }

    #[test]
    fn strip_core_worktree_removes_only_the_core_section_key() {
        // Pollution under [core] is removed; the [core "sub"] subsection and a
        // legitimate worktree key in another section are preserved.
        let src = "\
[core]
\trepositoryformatversion = 0
\tbare = false
\tworktree = /no/such/tree
[remote \"origin\"]
\turl = https://example/r.git
[core \"sub\"]
\tworktree = keep-me
";
        let out = strip_core_worktree(src).expect("a [core].worktree was present");
        assert!(!out.contains("worktree = /no/such/tree"));
        assert!(out.contains("worktree = keep-me"), "subsection untouched");
        assert!(out.contains("bare = false"));
        assert!(out.contains("url = https://example/r.git"));
        // No [core].worktree left, and no spurious changes (line count -1).
        assert_eq!(out.lines().count(), src.lines().count() - 1);
        // A clean config returns None (caller skips the write).
        assert!(strip_core_worktree("[core]\n\tbare = false\n").is_none());
    }

    #[test]
    fn heal_strips_stray_core_worktree_even_with_a_missing_target() {
        // Build a real main checkout, then inject the pollution by TEXT — a
        // missing target path, the worst case where `git config` itself aborts.
        let dir = std::env::temp_dir().join(format!("sz-heal-strip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            git_cmd(&dir)
                .args(["init", "-q", "-b", "main"])
                .status()
                .unwrap()
                .success()
        );
        let cfg = dir.join(".git/config");
        let mut text = std::fs::read_to_string(&cfg).unwrap();
        text.push_str("\tworktree = /no/such/tree\n"); // append under [core]
        std::fs::write(&cfg, &text).unwrap();

        assert!(
            heal_main_checkout_worktree(&dir),
            "heal should report a fix"
        );
        assert!(
            !std::fs::read_to_string(&cfg)
                .unwrap()
                .contains("worktree ="),
            "core.worktree line is gone after heal"
        );
        // Idempotent: a clean repo is a no-op.
        assert!(!heal_main_checkout_worktree(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn heal_leaves_linked_worktrees_untouched() {
        // A linked worktree's `.git` is a FILE, and its config legitimately
        // sets core.worktree — heal must never touch those.
        let dir = std::env::temp_dir().join(format!("sz-heal-linked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".git"), "gitdir: /elsewhere/.git/worktrees/wt\n").unwrap();

        assert!(
            !heal_main_checkout_worktree(&dir),
            "a .git FILE (linked worktree) is never healed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(windows)]
    fn test_windows_shell_resolution() {
        let shell = shell();
        assert!(
            shell.ends_with("pwsh.exe")
                || shell.ends_with("powershell.exe")
                || shell.ends_with("cmd.exe")
        );
    }

    #[test]
    #[cfg(windows)]
    fn test_windows_xdg_paths() {
        let config_home = xdg_config_home();
        assert!(config_home.to_string_lossy().contains("AppData"));
        let state_home = xdg_state_home();
        assert!(state_home.to_string_lossy().contains("AppData"));
    }
}
