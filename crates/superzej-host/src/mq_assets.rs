//! Bundled `/mq` merge-queue skill, embedded in the binary and auto-seeded into
//! each worktree's project skills dir (`.claude/skills/mq/SKILL.md`) so any
//! Claude-family agent a user runs inside superzej discovers the merge-queue
//! commands without hand-installing anything. Mirrors the `pi_assets` embed
//! pattern. The pi (ACP) and control-API surfaces expose the same actions
//! natively; this is the discoverability layer for plain shell-pane agents.

use std::path::Path;
use superzej_core::config::Config;

const MQ_SKILL_MD: &str = include_str!("../../../extensions/skills/mq/SKILL.md");

/// The local-ignore pattern (anchored at each worktree's top level) so the
/// seeded skill never shows up as an untracked change in `git status`.
const EXCLUDE_PAT: &str = ".claude/skills/mq/";

/// Seed the `/mq` skill into a worktree (idempotent overwrite) and locally
/// ignore it. Returns an error only on I/O failure at the write site.
pub fn seed(worktree: &Path) -> std::io::Result<()> {
    let dir = worktree.join(".claude").join("skills").join("mq");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SKILL.md"), MQ_SKILL_MD)?;
    exclude_locally(worktree);
    Ok(())
}

/// Seed every persisted **local** worktree once (best-effort). Covers worktrees
/// created before this build; newly-created ones are seeded at create time. No-op
/// when the merge queue is disabled. Kept here (not `run.rs`) to keep that
/// god-file lean.
pub fn seed_persisted_worktrees(cfg: &Config) {
    if !cfg.merge_queue.enabled {
        return;
    }
    if let Ok(db) = superzej_core::db::Db::open() {
        use superzej_core::store::WorkspaceStore;
        for wt in db.worktrees().unwrap_or_default() {
            if wt.location.is_empty() {
                let _ = seed(std::path::Path::new(&wt.worktree));
            }
        }
    }
}

/// Gated, best-effort seed: only when the merge queue is enabled. The skill is a
/// convenience, never load-bearing — failures (e.g. a read-only canonical tree)
/// must not disrupt worktree creation.
pub fn seed_if_enabled(cfg: &Config, worktree: &Path) {
    if cfg.merge_queue.enabled {
        // best-effort: discoverability aid, not a correctness requirement.
        let _ = seed(worktree);
    }
}

/// Append `EXCLUDE_PAT` to the repo's shared `.git/info/exclude` (once) so the
/// seeded skill is ignored across all worktrees. Same idiom as
/// `worktree::add_checked` uses for `.worktrees/`.
fn exclude_locally(worktree: &Path) {
    let excl = superzej_core::util::git_common_dir(worktree)
        .join("info")
        .join("exclude");
    if let Ok(contents) = std::fs::read_to_string(&excl) {
        if contents.lines().any(|l| l.trim() == EXCLUDE_PAT) {
            return;
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&excl) {
            let _ = writeln!(f, "{EXCLUDE_PAT}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sz-mq-assets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".git").join("info")).unwrap();
        // A real-ish info/exclude so the append path (which requires the file to
        // exist) runs, mirroring a git-initialised repo.
        std::fs::write(
            d.join(".git").join("info").join("exclude"),
            "# git ignores\n",
        )
        .unwrap();
        d
    }

    #[test]
    fn seed_writes_skill_and_is_idempotent() {
        let wt = scratch("idem");
        seed(&wt).unwrap();
        let skill = wt.join(".claude/skills/mq/SKILL.md");
        assert!(skill.exists());
        let body = std::fs::read_to_string(&skill).unwrap();
        assert!(body.contains("szhost merge add"));

        // Second seed: still fine, and the exclude line is not duplicated.
        seed(&wt).unwrap();
        let excl = std::fs::read_to_string(wt.join(".git/info/exclude")).unwrap();
        let hits = excl.lines().filter(|l| l.trim() == EXCLUDE_PAT).count();
        assert_eq!(hits, 1, "exclude pattern should be appended exactly once");

        let _ = std::fs::remove_dir_all(&wt);
    }
}
