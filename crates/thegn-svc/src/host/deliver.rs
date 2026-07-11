//! The registry-less **SshStream** delivery: stage a content-addressed
//! oci-archive locally, stream it to the host over the multiplexed control
//! channel with true byte-offset resume, verify its sha256, then load + tag it
//! `localhost/thegn/base:<digest12>`.
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

use thegn_core::image::{Digest, ImageRef, managed_tag};
use thegn_core::placement::Placement;

use thegn_core::transport_error::{ClassifiedErr, classify_exec, describe_exec_failure};

use super::{OciRunner, exec_argv, sha256_file_local};

/// Remote staging directory for in-flight archives.
const REMOTE_STAGE_DIR: &str = "$HOME/.cache/thegn/oci";
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
        .join("thegn/oci")
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
    let name_tag = image.name_tag();
    let tmp = dir.join(format!("{hex}.tar.tmp"));
    let _ = std::fs::remove_file(&tmp);
    // LOCAL container storage FIRST, by name:tag — a `just image-build`-loaded
    // image has no RepoDigest, so a `name@digest` pull would miss; staging the
    // local image directly is the fully-local, registry-free path. `podman save`
    // is the reliable rootless path; skopeo needs the store spelled out (see
    // `local_containers_storage_prefix`); docker-daemon covers a docker host.
    // Then the registry: skopeo streams straight to an archive; podman via store.
    let mut attempts: Vec<String> = Vec::new();
    attempts.push(format!(
        "podman image exists {name_tag} && podman save --format oci-archive -o {} {name_tag}",
        tmp.display()
    ));
    if let Some(cs) = super::local_containers_storage_prefix() {
        attempts.push(format!(
            "skopeo copy {cs}{name_tag} oci-archive:{}",
            tmp.display()
        ));
    }
    attempts.push(format!(
        "skopeo copy docker-daemon:{name_tag} oci-archive:{}",
        tmp.display()
    ));
    attempts.push(format!(
        "skopeo copy docker://{target} oci-archive:{}",
        tmp.display()
    ));
    attempts.push(format!(
        "podman image exists {target} || podman pull -q {target}; \
         podman save --format oci-archive -o {} {target}",
        tmp.display()
    ));
    let mut staged = false;
    for cmd in attempts {
        match exec_argv(&["sh".into(), "-lc".into(), cmd], Duration::from_secs(1800)) {
            Ok(o) if o.ok && tmp.is_file() => {
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
            "no route to stage {target} — need the image in local container \
             storage (`just image-build` / `podman pull`) or skopeo/podman with \
             registry access"
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
) -> Result<(), ClassifiedErr> {
    let (tar, archive_sha, total) = stage_local_archive(image, digest)?;
    let hex = &digest.as_str()["sha256:".len()..];
    let remote_partial = format!("{REMOTE_STAGE_DIR}/{hex}.tar.partial");

    // Resume offset: how much of the archive the host already holds. A flap
    // here must not burn a whole delivery attempt — retry the cheap query
    // inline (an unparsable answer already falls back to offset 0 below).
    let out = thegn_core::retry::with_retry(
        "offset query",
        &thegn_core::retry::StepBudget::new(
            thegn_core::retry::ReconnectPolicy::probe(),
            Duration::from_secs(120),
        ),
        &mut |_| {},
        &mut || {},
        &mut || {
            let o = runner
                .exec(
                    &format!(
                        "mkdir -p {REMOTE_STAGE_DIR} && \
                         stat -c %s {remote_partial} 2>/dev/null || echo 0"
                    ),
                    Duration::from_secs(30),
                )
                .map_err(|f| f.cerr("offset query"))?;
            if !o.ok {
                return Err(o.cerr("offset query"));
            }
            Ok(o)
        },
    )?;
    let mut offset: u64 = out
        .stdout
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
    let out = runner
        .exec(
            &format!("sha256sum {remote_partial} | awk '{{print $1}}'"),
            Duration::from_secs(300),
        )
        .map_err(|f| f.cerr("verify"))?;
    if !out.ok {
        return Err(out.cerr("verify"));
    }
    let remote_sha = Digest::from_hex(out.stdout.trim().lines().last().unwrap_or("").trim())
        .map_err(|e| format!("verify: {e}"))?;
    if remote_sha != archive_sha {
        // Corrupt assembly: discard so the retry restarts clean — retryable by
        // construction (the next attempt streams a fresh archive).
        let _ = runner.exec(&format!("rm -f {remote_partial}"), Duration::from_secs(30));
        return Err(ClassifiedErr::transient(format!(
            "transferred archive hash mismatch (want {}, got {}) — partial discarded",
            archive_sha.short(),
            remote_sha.short()
        )));
    }

    // Load into the host runtime's image storage under the managed digest tag.
    // The command is spelled for podman OR docker (storage transport + fallback
    // binary) — see `OciRunner::load_archive_cmd`.
    let tag = managed_tag(digest);
    let load = runner.load_archive_cmd(&remote_partial, &tag)?;
    let out = runner
        .exec(&load, Duration::from_secs(900))
        .map_err(|f| f.cerr("load"))?;
    if !out.ok {
        return Err(out.cerr("load"));
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
) -> Result<(), ClassifiedErr> {
    use std::process::{Command, Stdio};
    let argv = runner.control_shell_argv(&format!("cat >> {remote_partial}"));
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ClassifiedErr::permanent(format!("transfer: spawn: {e}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ClassifiedErr::permanent("transfer: no stdin"))?;

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

    let stream = (|| -> Result<(), ClassifiedErr> {
        let mut f = std::fs::File::open(tar)
            .map_err(|e| ClassifiedErr::permanent(format!("transfer: open: {e}")))?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| ClassifiedErr::permanent(format!("transfer: seek: {e}")))?;
        let mut buf = vec![0u8; CHUNK];
        let mut cursor = offset;
        loop {
            let n = f
                .read(&mut buf)
                .map_err(|e| ClassifiedErr::permanent(format!("transfer: read: {e}")))?;
            if n == 0 {
                break;
            }
            // A mid-stream write failure IS the flaky-link case (channel died,
            // stall-watchdog kill): transient — the partial resumes next try.
            stdin.write_all(&buf[..n]).map_err(|e| {
                ClassifiedErr::transient(format!(
                    "transfer stalled or channel died at {cursor}/{total} bytes: {e}"
                ))
            })?;
            cursor += n as u64;
            sent.store(cursor, Ordering::Relaxed);
            progress(cursor, Some(total));
        }
        stdin
            .flush()
            .map_err(|e| ClassifiedErr::transient(format!("transfer: flush: {e}")))?;
        Ok(())
    })();
    drop(stdin); // close the pipe so the remote cat finishes
    done.store(1, Ordering::Relaxed);
    let status = child
        .wait()
        .map_err(|e| ClassifiedErr::permanent(format!("transfer: wait: {e}")))?;
    let _ = watchdog.join();
    stream?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut r) = child.stderr.take() {
            let _ = r.read_to_string(&mut err);
        }
        return Err(ClassifiedErr {
            class: classify_exec(status.code(), false, &err),
            msg: describe_exec_failure("transfer: remote append", status.code(), false, &err),
        });
    }
    Ok(())
}

/// rsync `--partial --inplace` variant: sturdier resume; progress scraped from
/// `--info=progress2` lines.
/// rsync exit codes that mean the *transport* (not the data) failed — worth a
/// retry on a flaky link: 10 socket I/O, 12 protocol stream, 30 timeout, plus
/// ssh's own 255.
fn rsync_class(code: Option<i32>, stderr: &str) -> thegn_core::transport_error::ErrorClass {
    match code {
        Some(10) | Some(12) | Some(30) => thegn_core::transport_error::ErrorClass::Transient,
        _ => classify_exec(code, false, stderr),
    }
}

fn rsync_append(
    runner: &OciRunner,
    tar: &Path,
    remote_partial: &str,
    total: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), ClassifiedErr> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let Placement::Ssh(p) = runner.placement() else {
        return Err("rsync delivery needs an ssh reach".into());
    };
    let ssh_cmd = {
        let mut v = vec!["ssh".to_string()];
        v.extend(p.ssh_base(true));
        thegn_core::util::sh_join(&v)
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
        .map_err(|e| ClassifiedErr::permanent(format!("rsync: spawn: {e}")))?;
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
    let status = child
        .wait()
        .map_err(|e| ClassifiedErr::permanent(format!("rsync: wait: {e}")))?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut r) = child.stderr.take() {
            let _ = r.read_to_string(&mut err);
        }
        return Err(ClassifiedErr {
            class: rsync_class(status.code(), &err),
            msg: describe_exec_failure("rsync", status.code(), false, &err),
        });
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
    let out = runner
        .exec(
            &format!(
                "mkdir -p $(dirname {remote_path}) && stat -c %s {remote_path} 2>/dev/null || echo 0"
            ),
            Duration::from_secs(30),
        )
        .map_err(|f| f.msg("offset query"))?;
    let offset: u64 = if out.ok {
        out.stdout
            .trim()
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
    cat_append(runner, local, remote_path, offset, total, progress).map_err(|e| e.msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_cache_dir_is_thegn_scoped() {
        let d = local_cache_dir();
        assert!(d.ends_with("thegn/oci"), "{d:?}");
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
