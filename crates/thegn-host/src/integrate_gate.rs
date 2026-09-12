//! Verified local gate workspace preparation (THE-597).
//!
//! No implicit checkout repair or global Git pruning. A refused/stale gate is
//! retained for explicit operator inspection. Pins/rechecks are not a lease
//! against arbitrary concurrent same-UID filesystem mutation.

use super::{GateVerdict, gate_base, tail};
use crate::platform::gate_path::{Directory, Lock, Regular, read_regular};
use anyhow::{Context, Result, ensure};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use thegn_core::config::MergeQueueConfig;
use thegn_core::{gate, util};

#[cfg(test)]
#[path = "integrate_gate_tests.rs"]
mod tests;

// Unknown child wait ownership poisons all further attempts in this process,
// including throwaway workspaces. Already admitted concurrent work is not a
// global transaction and is not cancelled by this flag.
static MATERIALIZATION_POISONED: AtomicBool = AtomicBool::new(false);
type WaitChild = fn(&mut std::process::Child) -> std::io::Result<std::process::ExitStatus>;

#[expect(
    clippy::disallowed_methods,
    reason = "gate preparation runs on its background worker; retain writing lease until wait completes"
)]
fn wait_materialization_child(
    child: &mut std::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    child.wait()
}

fn materialization_available(poisoned: &AtomicBool) -> Result<()> {
    ensure!(
        !poisoned.load(Ordering::Acquire),
        "gate materialization wait ownership unknown; restart and inspect retained state"
    );
    Ok(())
}

struct FreshIndex {
    parent: Directory,
    path: PathBuf,
    file: Option<Regular>,
}

impl FreshIndex {
    fn new(parent: &Directory) -> Result<Self> {
        parent.verify()?;
        let temporary = tempfile::Builder::new()
            .prefix("materialize-")
            .tempdir_in(parent.path())?;
        let directory = Directory::open(temporary.path(), Some(temporary.path()))?;
        let path = directory.path().join("index");
        ensure!(
            !path.try_exists()?,
            "private materialization index unexpectedly exists"
        );
        // Git may create index/lock files. Never recursively remove uncertain
        // state on an error, especially when a child's wait state is unknown.
        let path = temporary.keep().join("index");
        Ok(Self {
            parent: directory,
            path,
            file: None,
        })
    }

    fn verify(&self) -> Result<()> {
        self.parent.verify()?;
        if let Some(file) = &self.file {
            file.verify()?;
        }
        Ok(())
    }

    fn cleanup(self) -> Result<()> {
        self.verify()?;
        self.file
            .context("private materialization index unavailable")?
            .remove_verified()?;
        self.parent.verify()?;
        std::fs::remove_dir(self.parent.path())
            .context("private materialization directory is not empty; retained")?;
        Ok(())
    }
}

struct Checkout {
    worktree: Directory,
    admin: Directory,
    gitfile: String,
    backlink: String,
    commondir: String,
}

fn path_line(value: &str) -> Result<&str> {
    let line = value.strip_suffix('\n').unwrap_or(value);
    ensure!(
        !line.is_empty() && !line.contains(['\n', '\r', '\0']),
        "unsupported Git identity path"
    );
    Ok(line)
}

fn resolve_path(base: &Path, value: &str) -> Result<PathBuf> {
    // Parse the supplied value BEFORE joining it to the physically pinned base.
    // Collapsing a supplied `normal/..` would skip a possible symlink that Git
    // itself follows, proving a different directory from the one Git will use.
    let value = path_line(value)?;
    let absolute = Path::new(value).is_absolute();
    let raw = if absolute {
        value
            .strip_prefix('/')
            .context("unsupported absolute Git identity path")?
    } else {
        value
    };
    ensure!(
        !raw.split('/').any(|part| part.is_empty() || part == "."),
        "noncanonical Git identity path refused"
    );
    let mut result = if absolute {
        PathBuf::from("/")
    } else {
        base.to_path_buf()
    };
    let mut supplied_normal = false;
    for part in Path::new(raw).components() {
        match part {
            Component::Normal(_) => {
                supplied_normal = true;
                result.push(part.as_os_str());
            }
            Component::ParentDir => {
                ensure!(
                    !absolute && !supplied_normal,
                    "parent traversal after a Git path component is refused"
                );
                ensure!(result.pop(), "invalid Git identity parent");
            }
            _ => anyhow::bail!("unsupported Git identity path"),
        }
    }
    ensure!(result.is_absolute(), "Git identity path is not absolute");
    Ok(result)
}

fn gitfile_path(worktree: &Path, contents: &str) -> Result<PathBuf> {
    let value = contents
        .strip_prefix("gitdir: ")
        .context("not a linked worktree gitfile")?;
    ensure!(
        Path::new(path_line(value)?).is_absolute(),
        "gitfile path must be absolute and canonical"
    );
    resolve_path(worktree, value)
}

struct Repository {
    root: Directory,
    admin: Directory,
    common: Directory,
    gitfile: Option<String>,
    commondir: Option<String>,
}

impl Repository {
    fn capture(repo: &Path) -> Result<Self> {
        let root = Directory::open(repo, None)?;
        let path = repo.join(".git");
        let meta =
            std::fs::symlink_metadata(&path).context("repository Git identity unavailable")?;
        let (admin, gitfile) = if meta.is_dir() {
            (Directory::open(&path, None)?, None)
        } else {
            let text = read_regular(&path)?;
            (
                Directory::open(&gitfile_path(repo, &text)?, None)?,
                Some(text),
            )
        };
        let path = admin.path().join("commondir");
        let commondir = optional_identity(&path)?;
        let common = Directory::open(
            &match &commondir {
                Some(text) => resolve_path(admin.path(), text)?,
                None => admin.path().to_owned(),
            },
            None,
        )?;
        let this = Self {
            root,
            admin,
            common,
            gitfile,
            commondir,
        };
        this.verify()?;
        Ok(this)
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn verify(&self) -> Result<()> {
        self.root.verify()?;
        self.admin.verify()?;
        self.common.verify()?;
        if let Some(text) = &self.gitfile {
            ensure!(
                read_regular(&self.root.path().join(".git"))? == *text,
                "root repository gitfile association changed"
            );
        } else {
            ensure!(
                std::fs::symlink_metadata(self.root.path().join(".git"))?.is_dir(),
                "root repository Git directory changed"
            );
        }
        ensure!(
            optional_identity(&self.admin.path().join("commondir"))? == self.commondir,
            "root repository common-directory association changed"
        );
        Ok(())
    }
}

fn optional_identity(path: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(Some(read_regular(path)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn detached_head(admin: &Path) -> Result<String> {
    let head = read_regular(&admin.join("HEAD"))?;
    let head = path_line(&head)?;
    ensure!(
        matches!(head.len(), 40 | 64) && head.bytes().all(|b| b.is_ascii_hexdigit()),
        "gate checkout is not detached at a complete object ID"
    );
    Ok(head.to_owned())
}

/// Only fixed, read-only probes use the shared cleanup/gate capture budget.
/// Do not put configured setup/gate commands through this policy wrapper.
fn probe(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = gate_git(root);
    command
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
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
    // Config stderr can contain configuration values. Do not publish those as
    // diagnostics; an unreadable/oversized/timed-out probe is always a hold.
    crate::bounded_git_probe::capture(command, None, "gate admission").map_err(|_| {
        anyhow::anyhow!("gate read-only Git probe unavailable or exceeded safety bound")
    })
}

/// A pinned OID must mean its original object, not a mutable replacement ref.
/// Keep this consistent for probes, preparation and fresh-index materialization.
fn gate_git(root: &Path) -> std::process::Command {
    let mut command = util::git_cmd(root);
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
    command
}

#[expect(
    clippy::disallowed_methods,
    reason = "existing gate preparation runs off-loop; captured-output/deadline work remains THE-601"
)]
fn gate_git_ok(root: &Path, args: &[&str]) -> bool {
    gate_git(root)
        .args(args)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn admit_config(bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    ensure!(
        bytes.is_empty() || bytes.ends_with(&[0]),
        "incomplete Git configuration probe"
    );
    for record in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
        let (key, value) = match record.iter().position(|byte| *byte == b'\n') {
            Some(at) => (&record[..at], Some(&record[at + 1..])),
            None => (record, None), // a valueless boolean means true
        };
        ensure!(!key.is_empty(), "invalid Git configuration probe");
        let key = key.to_ascii_lowercase();
        if matches!(
            key.as_slice(),
            b"core.sparsecheckout" | b"core.sparsecheckoutcone" | b"index.sparse"
        ) {
            // Conservatively accept only explicit familiar false spellings.
            // Unknown/noncanonical values are not repaired or normalized.
            let false_value = value.is_some_and(|value| {
                matches!(
                    value.to_ascii_lowercase().as_slice(),
                    b"" | b"false" | b"no" | b"off" | b"0"
                )
            });
            ensure!(
                false_value,
                "sparse Git configuration is unsupported for gates"
            );
        }
        // Gate materialization preserves operator-authorized Git filters, like
        // configured setup/hooks. Cleanup's stricter no-filter policy is separate.
    }
    Ok(())
}

fn admit_index(bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    ensure!(
        bytes.is_empty() || bytes.ends_with(&[0]),
        "incomplete Git index probe"
    );
    for record in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
        // H is an ordinary cached entry. S and lowercase variants hide
        // skip-worktree/assume-unchanged state; unmerged/unknown tags also hold.
        ensure!(
            record.starts_with(b"H ") && record.len() > 2,
            "hidden, unmerged or unsupported Git index state is refused for gates"
        );
    }
    Ok(())
}

fn admit_materialization(root: &Path, index: bool) -> Result<()> {
    admit_config(&probe(root, &["config", "--null", "--includes", "--list"])?)?;
    if index {
        admit_index(&probe(root, &["ls-files", "--cached", "-v", "-z"])?)?;
    }
    Ok(())
}

impl Checkout {
    fn capture(worktree: &Path, common: &Path) -> Result<Self> {
        let worktree = Directory::open(worktree, None)?;
        let gitfile = read_regular(&worktree.path().join(".git"))?;
        let admin_path = gitfile_path(worktree.path(), &gitfile)?;
        ensure!(
            admin_path.parent() == Some(common.join("worktrees").as_path()),
            "gate checkout belongs to a different repository"
        );
        let admin = Directory::open(&admin_path, None)?;
        let backlink = read_regular(&admin_path.join("gitdir"))?;
        ensure!(
            Path::new(path_line(&backlink)?) == worktree.path().join(".git"),
            "gate registration does not point to this checkout"
        );
        let commondir = read_regular(&admin_path.join("commondir"))?;
        ensure!(
            resolve_path(&admin_path, &commondir)? == common,
            "gate common directory changed"
        );
        detached_head(&admin_path)?;
        Ok(Self {
            worktree,
            admin,
            gitfile,
            backlink,
            commondir,
        })
    }

    fn verify(&self, expected_oid: Option<&str>) -> Result<()> {
        self.worktree.verify()?;
        self.admin.verify()?;
        ensure!(
            read_regular(&self.worktree.path().join(".git"))? == self.gitfile
                && read_regular(&self.admin.path().join("gitdir"))? == self.backlink
                && read_regular(&self.admin.path().join("commondir"))? == self.commondir,
            "gate linked registration changed"
        );
        let head = detached_head(self.admin.path())?;
        ensure!(
            expected_oid.is_none_or(|oid| oid == head),
            "gate HEAD no longer matches the pinned commit"
        );
        Ok(())
    }
}

struct Workspace {
    repo: Repository,
    common: Directory,
    parent: Directory,
    lock: Option<Lock>,
    checkout: Checkout,
    oid: String,
    target: Option<PathBuf>,
    temporary_parent: Option<PathBuf>,
}

impl Workspace {
    fn prepare(repo_root: &Path, oid: &str, config: &MergeQueueConfig) -> Result<Self> {
        Self::prepare_with(
            repo_root,
            oid,
            config,
            &MATERIALIZATION_POISONED,
            wait_materialization_child,
        )
    }

    fn prepare_with(
        repo_root: &Path,
        oid: &str,
        config: &MergeQueueConfig,
        poisoned: &AtomicBool,
        wait: WaitChild,
    ) -> Result<Self> {
        materialization_available(poisoned)?;
        let canonical_repo =
            std::fs::canonicalize(repo_root).context("gate repository unavailable")?;
        let repo = Repository::capture(&canonical_repo)?;
        let common = Directory::open(repo.common.path(), None)?;
        admit_materialization(repo.path(), false)?;
        repo.verify()?;
        ensure!(
            matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit()),
            "gate needs an exact commit object ID"
        );
        let resolved = String::from_utf8(probe(
            repo.path(),
            &[
                "-c",
                "core.fsmonitor=false",
                "rev-parse",
                "--verify",
                &format!("{oid}^{{commit}}"),
            ],
        )?)
        .context("requested gate commit could not be resolved")?;
        ensure!(
            resolved.trim() == oid,
            "gate object is not the requested commit"
        );

        let (parent, lock, temporary_parent, target) = if config.gate_reuse_worktree {
            let state = util::xdg_state_home();
            let base = gate_base(repo.path());
            let parent =
                Directory::open(&base, Some(&state)).context("gate state directory unavailable")?;
            let lock = Lock::acquire(&base.join("wt.lock")).context("gate lock unavailable")?;
            let target = if config.gate_target_dir.is_empty() {
                base.join("target")
            } else {
                PathBuf::from(&config.gate_target_dir)
            };
            (parent, Some(lock), None, Some(target))
        } else {
            // Once Git may populate the child, do not let TempDir::drop perform
            // recursive cleanup after a failed/replaced identity check.
            let temporary = tempfile::Builder::new()
                .prefix("thegn-gate-")
                .tempdir_in(std::fs::canonicalize(std::env::temp_dir())?)?;
            let parent = Directory::open(temporary.path(), None)?;
            let path = temporary.keep();
            let target = (!config.gate_target_dir.is_empty())
                .then(|| PathBuf::from(&config.gate_target_dir));
            (parent, None, Some(path), target)
        };
        let wt = parent.path().join("wt");
        parent.verify()?;
        if let Some(lock) = &lock {
            lock.verify()?;
        }
        repo.verify()?;
        common.verify()?;
        match std::fs::symlink_metadata(&wt) {
            Ok(_) => {
                ensure!(
                    config.gate_reuse_worktree,
                    "new gate path unexpectedly exists"
                );
                let previous = Checkout::capture(&wt, common.path())
                    .context("existing gate identity refused; inspect it explicitly")?;
                previous.verify(None)?;
                admit_materialization(&wt, true)?;
                previous.verify(None)?;
                repo.verify()?;
                parent.verify()?;
                if let Some(lock) = &lock {
                    lock.verify()?;
                }
                ensure!(
                    gate_git_ok(
                        &wt,
                        &[
                            "-c",
                            "core.fsmonitor=false",
                            "checkout",
                            "--detach",
                            "--force",
                            oid
                        ]
                    ),
                    "gate checkout failed; existing path retained without repair"
                );
                previous.verify(Some(oid))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // No --force and no global prune: an orphaned registration holds
                // for explicit repair rather than deleting unrelated stale state.
                ensure!(
                    gate_git_ok(
                        repo.path(),
                        &[
                            "-c",
                            "core.fsmonitor=false",
                            "worktree",
                            "add",
                            "--detach",
                            wt.to_str().context("gate path is not UTF-8")?,
                            oid
                        ]
                    ),
                    "gate creation failed; stale registration or partial state retained"
                );
            }
            Err(error) => return Err(error.into()),
        }
        let checkout = Checkout::capture(&wt, common.path())?;
        let workspace = Self {
            repo,
            common,
            parent,
            lock,
            checkout,
            oid: oid.into(),
            target,
            temporary_parent,
        };
        workspace.verify()?;
        workspace.materialize(poisoned, wait)
    }

    /// Fresh index entries have no inherited stat-cache trust. Blocking waits
    /// deliberately retain the workspace lease; a timed-out read-only probe
    /// runner cannot own a writer's lease. No execution deadline is claimed.
    fn materialize(self, poisoned: &AtomicBool, wait: WaitChild) -> Result<Self> {
        use std::process::Stdio;
        materialization_available(poisoned)?;
        self.verify()?;
        let mut index = FreshIndex::new(&self.parent)?;
        let oid = self.oid.clone();
        let operations: [&[&str]; 2] = [
            &["read-tree", &oid],
            &["checkout-index", "--all", "--force"],
        ];
        for args in operations {
            materialization_available(poisoned)?;
            self.verify()?;
            index.verify()?;
            let mut command = gate_git(self.checkout.worktree.path());
            command
                .env("GIT_INDEX_FILE", &index.path)
                .env("GIT_TERMINAL_PROMPT", "0")
                .args([
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "core.trustctime=true",
                    "-c",
                    "core.checkstat=default",
                ])
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = command.spawn().map_err(|_| {
                anyhow::anyhow!(
                    "gate materialization command could not start; private state retained"
                )
            })?;
            match wait(&mut child) {
                Ok(status) => ensure!(
                    status.success(),
                    "gate materialization command failed; private state retained"
                ),
                Err(_) => {
                    // Retain the *entire* writing lease, not just the index.
                    // Set poison first so future reuse AND throwaway attempts
                    // cannot accumulate children with unknown wait ownership.
                    poisoned.store(true, Ordering::Release);
                    std::mem::forget((child, self, index));
                    anyhow::bail!(
                        "gate materialization wait ownership unknown; restart and inspect retained state"
                    );
                }
            }
            if index.file.is_none() {
                index.file = Some(Regular::open_existing(&index.path)?);
            }
            self.verify()?;
            index.verify()?;
        }
        index.cleanup()?;
        Ok(self)
    }

    fn verify(&self) -> Result<()> {
        self.repo.verify()?;
        self.common.verify()?;
        self.parent.verify()?;
        if let Some(lock) = &self.lock {
            lock.verify()?;
        }
        self.checkout.verify(Some(&self.oid))?;
        admit_materialization(self.checkout.worktree.path(), true)?;
        self.repo.verify()?;
        self.parent.verify()?;
        if let Some(lock) = &self.lock {
            lock.verify()?;
        }
        self.checkout.verify(Some(&self.oid))
    }

    fn cleanup(&self) -> Result<()> {
        let Some(parent) = &self.temporary_parent else {
            return Ok(());
        };
        self.verify()?;
        ensure!(
            gate_git_ok(
                self.repo.path(),
                &[
                    "-c",
                    "core.fsmonitor=false",
                    "worktree",
                    "remove",
                    "--force",
                    self.checkout
                        .worktree
                        .path()
                        .to_str()
                        .context("gate path is not UTF-8")?
                ]
            ),
            "temporary gate removal failed; state retained"
        );
        self.parent.verify()?;
        std::fs::remove_dir(parent).context("temporary gate parent is not empty; retained")?;
        Ok(())
    }

    fn command_for(&self, command: &str) -> Result<std::process::Command> {
        self.verify()?;
        let argv = thegn_core::sandbox_cpucap::wrap_background_argv(vec![
            "sh".into(),
            "-c".into(),
            command.into(),
        ]);
        let mut command = std::process::Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(self.checkout.worktree.path());
        if let Some(target) = &self.target {
            std::fs::create_dir_all(target).context("gate artifact directory unavailable")?;
            command.env("CARGO_TARGET_DIR", target);
        }
        for key in util::GIT_ENV_VARS {
            command.env_remove(key);
        }
        command
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("THEGN_GATE", "1")
            .env("THEGN_WORKTREE", self.checkout.worktree.path())
            .env("THEGN_GATE_OID", &self.oid);
        Ok(command)
    }

    #[expect(clippy::disallowed_methods)]
    fn spawn(&self, command: &str) -> Result<std::process::Output> {
        self.command_for(command)?
            .output()
            .context("gate command could not be started")
    }
}

pub(super) fn run(repo: &Path, oid: &str, config: &MergeQueueConfig) -> Result<GateVerdict> {
    let workspace = Workspace::prepare(repo, oid, config)?;
    let verdict = (|| -> Result<GateVerdict> {
        if !config.gate_setup_command.is_empty() {
            let output = workspace.spawn(&config.gate_setup_command)?;
            workspace.verify()?;
            if !output.status.success() {
                return Ok(GateVerdict::Error {
                    reason: format!(
                        "gate_setup_command failed (exit {})",
                        output
                            .status
                            .code()
                            .map_or_else(|| "signal".into(), |code| code.to_string())
                    ),
                    log: output_tail(&output),
                });
            }
        }
        let output = workspace.spawn(&config.gate_command)?;
        workspace.verify()?;
        Ok(match gate::classify_exit(output.status.code(), false) {
            gate::GateClass::Passed => GateVerdict::Passed,
            gate::GateClass::Failed => GateVerdict::Failed {
                log: output_tail(&output),
            },
            gate::GateClass::Error => GateVerdict::Error {
                reason: gate::error_reason(output.status.code(), false).into(),
                log: output_tail(&output),
            },
        })
    })();
    // Never remove a replaced or unverified checkout, even on an error path.
    workspace.verify()?;
    workspace.cleanup()?;
    verdict
}

fn output_tail(output: &std::process::Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    tail(&text, 4000)
}
