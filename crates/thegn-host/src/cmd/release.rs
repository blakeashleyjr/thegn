//! `thegn release prepare` — the release bump as a verb.
//!
//! A thin shell over [`thegn_core::release_bump`]: it reads `Cargo.toml`,
//! `Cargo.lock`, `README.md` and `CHANGELOG.md` in the current worktree, applies
//! the pure rewrites, and writes the four files back. It never commits, tags or
//! pushes — the caller commits the result.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use thegn_core::outln;
use thegn_core::release_bump::{
    self, BumpKind, BumpPlan, bump_cargo_lock, bump_cargo_toml, bump_changelog, bump_readme,
    workspace_version,
};
use thegn_core::util::{git_ok, git_out, git_out_allow_empty};

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bump {
    Alpha,
    Beta,
    Rc,
    Patch,
}

impl From<Bump> for BumpKind {
    fn from(b: Bump) -> Self {
        match b {
            Bump::Alpha => BumpKind::Alpha,
            Bump::Beta => BumpKind::Beta,
            Bump::Rc => BumpKind::Rc,
            Bump::Patch => BumpKind::Patch,
        }
    }
}

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Bump the workspace version and the files that mention it (no commit).
    ///
    /// Rewrites `Cargo.toml`, `Cargo.lock`, `README.md` and `CHANGELOG.md` in
    /// the current worktree, which must be clean. Does not commit, tag or push.
    Prepare {
        /// Which release step to take.
        #[arg(long, value_enum)]
        bump: Bump,
        /// Ref to cut the release branch from (default: HEAD).
        #[arg(long, default_value = "HEAD")]
        from: String,
        /// Branch to work on (default: `tg/release-<new version>`). Created
        /// from `--from` when it is not the current branch.
        #[arg(long)]
        branch: Option<String>,
        /// Changelog date, `YYYY-MM-DD` (default: today).
        #[arg(long)]
        date: Option<String>,
        /// Take `Cargo.lock` from another worktree (a directory, or a lock
        /// file path) before bumping.
        #[arg(long)]
        lock_from: Option<PathBuf>,
        /// Show what would change without touching anything.
        #[arg(long)]
        dry_run: bool,
        /// Emit one JSON object instead of the human summary.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::Prepare {
            bump,
            from,
            branch,
            date,
            lock_from,
            dry_run,
            json,
        } => prepare(PrepareOpts {
            kind: bump.into(),
            from,
            branch,
            date,
            lock_from,
            dry_run,
            json,
        }),
    }
}

struct PrepareOpts {
    kind: BumpKind,
    from: String,
    branch: Option<String>,
    date: Option<String>,
    lock_from: Option<PathBuf>,
    dry_run: bool,
    json: bool,
}

const FILES: [&str; 4] = ["Cargo.toml", "Cargo.lock", "README.md", "CHANGELOG.md"];

fn valid_date(s: &str) -> bool {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

fn read(root: &Path, name: &str) -> Result<String> {
    std::fs::read_to_string(root.join(name)).with_context(|| format!("reading {name}"))
}

fn lock_path(p: &Path) -> PathBuf {
    if p.is_dir() {
        p.join("Cargo.lock")
    } else {
        p.to_path_buf()
    }
}

fn prepare(o: PrepareOpts) -> Result<()> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let date = match o.date {
        Some(d) if valid_date(&d) => d,
        Some(d) => bail!("--date `{d}` is not YYYY-MM-DD"),
        None => chrono::Local::now().format("%Y-%m-%d").to_string(),
    };
    let Some(status) = git_out_allow_empty(&cwd, &["status", "--porcelain"]) else {
        bail!("`git status` failed: is this a git worktree?");
    };
    if !status.is_empty() {
        bail!("the worktree is not clean; commit or stash first");
    }
    let current = git_out(&cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
        .context("could not read the current branch")?;
    if o.dry_run && o.from != "HEAD" {
        bail!("--dry-run reads the current checkout, so it requires --from HEAD");
    }

    // A branch switch changes what the files say, so the new version (and so
    // the default branch name) is computed from the ref's own Cargo.toml.
    let will_switch_from = o.from != "HEAD";
    let toml_at_ref = if will_switch_from {
        let spec = format!("{}:Cargo.toml", o.from);
        // `git show` output is trimmed by the helper; only the version line is
        // needed here, so that is harmless.
        git_out(&cwd, &["show", &spec])
            .with_context(|| format!("reading Cargo.toml at `{}`", o.from))?
    } else {
        read(&cwd, "Cargo.toml")?
    };
    let old = workspace_version(&toml_at_ref)?;
    let new = old.next(o.kind)?;
    let target = o.branch.unwrap_or_else(|| format!("tg/release-{new}"));
    let switching = target != current;
    if switching && !o.dry_run && !git_ok(&cwd, &["switch", "-c", &target, &o.from]) {
        bail!("`git switch -c {target} {}` failed", o.from);
    }

    let plan = BumpPlan {
        old: old.clone(),
        new: new.clone(),
        today: date.clone(),
    };
    let lock_text = match &o.lock_from {
        Some(p) => std::fs::read_to_string(lock_path(p))
            .with_context(|| format!("reading Cargo.lock from {}", p.display()))?,
        None => read(&cwd, "Cargo.lock")?,
    };
    let outputs = [
        bump_cargo_toml(&read(&cwd, "Cargo.toml")?, &plan)?,
        bump_cargo_lock(&lock_text, &plan)?,
        bump_readme(&read(&cwd, "README.md")?, &plan)?,
        bump_changelog(&read(&cwd, "CHANGELOG.md")?, &old, &new, &date)?,
    ];
    let mut changed = Vec::new();
    for (name, text) in FILES.iter().zip(outputs.iter()) {
        if read(&cwd, name)? != *text {
            changed.push(*name);
            if !o.dry_run {
                std::fs::write(cwd.join(name), text).with_context(|| format!("writing {name}"))?;
            }
        }
    }
    report(&old, &new, &target, &changed, o.dry_run, o.json);
    Ok(())
}

fn report(
    old: &release_bump::Version,
    new: &release_bump::Version,
    branch: &str,
    changed: &[&str],
    dry_run: bool,
    json: bool,
) {
    if json {
        let v = serde_json::json!({
            "old": old.to_string(),
            "new": new.to_string(),
            "branch": branch,
            "files": changed,
            "dry_run": dry_run,
            "committed": false,
        });
        outln!("{v}");
        return;
    }
    let verb = if dry_run { "would change" } else { "changed" };
    outln!("release prepare: {old} -> {new} (branch {branch})");
    for f in changed {
        outln!("  {verb}: {f}");
    }
    if dry_run {
        outln!("dry run: nothing was written");
    } else {
        outln!("nothing was committed, tagged or pushed; review and commit the result");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_maps_to_core_kind() {
        assert_eq!(BumpKind::from(Bump::Alpha), BumpKind::Alpha);
        assert_eq!(BumpKind::from(Bump::Beta), BumpKind::Beta);
        assert_eq!(BumpKind::from(Bump::Rc), BumpKind::Rc);
        assert_eq!(BumpKind::from(Bump::Patch), BumpKind::Patch);
    }

    #[test]
    fn date_validation() {
        assert!(valid_date("2026-11-12"));
        assert!(!valid_date("2026-13-01"));
        assert!(!valid_date("tomorrow"));
    }

    #[test]
    fn lock_path_accepts_dir_or_file() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(lock_path(t.path()), t.path().join("Cargo.lock"));
        let f = t.path().join("x.lock");
        assert_eq!(lock_path(&f), f);
    }

    #[test]
    fn read_reports_missing_file() {
        let t = tempfile::tempdir().unwrap();
        let err = read(t.path(), "Cargo.toml").unwrap_err();
        assert!(err.to_string().contains("Cargo.toml"));
    }
}
