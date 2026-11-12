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

struct ValidationOptions<'a> {
    base: Option<&'a str>,
    env: Option<&'a str>,
    profile: Option<&'a str>,
    folder: Option<&'a str>,
}

/// Run one batch, validating all lanes before the first checkout.
#[expect(clippy::disallowed_macros, reason = "per-lane batch errors on stderr")]
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
    let validation_options = ValidationOptions {
        base: base.as_deref(),
        env: env.as_deref(),
        profile: profile.as_deref(),
        folder: folder.as_deref(),
    };
    validate_names(&root, cfg, &lanes, exact_branch, &validation_options)
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
    options: &ValidationOptions<'_>,
) -> Result<()> {
    let first_lane = &lanes[0];
    if let Some(profile) = options.profile {
        thegn_core::sandbox_pin::parse_pin(profile)
            .map_err(|error| lane_error(first_lane, error))?;
    }
    if let Some(folder) = options.folder {
        wt::validate_folder_name(folder).map_err(|error| lane_error(first_lane, error))?;
    }
    if let Some(env) = options.env
        && env != "default"
        && !cfg.env.contains_key(env)
    {
        let mut known: Vec<&str> = cfg.env.keys().map(String::as_str).collect();
        known.sort_unstable();
        return Err(lane_error(
            first_lane,
            format!(
                "no [env.{env}] defined (known: default{}{})",
                if known.is_empty() { "" } else { ", " },
                known.join(", ")
            ),
        ));
    }

    let default_base = match options.base.filter(|base| !base.trim().is_empty()) {
        Some(base) => base.to_string(),
        None => thegn_core::worktree::resolve_base_checked(root, cfg)
            .map_err(|error| lane_error(first_lane, error))?,
    };
    let mut checked_bases = std::collections::HashSet::new();
    for lane in lanes {
        let effective_base = lane.base.as_deref().unwrap_or(&default_base);
        if checked_bases.insert(effective_base)
            && !wt::base_has_commits(root, effective_base)
                .map_err(|error| lane_error(lane, error))?
        {
            return Err(lane_error(
                lane,
                format!("'{effective_base}' has no commits yet — make an initial commit first"),
            ));
        }
    }

    let taken = BranchSet::load_checked(root).map_err(|error| lane_error(first_lane, error))?;
    let mut planned = Vec::with_capacity(lanes.len());
    for lane in lanes {
        let branch = if exact_branch {
            wt::exact_branch_name(Some(&lane.name), |candidate| {
                branch_taken(candidate, &taken, &planned)
            })
            .map_err(|error| lane_error(lane, error))?
        } else {
            let base = worktree::human_base(&lane.name, cfg);
            let mut candidate = base.clone();
            let mut suffix = 0;
            while branch_taken(&candidate, &taken, &planned) {
                suffix += 1;
                candidate = format!("{base}-{suffix}");
            }
            thegn_core::git_operand::branch_name(&candidate)
                .map_err(|error| lane_error(lane, error))?;
            candidate
        };
        if planned.iter().any(|candidate| candidate == &branch) {
            return Err(lane_error(lane, format!("branch '{branch}' is duplicated")));
        }
        planned.push(branch);
    }
    Ok(())
}

fn lane_error(
    lane: &thegn_core::wt_batch::BatchLane,
    error: impl std::fmt::Display,
) -> anyhow::Error {
    anyhow::anyhow!("line {}: {}: {error}", lane.line, lane.name)
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
    use super::{BatchOptions, ValidationOptions, read_batch, run, validate_names};
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
                line: 1,
            },
            thegn_core::wt_batch::BatchLane {
                name: "bad..name".into(),
                base: None,
                line: 2,
            },
        ];
        assert!(
            validate_names(
                repo.path(),
                &Config::default(),
                &lanes,
                true,
                &ValidationOptions {
                    base: None,
                    env: None,
                    profile: None,
                    folder: None,
                },
            )
            .is_err()
        );
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
    fn invalid_batch_base_creates_no_worktree() {
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
        std::fs::write(&lanes, "good\nlast no-such-base\n").unwrap();
        let cfg = Config {
            worktrees_dir: repo.path().join("checkouts").display().to_string(),
            ..Config::default()
        };
        let before = thegn_core::util::git_out(repo.path(), &["worktree", "list"]).unwrap();
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
        let after = thegn_core::util::git_out(repo.path(), &["worktree", "list"]).unwrap();
        assert_eq!(before, after);
        assert!(!repo.path().join("checkouts").exists());
        drop(state);
    }

    #[test]
    fn validation_errors_use_batch_source_lines() {
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
        let lanes =
            thegn_core::wt_batch::parse_batch("# comment\n\nvalid\ninvalid..name\n").unwrap();
        let error = validate_names(
            repo.path(),
            &Config::default(),
            &lanes,
            true,
            &ValidationOptions {
                base: None,
                env: None,
                profile: None,
                folder: None,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("line 4: invalid..name:"), "{error}");
        drop(state);
    }

    #[test]
    fn batch_options_are_validated_before_branch_planning() {
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
        let lanes = thegn_core::wt_batch::parse_batch("lane\n").unwrap();

        for (env, profile, folder, expected) in [
            (Some("missing"), None, None, "no [env.missing] defined"),
            (
                None,
                Some("open"),
                None,
                "open is a config-only escape hatch",
            ),
            (None, None, Some("   "), "folder name must not be empty"),
        ] {
            let error = validate_names(
                repo.path(),
                &Config::default(),
                &lanes,
                true,
                &ValidationOptions {
                    base: None,
                    env,
                    profile,
                    folder,
                },
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(expected), "{error}");
            assert!(error.starts_with("line 1: lane:"), "{error}");
        }
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
