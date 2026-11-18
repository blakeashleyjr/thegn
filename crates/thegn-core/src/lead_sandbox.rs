//! The Lead sandbox policy (THE-2171, THE-2187): the Lead agent runs under
//! bwrap with the whole host read-only, its notes dir and harness state as the
//! only writable paths, and the daemon socket and run dirs masked so it cannot
//! reach the implicit-admin unix socket. No bwrap means no Lead (fail closed):
//! the host launcher never falls back to a host shell.
//!
//! Everything here is pure and platform-neutral; the host side
//! (`thegn-host` `lead::launch`) resolves the paths and probes bwrap.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// The bind layout for one Lead tab.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeadMountPlan {
    /// Host paths bound read-write at the same path (notes dir, harness state).
    pub writable: Vec<PathBuf>,
    /// Directories hidden behind an empty tmpfs (daemon socket dir, run dir,
    /// `$XDG_RUNTIME_DIR`).
    pub masked: Vec<PathBuf>,
    /// Non-directory entries (sockets) hidden behind a read-only `/dev/null`
    /// bind, since a tmpfs cannot cover a non-directory.
    pub masked_files: Vec<PathBuf>,
    /// Regular files under a masked directory that the Lead must still read
    /// (the MCP plan's token file); re-bound read-only after the masks.
    pub readable: Vec<PathBuf>,
    /// The working directory inside the sandbox (the notes dir).
    pub chdir: PathBuf,
    /// Keep bwrap's `--die-with-parent`. The interactive terminal path drops
    /// it for daemon-owned panes (the guard is keyed to the transient forking
    /// thread and would reap the session); the Lead follows the same rule.
    pub die_with_parent: bool,
}

/// Environment variables the sandbox unsets after entry: the control socket
/// the daemon injects (its dir is masked; the Lead talks through its scoped
/// loopback token instead), the session bus (host command authority), and the
/// ssh agent (the Lead never pushes).
pub const UNSET_ENV: &[&str] = &[
    "THEGN_CONTROL_SOCKET",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
];

/// The harness state the agent CLI must be able to write (session logs,
/// credentials refresh, settings). `env` is the effective launch env (host env
/// overlaid with the agent entry's and the account's env). Unknown harnesses
/// get nothing: the notes dir is their only writable path.
pub fn harness_state_paths(
    harness: &str,
    home: &Path,
    env: &BTreeMap<String, String>,
) -> Vec<PathBuf> {
    let var = |k: &str| {
        env.get(k)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(|v| PathBuf::from(crate::util::expand_tilde_in(v, home)))
    };
    match harness.trim() {
        "claude" => match var("CLAUDE_CONFIG_DIR") {
            // With CLAUDE_CONFIG_DIR set, claude keeps `.claude.json` inside it.
            Some(dir) => vec![dir],
            None => vec![home.join(".claude"), home.join(".claude.json")],
        },
        "codex" => vec![var("CODEX_HOME").unwrap_or_else(|| home.join(".codex"))],
        "pi" => vec![var("PI_CODING_AGENT_DIR").unwrap_or_else(|| home.join(".pi"))],
        _ => Vec::new(),
    }
}

/// The env var that relocates a harness's state dir when the harness has no
/// credential [`crate::harness::HomeSpec`] of its own (`Harness::home()` is
/// `None`). The Lead points it at a private dir so the harness never writes
/// its shared host state from inside the sandbox (and its sessions, skills
/// and settings stay apart from the worker sessions' copy).
///
/// - `pi` → `PI_CODING_AGENT_DIR`.
/// - `aider` keeps no login and writes its history and cache files into its
///   cwd, which for the Lead is the notes dir (already writable): it needs no
///   relocation.
/// - Every other harness without a home spec: `None` (the notes dir is its
///   only writable path).
pub fn private_home_env(harness_id: &str) -> Option<&'static str> {
    match harness_id.trim() {
        "pi" => Some("PI_CODING_AGENT_DIR"),
        _ => None,
    }
}

/// `a` equals `b` or lies beneath it, component-wise.
fn within(a: &Path, b: &Path) -> bool {
    a.starts_with(b)
}

fn lexically_clean(p: &Path) -> bool {
    p.is_absolute()
        && !p
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
}

/// Refuse a plan that would make any repo writable, re-expose a masked dir, or
/// hand the Lead `/` or `$HOME`. See [`validate_in`].
pub fn validate(plan: &LeadMountPlan, repo_roots: &[PathBuf]) -> Result<(), String> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    validate_in(plan, repo_roots, home.as_deref())
}

/// [`validate`] with `$HOME` injected. A writable path is refused when it:
/// - is not absolute or carries `.`/`..` components;
/// - is `/`, `$HOME`, or an ancestor of `$HOME`;
/// - is inside, equal to, or an ancestor of any repo root;
/// - is inside, equal to, or an ancestor of any masked path (a bind of an
///   ancestor would shadow the mask; a bind inside would punch through it).
///
/// Masked paths must be absolute and never `/`; the chdir must be absolute.
pub fn validate_in(
    plan: &LeadMountPlan,
    repo_roots: &[PathBuf],
    home: Option<&Path>,
) -> Result<(), String> {
    if !lexically_clean(&plan.chdir) {
        return Err(format!(
            "the Lead's working dir {} must be an absolute path",
            plan.chdir.display()
        ));
    }
    let masks = plan.masked.iter().chain(&plan.masked_files);
    for m in masks.clone() {
        if !lexically_clean(m) || m == Path::new("/") {
            return Err(format!(
                "the Lead sandbox cannot mask {}: masks must be absolute and never `/`",
                m.display()
            ));
        }
    }
    for r in &plan.readable {
        if !lexically_clean(r) {
            return Err(format!(
                "the Lead sandbox cannot re-expose {}: not an absolute path",
                r.display()
            ));
        }
    }
    for w in &plan.writable {
        let shown = w.display();
        if !lexically_clean(w) {
            return Err(format!(
                "the Lead cannot write {shown}: writable paths must be absolute without `.`/`..`"
            ));
        }
        if w == Path::new("/") {
            return Err("the Lead cannot have `/` writable".into());
        }
        if let Some(home) = home
            && within(home, w)
        {
            return Err(format!(
                "the Lead cannot write {shown}: it is $HOME or contains it"
            ));
        }
        for root in repo_roots {
            if within(w, root) || within(root, w) {
                return Err(format!(
                    "the Lead cannot write {shown}: it overlaps the repo {} (the Lead is read-only \
                     over every repo; point [lead] notes_dir outside your repos)",
                    root.display()
                ));
            }
        }
        for m in masks.clone() {
            if within(w, m) || within(m, w) {
                return Err(format!(
                    "the Lead cannot write {shown}: it overlaps the masked control path {}",
                    m.display()
                ));
            }
        }
    }
    Ok(())
}

/// Drop masks covered by another mask (or by the private `/tmp`) and order
/// the rest parents-first, so every tmpfs lands on a mount point that exists.
pub fn normalize_masks(masked: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted: Vec<PathBuf> = masked.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for m in sorted {
        if m.starts_with("/tmp") || out.iter().any(|kept| m.starts_with(kept)) {
            continue;
        }
        out.push(m);
    }
    out
}

/// The full bwrap argv. Order matters and is fixed:
/// 1. session flags (`--die-with-parent` only when the plan keeps it;
///    `--unshare-all --share-net`: no `--new-session`, a TUI needs its
///    controlling tty, matching the interactive terminal path);
/// 2. `--ro-bind / /`, fresh `/dev`, `/proc` and a private `/tmp`;
/// 3. masks (`--tmpfs` per dir, then `/dev/null` over each masked file);
/// 4. read-only re-binds of `readable` files;
/// 5. writable binds (after the masks; `validate` keeps them disjoint);
/// 6. env unsets, `--chdir`, `--`, then `inner`.
pub fn bwrap_argv(plan: &LeadMountPlan, inner: &[String]) -> Vec<String> {
    let s = |p: &Path| p.display().to_string();
    let mut v: Vec<String> = vec!["bwrap".into()];
    if plan.die_with_parent {
        v.push("--die-with-parent".into());
    }
    v.extend(
        [
            "--unshare-all",
            "--share-net",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
        ]
        .map(String::from),
    );
    for m in normalize_masks(&plan.masked) {
        v.extend(["--tmpfs".into(), s(&m)]);
    }
    for f in &plan.masked_files {
        v.extend(["--ro-bind".into(), "/dev/null".into(), s(f)]);
    }
    for r in &plan.readable {
        v.extend(["--ro-bind".into(), s(r), s(r)]);
    }
    for w in &plan.writable {
        v.extend(["--bind".into(), s(w), s(w)]);
    }
    for k in UNSET_ENV {
        v.extend(["--unsetenv".into(), (*k).to_string()]);
    }
    v.extend(["--chdir".into(), s(&plan.chdir), "--".into()]);
    v.extend(inner.iter().cloned());
    v
}

/// Whether bwrap can run here: Linux, and the same memoized local backend
/// probe the sandbox resolver uses (`sandbox_backend::available`). The one
/// impure function in this module; the host injects it into its launch so
/// tests can force the unavailable branch.
pub fn bwrap_available() -> bool {
    cfg!(target_os = "linux")
        && crate::sandbox_backend::available(
            &crate::placement::Placement::Local,
            crate::sandbox::Backend::Bwrap,
        ) == crate::placement::RuntimeProbe::Present
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn plan() -> LeadMountPlan {
        LeadMountPlan {
            writable: vec![p("/home/u/.local/state/thegn/lead"), p("/home/u/.claude")],
            masked: vec![p("/run/user/1000"), p("/home/u/.local/state/thegn/run")],
            masked_files: vec![p("/run/docker.sock")],
            readable: vec![],
            chdir: p("/home/u/.local/state/thegn/lead"),
            die_with_parent: false,
        }
    }

    const HOME: &str = "/home/u";

    fn check(plan: &LeadMountPlan, roots: &[&str]) -> Result<(), String> {
        let roots: Vec<PathBuf> = roots.iter().map(|r| p(r)).collect();
        validate_in(plan, &roots, Some(Path::new(HOME)))
    }

    #[test]
    fn harness_state_paths_defaults_per_harness() {
        let home = p(HOME);
        let none = BTreeMap::new();
        assert_eq!(
            harness_state_paths("claude", &home, &none),
            vec![p("/home/u/.claude"), p("/home/u/.claude.json")]
        );
        assert_eq!(
            harness_state_paths("codex", &home, &none),
            vec![p("/home/u/.codex")]
        );
        assert_eq!(
            harness_state_paths("pi", &home, &none),
            vec![p("/home/u/.pi")]
        );
        assert!(harness_state_paths("aider", &home, &none).is_empty());
        assert!(harness_state_paths("", &home, &none).is_empty());
    }

    #[test]
    fn harness_state_paths_honours_env_overrides() {
        let home = p(HOME);
        let e = env(&[
            ("CLAUDE_CONFIG_DIR", "/acct/claude-work"),
            ("CODEX_HOME", "~/codex-work"),
            ("PI_CODING_AGENT_DIR", "/acct/pi"),
        ]);
        assert_eq!(
            harness_state_paths("claude", &home, &e),
            vec![p("/acct/claude-work")],
            "a config dir override holds .claude.json itself"
        );
        assert_eq!(
            harness_state_paths("codex", &home, &e),
            vec![p("/home/u/codex-work")]
        );
        assert_eq!(harness_state_paths("pi", &home, &e), vec![p("/acct/pi")]);
        // A blank override falls back to the default.
        let blank = env(&[("CODEX_HOME", "  ")]);
        assert_eq!(
            harness_state_paths("codex", &home, &blank),
            vec![p("/home/u/.codex")]
        );
    }

    #[test]
    fn private_home_env_covers_pi_only() {
        assert_eq!(private_home_env("pi"), Some("PI_CODING_AGENT_DIR"));
        assert_eq!(private_home_env(" pi "), Some("PI_CODING_AGENT_DIR"));
        assert_eq!(private_home_env("aider"), None, "aider writes into its cwd");
        // A harness with a credential home spec uses that, never this table.
        assert_eq!(private_home_env("claude"), None);
        assert_eq!(private_home_env("codex"), None);
        assert_eq!(private_home_env(""), None);
        // The table agrees with the state paths the sandbox already knows.
        let home = p(HOME);
        let e = env(&[("PI_CODING_AGENT_DIR", "/private/pi")]);
        assert_eq!(harness_state_paths("pi", &home, &e), vec![p("/private/pi")]);
    }

    #[test]
    fn validate_accepts_a_disjoint_plan() {
        assert_eq!(check(&plan(), &["/home/u/code/thegn", "/srv/repo"]), Ok(()));
    }

    #[test]
    fn validate_refuses_writable_inside_a_repo() {
        let mut pl = plan();
        pl.writable = vec![p("/home/u/code/thegn/notes")];
        let err = check(&pl, &["/home/u/code/thegn"]).unwrap_err();
        assert!(err.contains("/home/u/code/thegn"), "{err}");
    }

    #[test]
    fn validate_refuses_writable_equal_to_a_repo() {
        let mut pl = plan();
        pl.writable = vec![p("/srv/repo")];
        assert!(check(&pl, &["/srv/repo"]).is_err());
    }

    #[test]
    fn validate_refuses_writable_ancestor_of_a_repo() {
        let mut pl = plan();
        pl.writable = vec![p("/srv")];
        assert!(check(&pl, &["/srv/repo"]).is_err());
    }

    #[test]
    fn validate_is_component_wise_not_string_prefix() {
        let mut pl = plan();
        pl.writable = vec![p("/srv/repo-notes")];
        assert_eq!(check(&pl, &["/srv/repo"]), Ok(()));
    }

    #[test]
    fn validate_refuses_root_and_home() {
        let mut pl = plan();
        pl.writable = vec![p("/")];
        assert!(check(&pl, &[]).is_err());
        pl.writable = vec![p(HOME)];
        assert!(check(&pl, &[]).unwrap_err().contains("$HOME"));
        pl.writable = vec![p("/home")];
        assert!(check(&pl, &[]).is_err(), "an ancestor of $HOME is $HOME");
    }

    #[test]
    fn validate_refuses_relative_and_dotdot_paths() {
        let mut pl = plan();
        pl.writable = vec![p("notes")];
        assert!(check(&pl, &[]).is_err());
        pl.writable = vec![p("/home/u/.local/../code/thegn")];
        assert!(check(&pl, &["/home/u/code/thegn"]).is_err());
        let mut pl = plan();
        pl.chdir = p("rel");
        assert!(check(&pl, &[]).is_err());
    }

    #[test]
    fn validate_refuses_writable_overlapping_a_mask() {
        let mut pl = plan();
        pl.writable = vec![p("/run/user/1000/thegn")];
        assert!(check(&pl, &[]).is_err(), "inside a mask punches through it");
        pl.writable = vec![p("/home/u/.local/state")];
        assert!(
            check(&pl, &[]).is_err(),
            "an ancestor bind shadows the mask"
        );
        pl.writable = vec![p("/run/docker.sock")];
        assert!(check(&pl, &[]).is_err());
    }

    #[test]
    fn validate_refuses_masking_root_or_relative() {
        let mut pl = plan();
        pl.masked = vec![p("/")];
        assert!(check(&pl, &[]).is_err());
        pl.masked = vec![p("run")];
        assert!(check(&pl, &[]).is_err());
        let mut pl = plan();
        pl.readable = vec![p("tok")];
        assert!(check(&pl, &[]).is_err());
    }

    #[test]
    fn normalize_masks_drops_covered_and_tmp() {
        let got = normalize_masks(&[
            p("/run/user/1000/thegn/k"),
            p("/run/user/1000"),
            p("/tmp/thegn-1000-k"),
            p("/home/u/.local/state/thegn/run"),
            p("/run/user/1000"),
        ]);
        assert_eq!(
            got,
            vec![p("/home/u/.local/state/thegn/run"), p("/run/user/1000")]
        );
    }

    fn pos(v: &[String], needle: &[&str]) -> usize {
        v.windows(needle.len())
            .position(|w| w.iter().map(String::as_str).eq(needle.iter().copied()))
            .unwrap_or_else(|| panic!("{needle:?} not in {v:?}"))
    }

    #[test]
    fn bwrap_argv_orders_root_masks_binds_then_inner() {
        let mut pl = plan();
        pl.readable = vec![p("/run/user/1000/thegn/lead.token")];
        let inner = vec![
            "sh".to_string(),
            "-lc".to_string(),
            "exec claude".to_string(),
        ];
        let v = bwrap_argv(&pl, &inner);
        assert_eq!(v[0], "bwrap");
        let unshare = pos(&v, &["--unshare-all", "--share-net"]);
        let root = pos(&v, &["--ro-bind", "/", "/"]);
        let tmp = pos(&v, &["--tmpfs", "/tmp"]);
        let mask_rt = pos(&v, &["--tmpfs", "/run/user/1000"]);
        let mask_run = pos(&v, &["--tmpfs", "/home/u/.local/state/thegn/run"]);
        let mask_file = pos(&v, &["--ro-bind", "/dev/null", "/run/docker.sock"]);
        let readable = pos(
            &v,
            &[
                "--ro-bind",
                "/run/user/1000/thegn/lead.token",
                "/run/user/1000/thegn/lead.token",
            ],
        );
        let notes = pos(
            &v,
            &[
                "--bind",
                "/home/u/.local/state/thegn/lead",
                "/home/u/.local/state/thegn/lead",
            ],
        );
        let claude = pos(&v, &["--bind", "/home/u/.claude", "/home/u/.claude"]);
        let unset = pos(&v, &["--unsetenv", "THEGN_CONTROL_SOCKET"]);
        let chdir = pos(&v, &["--chdir", "/home/u/.local/state/thegn/lead"]);
        let sep = pos(&v, &["--"]);
        assert!(unshare < root, "session flags first");
        assert!(root < tmp && tmp < mask_run && mask_run < mask_rt);
        assert!(mask_rt < mask_file && mask_file < readable);
        assert!(readable < notes && notes < claude, "writables after masks");
        assert!(claude < unset && unset < chdir && chdir < sep);
        assert_eq!(&v[sep + 1..], &inner[..], "`--` immediately precedes inner");
        assert!(
            !v.iter().any(|a| a == "--new-session"),
            "a TUI keeps its tty"
        );
        assert!(!v.iter().any(|a| a == "--die-with-parent"));
    }

    #[test]
    fn bwrap_argv_keeps_die_with_parent_when_asked() {
        let mut pl = plan();
        pl.die_with_parent = true;
        let v = bwrap_argv(&pl, &[]);
        assert_eq!(v[1], "--die-with-parent");
        assert_eq!(v.last().map(String::as_str), Some("--"));
    }

    #[test]
    fn validate_reads_home_from_the_env_and_still_refuses_root() {
        let mut pl = plan();
        pl.writable = vec![p("/")];
        assert!(validate(&pl, &[]).is_err());
    }

    #[test]
    fn bwrap_available_is_false_off_linux() {
        let available = bwrap_available();
        if !cfg!(target_os = "linux") {
            assert!(!available);
        }
    }

    #[test]
    fn bwrap_argv_unsets_every_listed_var() {
        let v = bwrap_argv(&LeadMountPlan::default(), &[]);
        for k in UNSET_ENV {
            pos(&v, &["--unsetenv", k]);
        }
    }
}
