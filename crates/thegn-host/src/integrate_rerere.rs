//! Queue-private rerere; merge-queue folds never access the repository's `rr-cache`.

use crate::platform::gate_path::{Directory, Lock, ScratchDirectory};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};
use thegn_core::util;
use thegn_proc::{End, Leader, Limits, Stream};

pub(super) struct Cache {
    _directory: Directory,
    _lock: Lock,
    pub(super) gitdir: PathBuf,
}

impl Cache {
    pub(super) fn open(repo_root: &Path) -> Option<Self> {
        let identity = thegn_core::repo::repository_id(repo_root).ok()?;
        let root = super::gate_base(&identity.hex());
        let cache_dir = root.join("rerere");
        let directory = Directory::open(&cache_dir, Some(&util::xdg_state_home())).ok()?;
        let lock = match Lock::acquire(&root.join("rerere.lock")) {
            Ok(lock) => lock,
            Err(error) => {
                static WARNED_LOCK: std::sync::OnceLock<()> = std::sync::OnceLock::new();
                WARNED_LOCK.get_or_init(|| {
                    tracing::warn!(target: "thegn::integrate", %error, "queue rerere cache is unavailable");
                });
                return None;
            }
        };
        let gitdir = cache_dir.join("git");
        if !gitdir.join("config").is_file() {
            let object_format = util::git_out(repo_root, &["rev-parse", "--show-object-format"])?;
            let object_format = object_format.trim();
            if !matches!(object_format, "sha1" | "sha256") {
                return None;
            }
            let gitdir_s = gitdir.to_string_lossy().to_string();
            let args = [
                "init",
                "--bare",
                "-q",
                "--template=",
                &format!("--object-format={object_format}"),
                &gitdir_s,
            ];
            if !run(repo_root, &args, Duration::from_secs(60)).is_ok_and(|out| out.is_empty()) {
                return None;
            }
        }
        if !run(
            repo_root,
            &[
                "config",
                "--file",
                gitdir.join("config").to_str()?,
                "gc.auto",
                "0",
            ],
            Duration::from_secs(60),
        )
        .is_ok()
        {
            return None;
        }
        let cache = Self {
            _directory: directory,
            _lock: lock,
            gitdir,
        };
        if cache.gc().is_err() {
            tracing::debug!(target: "thegn::integrate", "queue rerere maintenance deferred");
        }
        Some(cache)
    }

    pub(super) fn rerere(&self, scratch: &Path, args: &[&str]) -> anyhow::Result<String> {
        self.clear_merge_rr()?;
        let index = util::git_out(
            scratch,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )
        .ok_or_else(|| anyhow::anyhow!("could not locate scratch index"))?;
        let mut command = util::git_cmd(scratch);
        command
            .args([
                "-c",
                "rerere.enabled=true",
                "-c",
                "rerere.autoUpdate=false",
                "-c",
                "gc.auto=0",
            ])
            .args(args)
            .current_dir(scratch)
            .stdin(Stdio::null())
            .env("GIT_DIR", &self.gitdir)
            .env("GIT_WORK_TREE", scratch)
            .env("GIT_INDEX_FILE", index.trim())
            .env("GIT_TERMINAL_PROMPT", "0");
        let limits = Limits {
            stdout: Stream::HeadOrFail { cap: 64 * 1024 },
            stderr: Stream::Tail { cap: 4096 },
            ..Limits::silent(Duration::from_secs(60))
        };
        let result = thegn_proc::run(&mut command, &limits, &AtomicBool::new(false));
        let clear = self.clear_merge_rr();
        clear?;
        match (&result.end, &result.leader) {
            (End::LeaderExited, Leader::Reaped(status)) if status.success() => {
                self.gc()?;
                Ok(String::from_utf8_lossy(&result.stdout.bytes).into_owned())
            }
            _ => anyhow::bail!("private git rerere failed or did not reap"),
        }
    }

    fn clear_merge_rr(&self) -> anyhow::Result<()> {
        match std::fs::remove_file(self.gitdir.join("MERGE_RR")) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn gc(&self) -> anyhow::Result<()> {
        let stamp = self.gitdir.join("thegn-gc");
        let due = std::fs::metadata(&stamp)
            .and_then(|meta| meta.modified())
            .map_or(true, |modified| {
                SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default()
                    >= Duration::from_secs(86_400)
            });
        if due {
            run(
                &self.gitdir,
                &[
                    "--git-dir",
                    self.gitdir
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("invalid cache path"))?,
                    "rerere",
                    "gc",
                ],
                Duration::from_secs(60),
            )?;
            std::fs::write(stamp, b"gc\n")?;
        }
        let rr_cache = self.gitdir.join("rr-cache");
        let mut entries = std::fs::read_dir(&rr_cache)
            .map(|items| items.filter_map(Result::ok).collect::<Vec<_>>())
            .unwrap_or_default();
        if entries.len() > 1024 {
            let excess = entries.len() - 1024;
            entries.sort_by_key(|entry| {
                entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH)
            });
            for entry in entries.into_iter().take(excess) {
                Directory::open(&entry.path(), None)?.remove_tree()?;
            }
        }
        Ok(())
    }
}

fn run(dir: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
    let mut command = util::git_cmd(dir);
    command
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0");
    let limits = Limits {
        stdout: Stream::HeadOrFail { cap: 64 * 1024 },
        stderr: Stream::Tail { cap: 4096 },
        ..Limits::silent(timeout)
    };
    let result = thegn_proc::run(&mut command, &limits, &AtomicBool::new(false));
    match (&result.end, &result.leader) {
        (End::LeaderExited, Leader::Reaped(status)) if status.success() => {
            Ok(String::from_utf8_lossy(&result.stdout.bytes).into_owned())
        }
        _ => anyhow::bail!("git command failed in private rerere setup"),
    }
}

pub(super) fn train_from_history(
    cache: &Cache,
    repo_root: &Path,
    ours: &str,
    theirs: &str,
    overrides: &[String],
) {
    let Ok(history) = run(
        repo_root,
        &[
            "rev-list",
            "--merges",
            "--parents",
            "--max-count=32",
            ours,
            theirs,
        ],
        Duration::from_secs(60),
    ) else {
        return;
    };
    let memo_path = cache.gitdir.join("thegn-trained");
    let mut memo = std::fs::read_to_string(&memo_path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut replayed = 0;
    for line in history.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 || memo.iter().any(|oid| oid == fields[0]) {
            continue;
        }
        let merge = fields[0];
        let mut probe = Vec::new();
        let override_refs = overrides.iter().map(String::as_str).collect::<Vec<_>>();
        for item in &override_refs {
            probe.push("-c");
            probe.push(item);
        }
        probe.extend(["merge-tree", "--write-tree"]);
        let parent1 = format!("{merge}^1");
        let parent2 = format!("{merge}^2");
        probe.extend([parent1.as_str(), parent2.as_str()]);
        if run(repo_root, &probe, Duration::from_secs(60)).is_ok() {
            memo.push(merge.to_owned());
            continue;
        }
        if replayed >= 8 {
            break;
        }
        replayed += 1;
        if let Err(error) = train_merge(cache, repo_root, fields[1], fields[2], merge, overrides) {
            tracing::debug!(target: "thegn::integrate", %error, "historical rerere merge could not be replayed");
        }
        memo.push(merge.to_owned());
    }
    if memo.len() > 512 {
        memo.drain(..memo.len() - 512);
    }
    if let Err(error) = std::fs::write(memo_path, memo.join("\n")) {
        tracing::debug!(target: "thegn::integrate", %error, "could not persist trained rerere merge ids");
    }
}

pub(super) fn train_merge(
    cache: &Cache,
    repo_root: &Path,
    parent1: &str,
    parent2: &str,
    merge: &str,
    overrides: &[String],
) -> anyhow::Result<()> {
    let scratch = ScratchDirectory::create("tg-rerere-train-")?;
    let path = scratch.path().to_path_buf();
    let path_s = path.to_string_lossy().to_string();
    if scratch.verify().is_err()
        || !util::git_ok(
            repo_root,
            &["worktree", "add", "--detach", &path_s, parent1],
        )
    {
        super::cleanup_failed_scratch_add(repo_root, scratch);
        anyhow::bail!("could not create rerere training worktree");
    }
    let result = (|| {
        let mut args = Vec::new();
        for item in overrides {
            args.push("-c".to_owned());
            args.push(item.clone());
        }
        args.extend([
            "-c".into(),
            "rerere.enabled=false".into(),
            "merge".into(),
            "--no-commit".into(),
            "--no-ff".into(),
            parent2.to_owned(),
        ]);
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let run = super::run_driver_merge(&path, &refs, Duration::from_secs(60));
        if !matches!(run, super::DriverRun::Completed { ref status, .. } if status.code() == Some(1))
        {
            anyhow::bail!("historical merge did not reproduce a conflict");
        }
        cache.rerere(&path, &["rerere"])?;
        anyhow::ensure!(
            util::git_ok(&path, &["checkout", merge, "--", "."]),
            "could not restore historical merge resolution"
        );
        cache.rerere(&path, &["rerere"])?;
        Ok(())
    })();
    super::remove_scratch_worktree(repo_root, scratch);
    result
}
