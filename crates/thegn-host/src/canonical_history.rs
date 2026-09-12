//! Local canonical-history admission for automatic merge and cleanup (THE-606).
//!
//! This is a conservative observation, not a transaction with Git or a lease
//! against another same-UID process. It never changes refs, config or history.

use anyhow::{Context, Result, ensure};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use thegn_core::{db::Db, store::WorkspaceStore};
use thegn_core::{remote::GitLoc, util};

use crate::platform::gate_path::{Directory, Regular};

const MAX_PATH: usize = 64 * 1024;
const HISTORY_ENV: &[&str] = &["GIT_GRAFT_FILE", "GIT_SHALLOW_FILE", "GIT_REPLACE_REF_BASE"];

fn environment(get: impl Fn(&str) -> Option<OsString>) -> Result<()> {
    for key in HISTORY_ENV {
        ensure!(
            get(key).is_none(),
            "canonical history unavailable: {key} is unsupported; unset it for automatic operation"
        );
    }
    if let Some(value) = get("GIT_NO_REPLACE_OBJECTS") {
        ensure!(
            value == "1",
            "canonical history unavailable: contradictory replacement policy"
        );
    }
    // util::git_cmd already scrubs GIT_NAMESPACE, object-store redirections and
    // the other seven repository-targeting variables, including hook exports.
    Ok(())
}

fn probe(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = util::git_cmd(root);
    command
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ])
        .args(args);
    crate::bounded_git_probe::capture(command, None, "canonical history")
        .map_err(|_| anyhow::anyhow!("canonical history probe failed or exceeded safety bounds"))
}

fn git_path(root: &Path, flag: &str) -> Result<PathBuf> {
    let bytes = probe(root, &["rev-parse", "--path-format=absolute", flag])?;
    ensure!(
        bytes.len() <= MAX_PATH,
        "canonical history path exceeds safety bound"
    );
    let text = std::str::from_utf8(&bytes).context("canonical history path is not UTF-8")?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    ensure!(
        !text.is_empty() && !text.chars().any(char::is_control),
        "canonical history path is malformed"
    );
    let path = PathBuf::from(text);
    ensure!(path.is_absolute(), "canonical history path is not absolute");
    Ok(path)
}

enum GitEntry {
    Directory(Directory),
    File(Regular),
}

impl GitEntry {
    fn open(path: &Path) -> Result<Self> {
        let metadata =
            std::fs::symlink_metadata(path).context("canonical Git entry unavailable")?;
        if metadata.is_dir() {
            Ok(Self::Directory(Directory::open(path, None)?))
        } else {
            ensure!(
                metadata.is_file(),
                "canonical Git entry is not a regular file or directory"
            );
            Ok(Self::File(Regular::open_existing(path)?))
        }
    }

    fn verify(&self) -> Result<()> {
        match self {
            Self::Directory(directory) => directory.verify()?,
            Self::File(file) => file.verify()?,
        }
        Ok(())
    }
}

fn absent(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => anyhow::bail!("canonical history metadata availability is unknown"),
        Ok(_) => anyhow::bail!("canonical history requires no graft or shallow metadata"),
    }
}

fn optional_info(common: &Path) -> Result<Option<Directory>> {
    let path = common.join("info");
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => anyhow::bail!("canonical history info directory unavailable"),
        Ok(_) => {
            let directory = Directory::open(&path, None)?;
            absent(&path.join("grafts"))?;
            directory.verify()?;
            Ok(Some(directory))
        }
    }
}

/// Retained original directory/file identities; revalidation never adopts a
/// new valid repository mapping after a hook or another observed mutation.
pub(crate) struct CanonicalHistory {
    registry: Option<Registry>,
    root: Directory,
    entry: GitEntry,
    admin: Directory,
    common: Directory,
    info: Option<Directory>,
}

struct Registry {
    db: Rc<Db>,
    path: String,
    location: Option<String>,
}

fn registry_location(db: &Db, path: &str) -> Result<Option<String>> {
    let location = db
        .worktree_record(path)
        .context("canonical history registry lookup failed")?
        .map(|row| row.location);
    ensure!(
        location
            .as_deref()
            .is_none_or(|value| value.is_empty() || value == "local"),
        "canonical history registry placement is nonlocal or unsupported"
    );
    Ok(location)
}

impl CanonicalHistory {
    pub(crate) fn worktree_path(&self) -> &Path {
        self.root.path()
    }
    pub(crate) fn worktree_loc(path: &Path) -> Result<(GitLoc, Self)> {
        let db = Rc::new(Db::open().context("canonical history registry unavailable")?);
        let history = Self::registered(db, path)?;
        Ok((GitLoc::Local(path.to_path_buf()), history))
    }

    pub(crate) fn registered(db: Rc<Db>, path: &Path) -> Result<Self> {
        let text = path
            .to_str()
            .context("canonical history registry path is not UTF-8")?
            .to_owned();
        let location = registry_location(&db, &text)?;
        let mut history = Self::capture(path)?;
        history.registry = Some(Registry {
            db,
            path: text,
            location,
        });
        history.revalidate()?;
        Ok(history)
    }

    pub(crate) fn local_child(&self, loc: &GitLoc) -> Result<Self> {
        match (loc, &self.registry) {
            (GitLoc::Local(path), Some(registry)) => {
                Self::registered(Rc::clone(&registry.db), path)
            }
            _ => Self::local(loc),
        }
    }

    pub(crate) fn checked<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        self.revalidate()?;
        let outcome = operation();
        // A callback's failure must not hide an observed history mutation and
        // become branch blame. Earlier authorized side effects may remain.
        self.revalidate()?;
        outcome
    }

    pub(crate) fn local(loc: &GitLoc) -> Result<Self> {
        match loc {
            GitLoc::Local(path) => Self::capture(path),
            _ => anyhow::bail!(
                "canonical history is unsupported for remote/provider merge operations; commit and integrate locally"
            ),
        }
    }

    pub(crate) fn capture(root: &Path) -> Result<Self> {
        environment(|key| std::env::var_os(key))?;
        let root = Directory::open(root, None)
            .context("canonical history requires a verified local repository path")?;
        let entry = GitEntry::open(&root.path().join(".git"))?;
        let admin = Directory::open(&git_path(root.path(), "--absolute-git-dir")?, None)?;
        let common = Directory::open(&git_path(root.path(), "--git-common-dir")?, None)?;
        let info = optional_info(common.path())?;
        let value = Self {
            registry: None,
            root,
            entry,
            admin,
            common,
            info,
        };
        value.revalidate()?;
        Ok(value)
    }

    pub(crate) fn revalidate(&self) -> Result<()> {
        environment(|key| std::env::var_os(key))?;
        if let Some(registry) = &self.registry {
            ensure!(
                registry_location(&registry.db, &registry.path)? == registry.location,
                "canonical history registry location changed after admission"
            );
        }
        self.root.verify()?;
        self.entry.verify()?;
        self.admin.verify()?;
        self.common.verify()?;
        ensure!(
            git_path(self.root.path(), "--absolute-git-dir")? == self.admin.path(),
            "canonical Git administration mapping changed"
        );
        ensure!(
            git_path(self.root.path(), "--git-common-dir")? == self.common.path(),
            "canonical Git common-directory mapping changed"
        );
        match &self.info {
            Some(info) => {
                info.verify()?;
                absent(&info.path().join("grafts"))?;
            }
            None => absent(&self.common.path().join("info"))?,
        }
        absent(&self.common.path().join("shallow"))?;
        if self.admin.path() != self.common.path() {
            absent(&self.admin.path().join("shallow"))?;
        }
        // Git's ref backend covers packed as well as loose replacement refs.
        // One record is sufficient to refuse; no values or secrets are logged.
        let refs = probe(
            self.root.path(),
            &[
                "for-each-ref",
                "--count=1",
                "--format=%(refname)",
                "refs/replace/",
            ],
        )?;
        ensure!(
            refs.is_empty(),
            "canonical history requires no replacement refs; automatic operation held"
        );
        self.root.verify()?;
        self.entry.verify()?;
        self.admin.verify()?;
        self.common.verify()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "canonical_history_tests.rs"]
mod tests;
