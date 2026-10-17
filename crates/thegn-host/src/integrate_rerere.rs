//! Queue-private rerere; folds only read, and never write, the repository's `rr-cache`.

use crate::platform::gate_path::{Directory, Lock, ScratchDirectory};
use std::io::Read;
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

pub(crate) struct CacheStatus {
    pub(crate) path: PathBuf,
    pub(crate) present: bool,
    pub(crate) entries: usize,
    pub(crate) resolved: usize,
}

/// Read the queue-private rerere cache posture without opening or creating it.
pub(crate) fn cache_status(repo_root: &Path) -> Option<CacheStatus> {
    let identity = thegn_core::repo::repository_id(repo_root).ok()?;
    let path = super::gate_base(&identity.hex()).join("rerere/git/rr-cache");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => {
            return Some(CacheStatus {
                path,
                present: false,
                entries: 0,
                resolved: 0,
            });
        }
    };
    let Ok(items) = std::fs::read_dir(&path) else {
        return Some(CacheStatus {
            path,
            present: true,
            entries: 0,
            resolved: 0,
        });
    };
    let mut entries = 0;
    let mut resolved = 0;
    for item in items.filter_map(Result::ok) {
        let name = item.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !valid_rerere_id(name) || entries == 4096 {
            continue;
        }
        let item_path = item.path();
        let Ok(item_metadata) = std::fs::symlink_metadata(&item_path) else {
            continue;
        };
        if !item_metadata.is_dir() || item_metadata.file_type().is_symlink() {
            continue;
        }
        entries += 1;
        let Ok(files) = std::fs::read_dir(item_path) else {
            continue;
        };
        let has_resolution = files.filter_map(Result::ok).any(|file| {
            let name = file.file_name();
            let Some(name) = name.to_str() else {
                return false;
            };
            let Some(suffix) = name.strip_prefix("postimage") else {
                return false;
            };
            if !valid_postimage_suffix(suffix) {
                return false;
            }
            std::fs::symlink_metadata(file.path())
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        });
        if has_resolution {
            resolved += 1;
        }
    }
    Some(CacheStatus {
        path,
        present: true,
        entries,
        resolved,
    })
}

fn valid_postimage_suffix(suffix: &str) -> bool {
    suffix.is_empty()
        || suffix.strip_prefix('.').is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn valid_rerere_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|byte| byte.is_ascii_hexdigit())
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

    pub(super) fn session<T>(
        &self,
        run_steps: impl FnOnce(&Session<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let session = Session { cache: self };
        self.clear_merge_rr()?;
        let result = run_steps(&session);
        drop(session);
        if let Err(error) = self.gc() {
            tracing::debug!(target: "thegn::integrate", %error, "queue rerere maintenance deferred");
        }
        result
    }

    fn rerere(&self, scratch: &Path, args: &[&str]) -> anyhow::Result<String> {
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
        match (&result.end, &result.leader) {
            (End::LeaderExited, Leader::Reaped(status)) if status.success() => {
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

pub(super) struct Session<'a> {
    cache: &'a Cache,
}

pub(super) fn import_shared(cache: &Cache, repo_root: &Path) {
    let Some(common_dir) = util::git_out(
        repo_root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) else {
        return;
    };
    let shared = PathBuf::from(common_dir.trim()).join("rr-cache");
    let Ok(entries) = std::fs::read_dir(shared) else {
        return;
    };
    let object_format = util::git_out(repo_root, &["rev-parse", "--show-object-format"]);
    let Some(hash_len) = object_format
        .as_deref()
        .map(str::trim)
        .and_then(|format| match format {
            "sha1" => Some(40),
            "sha256" => Some(64),
            _ => None,
        })
    else {
        return;
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let id = entry.file_name();
        let id = id.to_string_lossy();
        if id.len() != hash_len || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&path) else {
            continue;
        };
        let mut pairs = Vec::new();
        for file in files {
            let Ok(file) = file else { continue };
            let name = file.file_name().to_string_lossy().into_owned();
            let Some(suffix) = name.strip_prefix("postimage") else {
                continue;
            };
            if !valid_postimage_suffix(suffix) {
                continue;
            }
            let preimage = format!("preimage{suffix}");
            let preimage_path = path.join(&preimage);
            let postimage_path = file.path();
            if std::fs::symlink_metadata(&preimage_path).is_err() {
                continue;
            }
            let modified = std::fs::symlink_metadata(&postimage_path)
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            pairs.push((preimage, name, modified));
        }
        if !pairs.is_empty() {
            let modified = pairs
                .iter()
                .map(|pair| pair.2)
                .max()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            candidates.push((modified, id.into_owned(), path, pairs));
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    for (_, id, source, pairs) in candidates.into_iter().take(1024) {
        let destination = cache.gitdir.join("rr-cache").join(id);
        for (preimage, postimage, _) in pairs {
            let preimage_path = destination.join(&preimage);
            let postimage_path = destination.join(&postimage);
            // A postimage means the private cache has a trained resolution. A
            // preimage alone is only a deferred conflict, so shared training
            // may replace it.
            if postimage_path.exists() {
                continue;
            }
            let pair = read_import_file(&source.join(&preimage))
                .and_then(|pre| read_import_file(&source.join(&postimage)).map(|post| (pre, post)));
            let (preimage_bytes, postimage_bytes) = match pair {
                Ok(pair) => pair,
                Err(error) => {
                    tracing::debug!(target: "thegn::integrate", %error, "could not read shared rerere pair");
                    continue;
                }
            };
            let imported = std::fs::create_dir_all(&destination).and_then(|()| {
                match std::fs::remove_file(&preimage_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                write_new_file(&preimage_path, &preimage_bytes)?;
                if let Err(error) = write_new_file(&postimage_path, &postimage_bytes) {
                    if let Err(cleanup_error) = std::fs::remove_file(&preimage_path) {
                        tracing::debug!(target: "thegn::integrate", %cleanup_error, "could not remove partial shared rerere import");
                    }
                    return Err(error);
                }
                Ok(())
            });
            if let Err(error) = imported {
                tracing::debug!(target: "thegn::integrate", %error, "could not import shared rerere pair");
            }
        }
    }
}

fn read_import_file(source: &Path) -> std::io::Result<Vec<u8>> {
    let input = crate::platform::open_read_nofollow(source)?;
    if !input.metadata()?.is_file() {
        return Err(std::io::Error::other(
            "shared rerere entry is not a regular file",
        ));
    }
    let mut contents = Vec::new();
    input.take(1024 * 1024 + 1).read_to_end(&mut contents)?;
    if contents.len() > 1024 * 1024 {
        return Err(std::io::Error::other(
            "shared rerere entry exceeds import limit",
        ));
    }
    Ok(contents)
}

fn write_new_file(destination: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    output.write_all(contents)
}

impl Session<'_> {
    pub(super) fn step(&self, scratch: &Path, args: &[&str]) -> anyhow::Result<String> {
        self.cache.rerere(scratch, args)
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.cache.clear_merge_rr() {
            tracing::debug!(target: "thegn::integrate", %error, "could not clear private MERGE_RR");
        }
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
    if thegn_core::git_operand::revision(ours).is_err()
        || thegn_core::git_operand::revision(theirs).is_err()
    {
        return;
    }
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
        let refused = super::scratch_driver_overrides(&path, overrides)
            .ok_or_else(|| anyhow::anyhow!("could not inspect training merge drivers"))?;
        let mut args = Vec::new();
        for item in &refused {
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
        let unmerged = util::git_out_allow_empty(&path, &["ls-files", "-u", "-z"])
            .ok_or_else(|| anyhow::anyhow!("could not inspect training conflict paths"))?;
        let mut paths = std::collections::BTreeSet::new();
        for record in unmerged.split('\0').filter(|record| !record.is_empty()) {
            let (_, path) = record
                .split_once('\t')
                .ok_or_else(|| anyhow::anyhow!("malformed training conflict path"))?;
            paths.insert(path.to_owned());
        }
        anyhow::ensure!(!paths.is_empty(), "historical merge had no unmerged paths");
        let paths = paths.into_iter().collect::<Vec<_>>();
        cache.session(|session| {
            session.step(&path, &["rerere"])?;
            let mut checkout = vec!["--literal-pathspecs", "checkout", merge, "--"];
            checkout.extend(paths.iter().map(String::as_str));
            anyhow::ensure!(
                util::git_ok(&path, &checkout),
                "could not restore historical merge resolution"
            );
            session.step(&path, &["rerere"])?;
            Ok(())
        })
    })();
    super::remove_scratch_worktree(repo_root, scratch);
    result
}
