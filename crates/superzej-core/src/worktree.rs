//! Branch-name generation, base-branch resolution, and worktree add/remove.

use crate::config::{Config, NameScheme, WorktreeMode};
use crate::msg;
use crate::repo;
use crate::util;
use std::path::{Path, PathBuf};
use std::process::Command;

const ADJ: &[&str] = &[
    "brisk", "calm", "clever", "bold", "swift", "quiet", "keen", "lucky", "nimble", "warm",
    "vivid", "amber", "cosmic", "dusty", "eager", "fancy", "gentle", "hardy", "ideal", "jolly",
    "merry", "noble", "proud",
];
const NOUN: &[&str] = &[
    "otter", "falcon", "maple", "cedar", "comet", "harbor", "meadow", "pebble", "willow", "ember",
    "lark", "quartz", "raven", "cobalt", "finch", "grove", "heron", "lotus", "marlin", "onyx",
    "pine", "reef", "sage",
];

fn branch_exists(root: &Path, branch: &str) -> bool {
    util::git_ok(
        root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

fn worktree_uses_branch(root: &Path, branch: &str) -> bool {
    util::git_out(root, &["worktree", "list", "--porcelain"])
        .map(|s| {
            s.lines()
                .any(|l| l == format!("branch refs/heads/{branch}"))
        })
        .unwrap_or(false)
}

/// Generate a collision-free branch name. `human` is an optional friendly name.
pub fn branch_name(root: &Path, human: Option<&str>, cfg: &Config) -> String {
    let prefix = &cfg.branch_prefix;
    let base = if let Some(h) = human {
        format!("{prefix}{}", util::slugify(h))
    } else if cfg.name_scheme == NameScheme::Numbered {
        format!("{prefix}pane")
    } else {
        let pane: u64 = std::env::var("ZELLIJ_PANE_ID")
            .ok()
            .and_then(|v| {
                v.chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .ok()
            })
            .unwrap_or(0);
        let seed = pane.wrapping_add(util::now() as u64);
        let adj = ADJ[(seed % ADJ.len() as u64) as usize];
        let noun = NOUN[((seed / 7 + 1) % NOUN.len() as u64) as usize];
        format!("{prefix}{adj}-{noun}")
    };

    let mut candidate = base.clone();
    let mut n = 0;
    while branch_exists(root, &candidate) || worktree_uses_branch(root, &candidate) {
        n += 1;
        candidate = format!("{base}-{n}");
    }
    candidate
}

/// Best-effort default branch: origin/HEAD, else main, else master, else HEAD.
pub fn default_branch(root: &Path) -> String {
    if let Some(r) = util::git_out(
        root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        return r.strip_prefix("origin/").unwrap_or(&r).to_string();
    }
    for b in ["main", "master"] {
        if branch_exists(root, b) {
            return b.to_string();
        }
    }
    "HEAD".to_string()
}

/// Resolve the base ref to branch a new worktree off (honours `base_branch`).
pub fn resolve_base(root: &Path, cfg: &Config) -> String {
    if cfg.base_branch != "auto" {
        return cfg.base_branch.clone();
    }
    if repo::is_bare(root) {
        return default_branch(root);
    }
    if let Some(head) = util::git_out(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        return head;
    }
    let def = default_branch(root);
    msg::warn(&format!(
        "main worktree is on a detached HEAD; basing new worktree off '{def}'"
    ));
    def
}

/// Compute the worktree directory path for a branch.
pub fn worktree_path(root: &Path, branch: &str, cfg: &Config) -> PathBuf {
    let slug = util::slugify(branch);
    if cfg.worktree_mode == WorktreeMode::InRepo {
        root.join(".worktrees").join(slug)
    } else {
        Path::new(&cfg.worktrees_dir)
            .join(repo::repo_name(root))
            .join(slug)
    }
}

/// Create a worktree. Returns false on failure (caller decides how to recover)
/// rather than killing the pane.
pub fn add(root: &Path, branch: &str, base: &str, path: &Path, cfg: &Config) -> bool {
    if cfg.worktree_mode == WorktreeMode::InRepo {
        // Keep .worktrees out of git locally without touching tracked .gitignore.
        let excl = root.join(".git/info/exclude");
        if let Ok(contents) = std::fs::read_to_string(&excl) {
            if !contents.lines().any(|l| l == ".worktrees/") {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&excl) {
                    let _ = writeln!(f, ".worktrees/");
                }
            }
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "add", "-b", branch])
        .arg(path)
        .arg(base)
        .output();
    match out {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            msg::warn(&format!(
                "git worktree add failed (branch={branch} base={base}): {}",
                stderr.trim()
            ));
            false
        }
        Err(e) => {
            msg::warn(&format!("could not run git worktree add: {e}"));
            false
        }
    }
}

/// Remove a worktree and optionally delete its branch.
pub fn remove(root: &Path, path: &Path, branch: &str, delete_branch: bool) {
    let removed = util::git_ok(root, &["worktree", "remove", &path.to_string_lossy()])
        || util::git_ok(
            root,
            &["worktree", "remove", "--force", &path.to_string_lossy()],
        );
    if !removed {
        msg::warn(&format!(
            "could not remove worktree at {} (uncommitted changes?)",
            path.display()
        ));
    }
    if delete_branch && !branch.is_empty() && !util::git_ok(root, &["branch", "-D", branch]) {
        msg::warn(&format!("could not delete branch {branch}"));
    }
}
