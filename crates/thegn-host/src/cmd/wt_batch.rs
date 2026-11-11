//! Headless multi-worktree creation for the batch form of wt new.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

use super::wt::{self, Created};
use thegn_core::config::Config;
use thegn_core::worktree::{self, BranchSet};

const MAX_BATCH_BYTES: usize = 64 * 1024;

#[derive(Debug, Serialize)]
struct Failed {
    name: String,
    error: String,
}

#[derive(Debug, Serialize)]
struct BatchOutput {
    created: Vec<Created>,
    failed: Vec<Failed>,
}

pub(crate) struct BatchOptions {
    pub(crate) repo: Option<String>,
    pub(crate) base: Option<String>,
    pub(crate) env: Option<String>,
    pub(crate) profile: Option<String>,
    pub(crate) exact_branch: bool,
    pub(crate) folder: Option<String>,
    pub(crate) json: bool,
}

/// Run one batch, validating all lane names before the first checkout.
pub(crate) fn run(
    cfg: &Config,
    file: &str,
    options: BatchOptions,
    config_dir: &Path,
) -> Result<()> {
    let BatchOptions {
        repo,
        base,
        env,
        profile,
        exact_branch,
        folder,
        json,
    } = options;
    let path = PathBuf::from(file);
    let text = read_batch(&path)?;
    let lanes = thegn_core::wt_batch::parse_batch(&text)
        .with_context(|| format!("parsing batch file {}", path.display()))?;
    let root = resolve_root(repo.as_deref())?;
    validate_names(&root, cfg, &lanes, exact_branch)
        .with_context(|| format!("validating batch file {}", path.display()))?;

    let mut output = BatchOutput {
        created: Vec::with_capacity(lanes.len()),
        failed: Vec::new(),
    };
    for lane in lanes {
        let lane_name = lane.name.clone();
        let lane_base = lane.base.or_else(|| base.clone());
        match wt::new_core(
            cfg,
            wt::NewOptions {
                name: Some(lane.name),
                repo: repo.clone(),
                base: lane_base,
                env: env.clone(),
                profile: profile.clone(),
                from_issue: None,
                exact_branch,
                folder: folder.clone(),
            },
            config_dir,
        ) {
            Ok((created, filing_error)) => {
                output.created.push(created);
                if let Some(error) = filing_error {
                    output.failed.push(Failed {
                        name: lane_name,
                        error: error.to_string(),
                    });
                }
            }
            Err(error) => output.failed.push(Failed {
                name: lane_name,
                error: format!("{error:#}"),
            }),
        }
    }

    if json {
        super::emit_json(&output)?;
    } else {
        for created in &output.created {
            thegn_core::outln!("{}", created.path);
        }
        for failed in &output.failed {
            eprintln!("error: {}: {}", failed.name, failed.error);
        }
    }
    if output.failed.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "{} batch lane(s) failed",
            output.failed.len()
        ))
    }
}

fn read_batch(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading batch file {}", path.display()))?;
    if bytes.len() > MAX_BATCH_BYTES {
        anyhow::bail!(
            "reading batch file {}: file exceeds {} KiB",
            path.display(),
            MAX_BATCH_BYTES / 1024
        );
    }
    String::from_utf8(bytes).with_context(|| {
        format!(
            "reading batch file {}: file is not valid UTF-8",
            path.display()
        )
    })
}

fn resolve_root(repo: Option<&str>) -> Result<PathBuf> {
    let start = super::resolve_worktree(repo.map(str::to_string));
    thegn_core::repo::main_worktree(&start).ok_or_else(|| {
        anyhow::Error::new(super::NotFound(format!(
            "not a git repo: {}",
            start.display()
        )))
    })
}

fn validate_names(
    root: &Path,
    cfg: &Config,
    lanes: &[thegn_core::wt_batch::BatchLane],
    exact_branch: bool,
) -> Result<()> {
    let taken = BranchSet::load_checked(root).map_err(anyhow::Error::msg)?;
    let mut planned = Vec::with_capacity(lanes.len());
    for (index, lane) in lanes.iter().enumerate() {
        let branch = if exact_branch {
            wt::exact_branch_name(Some(&lane.name), |candidate| {
                branch_taken(candidate, &taken, &planned)
            })?
        } else {
            let base = worktree::human_base(&lane.name, cfg);
            let mut candidate = base.clone();
            let mut suffix = 0;
            while branch_taken(&candidate, &taken, &planned) {
                suffix += 1;
                candidate = format!("{base}-{suffix}");
            }
            thegn_core::git_operand::branch_name(&candidate)?;
            candidate
        };
        if planned.iter().any(|candidate| candidate == &branch) {
            anyhow::bail!("line {}: branch '{branch}' is duplicated", index + 1);
        }
        planned.push(branch);
    }
    Ok(())
}

fn branch_taken(candidate: &str, taken: &BranchSet, planned: &[String]) -> bool {
    taken.taken(candidate)
        || planned.iter().any(|existing| {
            existing == candidate
                || existing.starts_with(&format!("{candidate}/"))
                || candidate.starts_with(&format!("{existing}/"))
        })
}

#[cfg(test)]
mod tests {
    use super::{BatchOptions, read_batch, run, validate_names};
    use std::path::Path;
    use thegn_core::config::Config;

    #[test]
    fn invalid_name_is_rejected_before_creation() {
        let state = crate::testenv::IsolatedState::new();
        let repo = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "test@example.invalid"],
            &["config", "user.name", "test"],
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "init",
            ],
        ] {
            assert!(thegn_core::util::git_ok(repo.path(), args));
        }
        let lanes = vec![
            thegn_core::wt_batch::BatchLane {
                name: "good".into(),
                base: None,
            },
            thegn_core::wt_batch::BatchLane {
                name: "bad..name".into(),
                base: None,
            },
        ];
        assert!(validate_names(repo.path(), &Config::default(), &lanes, true).is_err());
        drop(state);
    }

    #[test]
    fn invalid_batch_name_creates_no_worktree() {
        let state = crate::testenv::IsolatedState::new();
        let repo = tempfile::tempdir().unwrap();
        for args in [
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "init",
                "-q",
                "-b",
                "main",
            ][..],
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "init",
            ],
        ] {
            assert!(thegn_core::util::git_ok(repo.path(), args));
        }
        let lanes = repo.path().join("lanes.txt");
        std::fs::write(&lanes, "good\nbad..name\n").unwrap();
        let cfg = Config {
            worktrees_dir: repo.path().join("checkouts").display().to_string(),
            ..Config::default()
        };
        assert!(
            run(
                &cfg,
                &lanes.display().to_string(),
                BatchOptions {
                    repo: Some(repo.path().display().to_string()),
                    base: None,
                    env: None,
                    profile: None,
                    exact_branch: true,
                    folder: None,
                    json: false,
                },
                Path::new("."),
            )
            .is_err()
        );
        let worktrees =
            thegn_core::util::git_out(repo.path(), &["worktree", "list"]).unwrap_or_default();
        assert_eq!(worktrees.lines().count(), 1, "{worktrees}");
        assert!(!repo.path().join("checkouts").exists());
        drop(state);
    }

    #[test]
    fn read_batch_names_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lanes.txt");
        std::fs::write(&path, vec![b'x'; 64 * 1024 + 1]).unwrap();
        let error = read_batch(&path).unwrap_err().to_string();
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert!(error.contains("64 KiB"), "{error}");
    }
}
