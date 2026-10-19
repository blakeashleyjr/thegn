//! Pure builders/parsers for mirroring worktree state across the host↔sandbox
//! boundary. Both directions move the same artifact triple:
//!
//! - `bundle` — commits not on any remote (`git bundle create … HEAD --not --remotes`)
//! - `patch`  — uncommitted tracked changes (`git diff HEAD --binary`)
//! - `tar`    — untracked, non-ignored files
//!
//! Local→remote ("local parity", provision-time) captures on the host and
//! replays in the sandbox; remote→durable ("hibernate", reverse capture)
//! captures IN the sandbox via [`capture_script`], whose output is parsed with
//! [`parse_capture_output`] so the host can verify sizes/checksums before it
//! trusts the download. Everything here is pure string work — no I/O — so it
//! sits in core under the coverage gate.

use crate::util::sh_quote;

/// The artifact short-names, in capture/replay order. These are the `<name>`
/// in `/tmp/<stem>.<name>` on the sandbox side and the `TG-ART <name> …`
/// trailer lines a capture emits.
pub const ARTIFACT_NAMES: [&str; 3] = ["bundle", "patch", "tar"];

/// Sandbox-side path of one artifact for a given file stem.
pub fn artifact_path(stem: &str, name: &str) -> String {
    format!("/tmp/{stem}.{name}")
}

/// Sandbox-side path for an artifact in a per-operation staging directory.
pub fn staged_artifact_path(dir: &str, name: &str) -> String {
    format!("{dir}/{name}")
}

/// Create a private, exclusive sandbox staging directory.
pub fn stage_dir_script(dir: &str) -> String {
    format!("umask 077; mkdir -- {}", sh_quote(dir))
}

/// One artifact reported by a remote capture: its short name plus the size and
/// sha256 the REMOTE computed — the host re-hashes after download and refuses
/// a mismatch (integrity across the ssh/WSS hop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureArtifact {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

/// Parsed result of running [`capture_script`] in a sandbox worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureReport {
    /// `HEAD` at capture time; `None` for an unborn branch (no commits yet).
    pub head: Option<String>,
    /// `git rev-parse --abbrev-ref HEAD` — `"HEAD"` when detached.
    pub branch: String,
    /// Only the artifacts that came out non-empty.
    pub artifacts: Vec<CaptureArtifact>,
}

impl CaptureReport {
    pub fn artifact(&self, name: &str) -> Option<&CaptureArtifact> {
        self.artifacts.iter().find(|a| a.name == name)
    }
    /// Nothing to persist: clean tree, nothing unpushed, nothing untracked.
    pub fn is_empty(&self) -> bool {
        self.artifacts.is_empty()
    }
}

/// Build the sh script that captures the artifact triple INSIDE a sandbox
/// worktree into `/tmp/<stem>.*` and prints a machine-readable trailer:
///
/// ```text
/// TG-HEAD <sha|none>
/// TG-BRANCH <name>
/// TG-ART <name> <bytes> <sha256>     (one line per non-empty artifact)
/// ```
///
/// Every stage is independently guarded and non-fatal; the trailer only
/// reports artifacts that exist and are non-empty, so a partial capture is
/// visible (not silently trusted) on the host side.
pub fn capture_script(workdir: &str, stem: &str) -> String {
    let wd = sh_quote(workdir);
    let bundle = artifact_path(stem, "bundle");
    let patch = artifact_path(stem, "patch");
    let tar = artifact_path(stem, "tar");
    let list = format!("/tmp/{stem}.list");
    format!(
        "cd {wd} || exit 1; \
         rm -f {bundle} {patch} {tar} {list} 2>/dev/null; \
         git bundle create {bundle} HEAD --not --remotes >/dev/null 2>&1 || true; \
         git diff HEAD --binary > {patch} 2>/dev/null || true; \
         git ls-files --others --exclude-standard -z > {list} 2>/dev/null || true; \
         if [ -s {list} ]; then tar -C {wd} --null -T {list} -czf {tar} 2>/dev/null || true; fi; \
         rm -f {list} 2>/dev/null; \
         echo \"TG-HEAD $(git rev-parse HEAD 2>/dev/null || echo none)\"; \
         echo \"TG-BRANCH $(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)\"; \
         for n in bundle patch tar; do \
           p=/tmp/{stem}.$n; \
           if [ -s \"$p\" ]; then \
             echo \"TG-ART $n $(wc -c < \"$p\" | tr -d ' ') $(sha256sum \"$p\" | cut -d' ' -f1)\"; \
           fi; \
         done"
    )
}

/// Parse the trailer printed by [`capture_script`]. Tolerates arbitrary noise
/// before/between the `TG-*` lines (git chatter), but every `TG-ART` line must
/// be well-formed — a malformed report is an error, never a silent skip: the
/// caller is about to destroy the VM on the strength of this report.
pub fn parse_capture_output(out: &str) -> Result<CaptureReport, String> {
    let mut head = None;
    let mut branch = None;
    let mut artifacts = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("TG-HEAD ") {
            let h = rest.trim();
            head = Some((h != "none" && !h.is_empty()).then(|| h.to_string()));
        } else if let Some(rest) = line.strip_prefix("TG-BRANCH ") {
            branch = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("TG-ART ") {
            let mut parts = rest.split_whitespace();
            let (name, bytes, sha) = (parts.next(), parts.next(), parts.next());
            let (Some(name), Some(bytes), Some(sha)) = (name, bytes, sha) else {
                return Err(format!("malformed TG-ART line: {line:?}"));
            };
            if !ARTIFACT_NAMES.contains(&name) {
                return Err(format!("unknown artifact {name:?} in capture report"));
            }
            let bytes: u64 = bytes
                .parse()
                .map_err(|_| format!("bad artifact size in {line:?}"))?;
            if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("bad sha256 in {line:?}"));
            }
            artifacts.push(CaptureArtifact {
                name: name.to_string(),
                bytes,
                sha256: sha.to_ascii_lowercase(),
            });
        }
    }
    let head = head.ok_or("capture report missing TG-HEAD (script did not run to completion)")?;
    Ok(CaptureReport {
        head,
        branch: branch.unwrap_or_else(|| "HEAD".into()),
        artifacts,
    })
}

/// Build the sh script that replays a previously-uploaded artifact triple over
/// the clone at `workdir`: fetch the bundle + hard-reset to `reset_head` (when
/// a bundle was carried), apply the patch (3-way fallback), untar the
/// untracked files (`tar xf` auto-detects gzip). A failed stage exits with a
/// `TG-PARITY-ERROR:<stage>` marker and leaves artifacts in place. Artifacts
/// are removed and completion is printed only after every requested stage passes.
pub fn replay_script(
    workdir: &str,
    stem: &str,
    reset_head: Option<&str>,
    has_patch: bool,
    has_tar: bool,
    done_msg: &str,
) -> String {
    replay_script_builder(
        workdir,
        ReplayArtifacts {
            bundle: artifact_path(stem, "bundle"),
            patch: artifact_path(stem, "patch"),
            tar: artifact_path(stem, "tar"),
            dir: None,
        },
        ReplayOptions {
            reset_head,
            has_patch,
            has_tar,
            done_msg,
            validation: String::new(),
        },
    )
}

/// Build a replay that verifies all carried artifacts against their captured
/// SHA-256 manifest before any checkout mutation.
pub fn replay_script_verified(
    workdir: &str,
    dir: &str,
    manifest: &[CaptureArtifact],
    reset_head: Option<&str>,
    has_patch: bool,
    has_tar: bool,
    done_msg: &str,
) -> String {
    let mut validation = String::new();
    let mut valid_names = Vec::new();
    for artifact in manifest {
        if !ARTIFACT_NAMES.contains(&artifact.name.as_str())
            || valid_names.contains(&artifact.name.as_str())
        {
            validation.push_str("fail artifact_validation; ");
            continue;
        }
        valid_names.push(artifact.name.as_str());
        let path = sh_quote(&staged_artifact_path(dir, &artifact.name));
        validation.push_str(&format!(
            "printf '%s  %s\\n' {} {path} | sha256sum -c - >/dev/null 2>&1 || fail artifact_validation; ",
            sh_quote(&artifact.sha256)
        ));
    }
    for (name, carried) in [
        ("bundle", reset_head.is_some()),
        ("patch", has_patch),
        ("tar", has_tar),
    ] {
        if carried && !valid_names.contains(&name) {
            validation.push_str("fail artifact_validation; ");
        }
    }

    replay_script_builder(
        workdir,
        ReplayArtifacts {
            bundle: staged_artifact_path(dir, "bundle"),
            patch: staged_artifact_path(dir, "patch"),
            tar: staged_artifact_path(dir, "tar"),
            dir: Some(dir.to_string()),
        },
        ReplayOptions {
            reset_head,
            has_patch,
            has_tar,
            done_msg,
            validation,
        },
    )
}

struct ReplayArtifacts {
    bundle: String,
    patch: String,
    tar: String,
    dir: Option<String>,
}

struct ReplayOptions<'a> {
    reset_head: Option<&'a str>,
    has_patch: bool,
    has_tar: bool,
    done_msg: &'a str,
    validation: String,
}

fn replay_script_builder(
    workdir: &str,
    artifacts: ReplayArtifacts,
    options: ReplayOptions<'_>,
) -> String {
    let wd = sh_quote(workdir);
    let bundle = sh_quote(&artifacts.bundle);
    let patch = sh_quote(&artifacts.patch);
    let tar = sh_quote(&artifacts.tar);
    let reset = match options.reset_head {
        Some(h) => format!(
            "if [ ! -s {bundle} ]; then fail artifact_validation; fi; \
             if ! git fetch {bundle} HEAD 2>&1; then \
               if ! git fetch {bundle} 2>&1; then fail fetch; fi; \
             fi; \
             if ! git reset --hard {} 2>&1; then fail reset; fi; ",
            sh_quote(h)
        ),
        None => String::new(),
    };
    let apply_patch = if options.has_patch {
        format!(
            "if [ ! -s {patch} ]; then fail artifact_validation; fi; \
             if ! git apply --whitespace=nowarn {patch} 2>&1; then \
               if ! git apply --3way --whitespace=nowarn {patch} 2>&1; then fail patch; fi; \
             fi; "
        )
    } else {
        String::new()
    };
    let untar = if options.has_tar {
        format!(
            "if [ ! -s {tar} ]; then fail artifact_validation; fi; \
             if ! tar xf {tar} -C {wd} 2>&1; then fail extraction; fi; "
        )
    } else {
        String::new()
    };
    let cleanup = match artifacts.dir {
        Some(dir) => format!(
            "if ! rm -f {bundle} {patch} {tar}; then fail cleanup; fi; \
             if ! rmdir {}; then fail cleanup; fi; ",
            sh_quote(&dir)
        ),
        None => format!("if ! rm -f {bundle} {patch} {tar}; then fail cleanup; fi; "),
    };
    format!(
        "fail() {{ printf 'TG-PARITY-ERROR:%s\\n' \"$1\"; exit 70; }}; \
         cd {wd} || fail checkout; {}{reset}{apply_patch}{untar}\
         {cleanup}\
         echo {}",
        options.validation,
        sh_quote(options.done_msg)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_script_stages_are_guarded_and_trailer_is_emitted() {
        // sh_quote passes safe paths through bare and quotes the rest.
        let s = capture_script("/work/tree", "tg-snap");
        assert!(s.starts_with("cd /work/tree || exit 1;"));
        assert!(s.contains("git bundle create /tmp/tg-snap.bundle HEAD --not --remotes"));
        assert!(s.contains("git diff HEAD --binary > /tmp/tg-snap.patch"));
        assert!(s.contains("git ls-files --others --exclude-standard -z"));
        assert!(s.contains("tar -C /work/tree --null -T /tmp/tg-snap.list -czf /tmp/tg-snap.tar"));
        assert!(s.contains("TG-HEAD"));
        assert!(s.contains("TG-BRANCH"));
        assert!(s.contains("sha256sum"));
        let odd = capture_script("/work/my tree", "tg-snap");
        assert!(odd.starts_with("cd '/work/my tree' || exit 1;"));
    }

    #[test]
    fn parse_roundtrips_a_full_report_amid_noise() {
        let out = "warning: some git chatter\n\
                   TG-HEAD 0123456789abcdef0123456789abcdef01234567\n\
                   TG-BRANCH feature/x\n\
                   TG-ART bundle 1024 "
            .to_string()
            + &"a".repeat(64)
            + "\nTG-ART tar 99 "
            + &"B".repeat(64)
            + "\n";
        let r = parse_capture_output(&out).unwrap();
        assert_eq!(
            r.head.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(r.branch, "feature/x");
        assert_eq!(r.artifacts.len(), 2);
        assert_eq!(r.artifact("bundle").unwrap().bytes, 1024);
        // Hashes normalize to lowercase for host-side comparison.
        assert_eq!(r.artifact("tar").unwrap().sha256, "b".repeat(64));
        assert!(r.artifact("patch").is_none());
        assert!(!r.is_empty());
    }

    #[test]
    fn parse_maps_unborn_head_to_none_and_no_artifacts_to_empty() {
        let out = "TG-HEAD none\nTG-BRANCH HEAD\n";
        let r = parse_capture_output(out).unwrap();
        assert_eq!(r.head, None);
        assert!(r.is_empty());
    }

    #[test]
    fn parse_rejects_malformed_reports() {
        // Missing the HEAD line entirely (script died early).
        assert!(parse_capture_output("TG-BRANCH main\n").is_err());
        let head = format!("TG-HEAD {}\n", "c".repeat(40));
        // Truncated TG-ART line.
        assert!(parse_capture_output(&format!("{head}TG-ART bundle 12\n")).is_err());
        // Unknown artifact name.
        let sha = "d".repeat(64);
        assert!(parse_capture_output(&format!("{head}TG-ART exe 12 {sha}\n")).is_err());
        // Non-numeric size.
        assert!(parse_capture_output(&format!("{head}TG-ART tar big {sha}\n")).is_err());
        // Bad hash (wrong length / non-hex).
        assert!(parse_capture_output(&format!("{head}TG-ART tar 12 abc\n")).is_err());
        let bad = "z".repeat(64);
        assert!(parse_capture_output(&format!("{head}TG-ART tar 12 {bad}\n")).is_err());
    }

    #[test]
    fn replay_script_includes_only_carried_stages() {
        let full = replay_script("/wd", "tg-parity", Some("abc123"), true, true, "done");
        assert!(full.contains("git fetch /tmp/tg-parity.bundle HEAD"));
        assert!(full.contains("git reset --hard abc123"));
        assert!(full.contains("git apply --whitespace=nowarn /tmp/tg-parity.patch"));
        assert!(full.contains("git apply --3way"));
        assert!(full.contains("tar xf /tmp/tg-parity.tar -C /wd"));
        assert!(full.contains("fail fetch;"));
        assert!(full.contains("fail reset;"));
        assert!(full.contains("fail patch;"));
        assert!(full.contains("fail extraction;"));
        assert!(full.contains("TG-PARITY-ERROR:%s\\n"));
        assert!(!full.contains("|| true"));
        assert!(full.contains("exit 70;"));
        assert!(!full.contains("\" >&2"));
        assert!(
            full.contains("rm -f /tmp/tg-parity.bundle /tmp/tg-parity.patch /tmp/tg-parity.tar")
        );
        assert!(full.find("rm -f").unwrap() < full.find("echo done").unwrap());
        assert!(full.ends_with("echo done"));

        let none = replay_script("/wd", "tg-parity", None, false, false, "noop");
        assert!(!none.contains("git fetch"));
        assert!(!none.contains("git apply"));
        assert!(!none.contains("tar xf"));
        // Artifacts are still consumed even when nothing replays.
        assert!(none.contains("rm -f /tmp/tg-parity.bundle"));
    }

    #[test]
    fn replay_script_output_is_unchanged() {
        assert_eq!(
            replay_script("/wd", "tg-parity", Some("abc123"), true, true, "done"),
            "fail() { printf 'TG-PARITY-ERROR:%s\\n' \"$1\"; exit 70; }; cd /wd || fail checkout; if [ ! -s /tmp/tg-parity.bundle ]; then fail artifact_validation; fi; if ! git fetch /tmp/tg-parity.bundle HEAD 2>&1; then if ! git fetch /tmp/tg-parity.bundle 2>&1; then fail fetch; fi; fi; if ! git reset --hard abc123 2>&1; then fail reset; fi; if [ ! -s /tmp/tg-parity.patch ]; then fail artifact_validation; fi; if ! git apply --whitespace=nowarn /tmp/tg-parity.patch 2>&1; then if ! git apply --3way --whitespace=nowarn /tmp/tg-parity.patch 2>&1; then fail patch; fi; fi; if [ ! -s /tmp/tg-parity.tar ]; then fail artifact_validation; fi; if ! tar xf /tmp/tg-parity.tar -C /wd 2>&1; then fail extraction; fi; if ! rm -f /tmp/tg-parity.bundle /tmp/tg-parity.patch /tmp/tg-parity.tar; then fail cleanup; fi; echo done"
        );
    }

    #[test]
    fn verified_replay_checks_digests_before_any_mutation() {
        let manifest = [
            CaptureArtifact {
                name: "bundle".into(),
                bytes: 11,
                sha256: "b9c72134b48cdc15e7a47f2476a41612d2084b763bea0575f5600b22041db7dc".into(),
            },
            CaptureArtifact {
                name: "patch".into(),
                bytes: 10,
                sha256: "50d39ba6ed2bd5b854b405fcc2189e9f5f9fbe637f107a7cf31408207d5f6499".into(),
            },
            CaptureArtifact {
                name: "tar".into(),
                bytes: 8,
                sha256: "52232c0fe139f6b5168632a400c4b67dbc80c112edf952aa7c67bccec57f1dbd".into(),
            },
        ];
        let script = replay_script_verified(
            "/work tree",
            "/tmp/tg-parity-operation",
            &manifest,
            Some("head"),
            true,
            true,
            "done",
        );
        let first_mutation = script.find("git fetch").unwrap();
        assert_eq!(script.matches("sha256sum -c - >/dev/null 2>&1").count(), 3);
        for digest in [
            "b9c72134b48cdc15e7a47f2476a41612d2084b763bea0575f5600b22041db7dc",
            "50d39ba6ed2bd5b854b405fcc2189e9f5f9fbe637f107a7cf31408207d5f6499",
            "52232c0fe139f6b5168632a400c4b67dbc80c112edf952aa7c67bccec57f1dbd",
        ] {
            assert!(script.contains(digest));
            assert!(script.find(digest).unwrap() < first_mutation);
        }
        for mutation in ["git fetch", "git apply", "tar xf"] {
            assert!(
                script.find("sha256sum -c - >/dev/null 2>&1").unwrap()
                    < script.find(mutation).unwrap()
            );
        }
        assert!(script.contains("/tmp/tg-parity-operation/bundle"));
        assert!(script.contains("/tmp/tg-parity-operation/patch"));
        assert!(script.contains("/tmp/tg-parity-operation/tar"));
    }

    #[test]
    fn verified_replay_refuses_a_carried_stage_without_a_manifest_entry() {
        let script = replay_script_verified("/wd", "/tmp/op", &[], None, true, false, "done");
        assert!(script.contains("cd /wd || fail checkout; fail artifact_validation; "));
    }

    #[test]
    fn verified_replay_refuses_a_missing_manifest_entry_before_stage_file_checks() {
        let manifest = [CaptureArtifact {
            name: "patch".into(),
            bytes: 5,
            sha256: "a4895eb44afc336fecbba6e520cd67e178dace0276655d102fceffa8e5f70570".into(),
        }];
        let script = replay_script_verified("/wd", "/tmp/op", &manifest, None, true, true, "done");
        let manifest_failure = script.find("fail artifact_validation; if [ ! -s").unwrap();
        let first_file_check = script.find("[ ! -s").unwrap();
        assert!(manifest_failure < first_file_check);
    }

    #[test]
    fn verified_replay_digest_mismatch_reports_validation_and_preserves_staging_dir() {
        use std::process::Command;

        let sh_check = Command::new("sh")
            .arg("-c")
            .arg("command -v sha256sum >/dev/null 2>&1")
            .status();
        let Ok(available) = sh_check else {
            return;
        };
        if !available.success() {
            return;
        }

        let workdir = tempfile::tempdir().unwrap();
        let staging_dir = workdir.path().join("staging");
        std::fs::create_dir(&staging_dir).unwrap();
        let artifact = staging_dir.join("patch");
        std::fs::write(&artifact, b"real patch bytes").unwrap();
        let manifest = [CaptureArtifact {
            name: "patch".into(),
            bytes: 16,
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        }];
        let script = replay_script_verified(
            workdir.path().to_str().unwrap(),
            staging_dir.to_str().unwrap(),
            &manifest,
            None,
            true,
            false,
            "done",
        );
        let output = Command::new("sh").arg("-c").arg(script).output().unwrap();
        assert_eq!(output.status.code(), Some(70));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("TG-PARITY-ERROR:artifact_validation"),
            "stdout: {stdout}"
        );
        assert!(artifact.exists());
        assert!(staging_dir.exists());
    }

    #[test]
    fn verified_replay_uses_the_staging_dir_and_removes_it() {
        let manifest = [CaptureArtifact {
            name: "patch".into(),
            bytes: 5,
            sha256: "a4895eb44afc336fecbba6e520cd67e178dace0276655d102fceffa8e5f70570".into(),
        }];
        let script = replay_script_verified("/wd", "/tmp/op", &manifest, None, true, false, "done");
        assert!(script.contains("/tmp/op/patch"));
        assert!(script.contains("rmdir /tmp/op"));
        assert!(
            script
                .find("rm -f /tmp/op/bundle /tmp/op/patch /tmp/op/tar")
                .unwrap()
                < script.find("rmdir /tmp/op").unwrap()
        );
    }

    #[test]
    fn verified_replay_rejects_unknown_and_duplicate_manifest_names() {
        let unknown = [CaptureArtifact {
            name: "other".into(),
            bytes: 0,
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        }];
        let unknown_script =
            replay_script_verified("/wd", "/tmp/op", &unknown, None, false, false, "done");
        assert!(unknown_script.contains("fail artifact_validation;"));

        let duplicate = [
            CaptureArtifact {
                name: "patch".into(),
                bytes: 5,
                sha256: "a4895eb44afc336fecbba6e520cd67e178dace0276655d102fceffa8e5f70570".into(),
            },
            CaptureArtifact {
                name: "patch".into(),
                bytes: 5,
                sha256: "a4895eb44afc336fecbba6e520cd67e178dace0276655d102fceffa8e5f70570".into(),
            },
        ];
        let duplicate_script =
            replay_script_verified("/wd", "/tmp/op", &duplicate, None, false, false, "done");
        assert!(duplicate_script.contains("fail artifact_validation;"));
    }

    #[test]
    fn stage_dir_script_is_private_and_exclusive() {
        let script = stage_dir_script("/tmp/tg-parity-op");
        assert!(script.starts_with("umask 077; mkdir -- "));
        assert!(!script.contains("mkdir -p"));
        assert!(script.ends_with("/tmp/tg-parity-op"));
        assert_eq!(
            staged_artifact_path("/tmp/tg-parity-op", "patch"),
            "/tmp/tg-parity-op/patch"
        );
    }

    #[test]
    fn patch_failure_reports_on_stdout_and_preserves_artifact() {
        use std::process::Command;
        use std::time::{SystemTime, UNIX_EPOCH};

        let repo = tempfile::tempdir().unwrap();
        let run_git = |args: &[&str]| {
            crate::util::git_cmd(repo.path())
                .arg("-c")
                .arg("commit.gpgsign=false")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(args)
                .output()
                .unwrap()
        };
        assert!(run_git(&["init", "-q"]).status.success());
        assert!(
            run_git(&["config", "user.name", "Parity Test"])
                .status
                .success()
        );
        assert!(
            run_git(&["config", "user.email", "parity@example.invalid"])
                .status
                .success()
        );
        std::fs::write(repo.path().join("tracked.txt"), "actual\n").unwrap();
        assert!(run_git(&["add", "tracked.txt"]).status.success());
        assert!(run_git(&["commit", "-m", "initial"]).status.success());

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let stem = format!("tg-parity-test-{}-{nanos}", std::process::id());
        let bundle_path = artifact_path(&stem, "bundle");
        let patch_path = artifact_path(&stem, "patch");
        let tar_path = artifact_path(&stem, "tar");
        let bad_patch = "diff --git a/tracked.txt b/tracked.txt\n\
            index 1234567..89abcde 100644\n\
            --- a/tracked.txt\n\
            +++ b/tracked.txt\n\
            @@ -1 +1 @@\n\
            -old\n\
            +new\n";
        std::fs::write(&patch_path, bad_patch).unwrap();
        std::fs::write(&bundle_path, "unused bundle artifact").unwrap();
        std::fs::write(&tar_path, "unused tar artifact").unwrap();

        let script = replay_script(
            repo.path().to_str().unwrap(),
            &stem,
            None,
            true,
            false,
            "done",
        );
        let output = Command::new("sh").arg("-c").arg(script).output().unwrap();
        assert_eq!(output.status.code(), Some(70));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("TG-PARITY-ERROR:patch"), "stdout: {stdout}");
        assert!(std::path::Path::new(&bundle_path).exists());
        assert!(std::path::Path::new(&patch_path).exists());
        assert!(std::path::Path::new(&tar_path).exists());
        std::fs::remove_file(bundle_path).unwrap();
        std::fs::remove_file(patch_path).unwrap();
        std::fs::remove_file(tar_path).unwrap();
    }

    #[test]
    fn artifact_paths_follow_the_stem() {
        assert_eq!(artifact_path("tg-snap", "bundle"), "/tmp/tg-snap.bundle");
        for n in ARTIFACT_NAMES {
            assert!(artifact_path("s", n).starts_with("/tmp/s."));
        }
    }
}
