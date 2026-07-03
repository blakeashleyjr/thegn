//! The registry-less **SshStream** delivery: stage a content-addressed
//! oci-archive locally, stream it to the host over the multiplexed control
//! channel with true byte-offset resume, verify its sha256, then load + tag it
//! `localhost/superzej/base:<digest12>`.
//!
//! A raw `podman save | ssh | podman load` pipe cannot resume (no offset
//! protocol); the staged `.partial` + `stat`-then-append design survives a
//! kill at any byte: the next run appends only the remainder. rsync
//! (`--partial --inplace`) replaces the append when both ends have it.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use superzej_core::image::{Digest, ImageRef, managed_tag};
use superzej_core::placement::Placement;

use super::{OciRunner, err_tail, exec_argv, sha256_file_local};

/// Remote staging directory for in-flight archives.
const REMOTE_STAGE_DIR: &str = "$HOME/.cache/superzej/oci";
/// No byte progress for this long ⇒ the transfer is stalled: kill + retryable.
const STALL_LIMIT: Duration = Duration::from_secs(120);
/// Stream chunk size.
const CHUNK: usize = 1 << 20;

/// Local content-addressed archive cache.
fn local_cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".cache"))
                .unwrap_or_else(|| PathBuf::from("/tmp"))
        })
        .join("superzej/oci")
}

/// Ensure the content-addressed oci-archive for `image@digest` exists locally;
/// returns `(path, archive_sha256, size_bytes)`. Idempotent: a cached archive
/// with a matching recorded hash is reused as-is.
fn stage_local_archive(
    image: &ImageRef,
    digest: &Digest,
) -> Result<(PathBuf, Digest, u64), String> {
    let dir = local_cache_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("archive cache: {e}"))?;
    let hex = &digest.as_str()["sha256:".len()..];
    let tar = dir.join(format!("{hex}.tar"));
    let sha_file = dir.join(format!("{hex}.tar.sha256"));
    if tar.is_file()
        && let Ok(recorded) = std::fs::read_to_string(&sha_file)
        && let Ok(d) = Digest::parse(recorded.trim())
    {
        let size = tar.metadata().map_err(|e| e.to_string())?.len();
        return Ok((tar, d, size));
    }
    let target = format!("{}@{}", image.name, digest);
    let tmp = dir.join(format!("{hex}.tar.tmp"));
    let _ = std::fs::remove_file(&tmp);
    // skopeo pulls straight from the registry into an archive; the podman
    // fallback goes via local container storage.
    let skopeo = format!(
        "skopeo copy docker://{target} oci-archive:{}",
        tmp.display()
    );
    let podman = format!(
        "podman image exists {target} || podman pull -q {target}; \
         podman save --format oci-archive -o {} {target}",
        tmp.display()
    );
    let mut staged = false;
    for cmd in [skopeo, podman] {
        match exec_argv(&["sh".into(), "-lc".into(), cmd], Duration::from_secs(1800)) {
            Ok((true, _, _)) if tmp.is_file() => {
                staged = true;
                break;
            }
            _ => {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    if !staged {
        return Err(format!(
            "no local route to stage {target} (need skopeo or podman with registry access)"
        ));
    }
    let sha = sha256_file_local(&tmp)?;
    std::fs::write(&sha_file, sha.as_str()).map_err(|e| format!("archive cache: {e}"))?;
    std::fs::rename(&tmp, &tar).map_err(|e| format!("archive cache: {e}"))?;
    let size = tar.metadata().map_err(|e| e.to_string())?.len();
    Ok((tar, sha, size))
}

/// The full SshStream delivery for `runner`'s host. Emits cumulative byte
/// progress through `progress(done, Some(total))`.
pub(super) fn ssh_stream(
    runner: &OciRunner,
    image: &ImageRef,
    digest: &Digest,
    rsync: bool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let (tar, archive_sha, total) = stage_local_archive(image, digest)?;
    let hex = &digest.as_str()["sha256:".len()..];
    let remote_partial = format!("{REMOTE_STAGE_DIR}/{hex}.tar.partial");

    // Resume offset: how much of the archive the host already holds.
    let (ok, out, err) = runner
        .exec(
            &format!(
                "mkdir -p {REMOTE_STAGE_DIR} && \
                 stat -c %s {remote_partial} 2>/dev/null || echo 0"
            ),
            Duration::from_secs(30),
        )
        .map_err(|e| format!("offset query: {e}"))?;
    if !ok {
        return Err(format!("offset query: {}", err_tail(&err)));
    }
    let mut offset: u64 = out
        .trim()
        .lines()
        .last()
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(0);
    if offset > total {
        // A stale partial from a different/older archive: restart clean.
        let _ = runner.exec(&format!("rm -f {remote_partial}"), Duration::from_secs(30));
        offset = 0;
    }
    progress(offset, Some(total));

    if offset < total {
        if rsync {
            rsync_append(runner, &tar, &remote_partial, total, progress)?;
        } else {
            cat_append(runner, &tar, &remote_partial, offset, total, progress)?;
        }
    }

    // Verify the assembled archive before anything trusts it.
    let (ok, out, err) = runner
        .exec(
            &format!("sha256sum {remote_partial} | awk '{{print $1}}'"),
            Duration::from_secs(300),
        )
        .map_err(|e| format!("verify: {e}"))?;
    if !ok {
        return Err(format!("verify: {}", err_tail(&err)));
    }
    let remote_sha = Digest::from_hex(out.trim().lines().last().unwrap_or("").trim())
        .map_err(|e| format!("verify: {e}"))?;
    if remote_sha != archive_sha {
        // Corrupt assembly: discard so the retry restarts clean.
        let _ = runner.exec(&format!("rm -f {remote_partial}"), Duration::from_secs(30));
        return Err(format!(
            "transferred archive hash mismatch (want {}, got {}) — partial discarded",
            archive_sha.short(),
            remote_sha.short()
        ));
    }

    // Load into container storage under the managed digest tag. skopeo (when
    // present) preserves identities cleanly; the podman fallback captures the
    // loaded ref and tags it.
    let tag = managed_tag(digest);
    let load = format!(
        "if command -v skopeo >/dev/null 2>&1; then \
           skopeo copy oci-archive:{remote_partial} containers-storage:{tag}; \
         else \
           ref=$(podman load -i {remote_partial} | sed -n 's/^Loaded image[^:]*: *//p' | tail -1); \
           [ -n \"$ref\" ] && podman tag \"$ref\" {tag}; \
         fi && rm -f {remote_partial}"
    );
    let (ok, _, err) = runner
        .exec(&load, Duration::from_secs(900))
        .map_err(|e| format!("load: {e}"))?;
    if !ok {
        return Err(format!("load: {}", err_tail(&err)));
    }
    Ok(())
}

/// Append the archive remainder through a remote `cat >> partial` with a
/// stall watchdog: no byte progress for [`STALL_LIMIT`] kills the child and
/// fails retryable — the next run resumes from the new offset.
fn cat_append(
    runner: &OciRunner,
    tar: &Path,
    remote_partial: &str,
    offset: u64,
    total: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let argv = runner.control_shell_argv(&format!("cat >> {remote_partial}"));
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("transfer: spawn: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("transfer: no stdin")?;

    // Stall watchdog: a side thread kills the child when the byte counter
    // hasn't moved for STALL_LIMIT (a blocked write can't time itself out).
    let sent = Arc::new(AtomicU64::new(offset));
    let done = Arc::new(AtomicU64::new(0));
    let pid = child.id();
    let watchdog = {
        let sent = Arc::clone(&sent);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut last = sent.load(Ordering::Relaxed);
            let mut last_change = Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(2));
                if done.load(Ordering::Relaxed) == 1 {
                    return;
                }
                let now = sent.load(Ordering::Relaxed);
                if now != last {
                    last = now;
                    last_change = Instant::now();
                } else if last_change.elapsed() >= STALL_LIMIT {
                    // best-effort: kill the stalled ssh child; the write side
                    // then errors out and reports the stall.
                    let _ = Command::new("kill").arg(pid.to_string()).status();
                    return;
                }
            }
        })
    };

    let stream = (|| -> Result<(), String> {
        let mut f = std::fs::File::open(tar).map_err(|e| format!("transfer: open: {e}"))?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("transfer: seek: {e}"))?;
        let mut buf = vec![0u8; CHUNK];
        let mut cursor = offset;
        loop {
            let n = f
                .read(&mut buf)
                .map_err(|e| format!("transfer: read: {e}"))?;
            if n == 0 {
                break;
            }
            stdin.write_all(&buf[..n]).map_err(|e| {
                format!("transfer stalled or channel died at {cursor}/{total} bytes: {e}")
            })?;
            cursor += n as u64;
            sent.store(cursor, Ordering::Relaxed);
            progress(cursor, Some(total));
        }
        stdin.flush().map_err(|e| format!("transfer: flush: {e}"))?;
        Ok(())
    })();
    drop(stdin); // close the pipe so the remote cat finishes
    done.store(1, Ordering::Relaxed);
    let status = child.wait().map_err(|e| format!("transfer: wait: {e}"))?;
    let _ = watchdog.join();
    stream?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut r) = child.stderr.take() {
            let _ = r.read_to_string(&mut err);
        }
        return Err(format!(
            "transfer: remote append failed: {}",
            err_tail(&err)
        ));
    }
    Ok(())
}

/// rsync `--partial --inplace` variant: sturdier resume; progress scraped from
/// `--info=progress2` lines.
fn rsync_append(
    runner: &OciRunner,
    tar: &Path,
    remote_partial: &str,
    total: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), String> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let Placement::Ssh(p) = runner.placement() else {
        return Err("rsync delivery needs an ssh reach".into());
    };
    let ssh_cmd = {
        let mut v = vec!["ssh".to_string()];
        v.extend(p.ssh_base(true));
        superzej_core::util::sh_join(&v)
    };
    // The remote path is under $HOME — rsync expands it remotely when
    // unquoted relative; strip the $HOME/ prefix into a relative path.
    let remote_rel = remote_partial.replace("$HOME/", "");
    let mut child = Command::new("rsync")
        .arg("--partial")
        .arg("--inplace")
        .arg("--info=progress2")
        .arg("-e")
        .arg(&ssh_cmd)
        .arg(tar)
        .arg(format!("{}:{}", p.host, remote_rel))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("rsync: spawn: {e}"))?;
    if let Some(out) = child.stdout.take() {
        // progress2 lines: "  123,456,789  42%  ..." (\r-separated).
        let reader = std::io::BufReader::new(out);
        for chunk in reader.split(b'\r') {
            let Ok(chunk) = chunk else { break };
            let line = String::from_utf8_lossy(&chunk);
            let bytes: String = line
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .filter(|c| c.is_ascii_digit())
                .collect();
            if let Ok(done) = bytes.parse::<u64>()
                && done > 0
            {
                progress(done, Some(total));
            }
        }
    }
    let status = child.wait().map_err(|e| format!("rsync: wait: {e}"))?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut r) = child.stderr.take() {
            let _ = r.read_to_string(&mut err);
        }
        return Err(format!("rsync: {}", err_tail(&err)));
    }
    progress(total, Some(total));
    Ok(())
}

/// Public seam kept for reuse by future volume-tarball delivery: stream any
/// staged file to the host with offset resume (the image path above).
pub fn stream_archive_over_ssh(
    runner: &OciRunner,
    local: &Path,
    remote_path: &str,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let total = local.metadata().map_err(|e| e.to_string())?.len();
    let (ok, out, _) = runner
        .exec(
            &format!(
                "mkdir -p $(dirname {remote_path}) && stat -c %s {remote_path} 2>/dev/null || echo 0"
            ),
            Duration::from_secs(30),
        )
        .map_err(|e| format!("offset query: {e}"))?;
    let offset: u64 = if ok {
        out.trim()
            .lines()
            .last()
            .unwrap_or("0")
            .trim()
            .parse()
            .unwrap_or(0)
    } else {
        0
    };
    if offset >= total {
        progress(total, Some(total));
        return Ok(());
    }
    cat_append(runner, local, remote_path, offset, total, progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_cache_dir_is_superzej_scoped() {
        let d = local_cache_dir();
        assert!(d.ends_with("superzej/oci"), "{d:?}");
    }

    /// A Local-placement runner turns `control_shell_argv` into a plain local
    /// `sh -lc`, so the streaming append machinery can be exercised end-to-end
    /// against the real filesystem — same code path a remote ssh channel runs.
    fn local_runner() -> OciRunner {
        OciRunner::new(Placement::Local)
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sz-deliver-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cat_append_streams_whole_file_with_progress() {
        let dir = tmpdir("whole");
        let src = dir.join("src.bin");
        let payload: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &payload).unwrap();
        let dst = dir.join("dst.partial");
        let mut seen = Vec::new();
        cat_append(
            &local_runner(),
            &src,
            &dst.to_string_lossy(),
            0,
            payload.len() as u64,
            &mut |done, total| seen.push((done, total)),
        )
        .expect("append succeeds");
        assert_eq!(std::fs::read(&dst).unwrap(), payload, "byte-identical");
        assert_eq!(
            seen.last().unwrap().0,
            payload.len() as u64,
            "progress reached the end"
        );
        assert_eq!(seen.last().unwrap().1, Some(payload.len() as u64));
    }

    #[test]
    fn cat_append_resumes_from_offset_appending_only_the_remainder() {
        // The CHAOS case, distilled: a killed transfer left a partial at N
        // bytes; the next run must append ONLY the remainder and assemble a
        // byte-identical file.
        let dir = tmpdir("resume");
        let src = dir.join("src.bin");
        let payload: Vec<u8> = (0..2 * 1024 * 1024u32).map(|i| (i % 239) as u8).collect();
        std::fs::write(&src, &payload).unwrap();
        let dst = dir.join("dst.partial");
        let cut = 700 * 1024usize; // "killed" mid-transfer at 700 KiB
        std::fs::write(&dst, &payload[..cut]).unwrap();

        let mut first_progress = None;
        cat_append(
            &local_runner(),
            &src,
            &dst.to_string_lossy(),
            cut as u64,
            payload.len() as u64,
            &mut |done, _| {
                first_progress.get_or_insert(done);
            },
        )
        .expect("resume succeeds");
        assert_eq!(std::fs::read(&dst).unwrap(), payload, "assembled correctly");
        assert!(
            first_progress.unwrap() > cut as u64,
            "streaming started FROM the offset (remainder only), got {first_progress:?}"
        );
    }

    #[test]
    fn stream_archive_over_ssh_is_offset_aware_and_idempotent() {
        let dir = tmpdir("stream");
        let src = dir.join("src.bin");
        let payload = vec![7u8; 512 * 1024];
        std::fs::write(&src, &payload).unwrap();
        let dst = dir.join("nested/dir/dst.tar");

        // Fresh: full copy (creates parent dirs).
        let mut ticks = 0;
        stream_archive_over_ssh(
            &local_runner(),
            &src,
            &dst.to_string_lossy(),
            &mut |_, _| {
                ticks += 1;
            },
        )
        .expect("fresh stream");
        assert_eq!(std::fs::read(&dst).unwrap(), payload);
        assert!(ticks > 0);

        // Complete file already there: a no-op that still reports completion.
        let mut done_at = None;
        stream_archive_over_ssh(
            &local_runner(),
            &src,
            &dst.to_string_lossy(),
            &mut |d, _| {
                done_at = Some(d);
            },
        )
        .expect("idempotent re-run");
        assert_eq!(done_at, Some(payload.len() as u64));
        assert_eq!(std::fs::read(&dst).unwrap(), payload, "not double-appended");
    }
}
