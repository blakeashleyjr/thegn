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
/// in `/tmp/<stem>.<name>` on the sandbox side and the `SZ-ART <name> …`
/// trailer lines a capture emits.
pub const ARTIFACT_NAMES: [&str; 3] = ["bundle", "patch", "tar"];

/// Sandbox-side path of one artifact for a given file stem.
pub fn artifact_path(stem: &str, name: &str) -> String {
    format!("/tmp/{stem}.{name}")
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
/// SZ-HEAD <sha|none>
/// SZ-BRANCH <name>
/// SZ-ART <name> <bytes> <sha256>     (one line per non-empty artifact)
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
         echo \"SZ-HEAD $(git rev-parse HEAD 2>/dev/null || echo none)\"; \
         echo \"SZ-BRANCH $(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)\"; \
         for n in bundle patch tar; do \
           p=/tmp/{stem}.$n; \
           if [ -s \"$p\" ]; then \
             echo \"SZ-ART $n $(wc -c < \"$p\" | tr -d ' ') $(sha256sum \"$p\" | cut -d' ' -f1)\"; \
           fi; \
         done"
    )
}

/// Parse the trailer printed by [`capture_script`]. Tolerates arbitrary noise
/// before/between the `SZ-*` lines (git chatter), but every `SZ-ART` line must
/// be well-formed — a malformed report is an error, never a silent skip: the
/// caller is about to destroy the VM on the strength of this report.
pub fn parse_capture_output(out: &str) -> Result<CaptureReport, String> {
    let mut head = None;
    let mut branch = None;
    let mut artifacts = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("SZ-HEAD ") {
            let h = rest.trim();
            head = Some((h != "none" && !h.is_empty()).then(|| h.to_string()));
        } else if let Some(rest) = line.strip_prefix("SZ-BRANCH ") {
            branch = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("SZ-ART ") {
            let mut parts = rest.split_whitespace();
            let (name, bytes, sha) = (parts.next(), parts.next(), parts.next());
            let (Some(name), Some(bytes), Some(sha)) = (name, bytes, sha) else {
                return Err(format!("malformed SZ-ART line: {line:?}"));
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
    let head = head.ok_or("capture report missing SZ-HEAD (script did not run to completion)")?;
    Ok(CaptureReport {
        head,
        branch: branch.unwrap_or_else(|| "HEAD".into()),
        artifacts,
    })
}

/// Build the sh script that replays a previously-uploaded artifact triple over
/// the clone at `workdir`: fetch the bundle + hard-reset to `reset_head` (when
/// a bundle was carried), apply the patch (3-way fallback), untar the
/// untracked files (`tar xf` auto-detects gzip). Each stage is independently
/// guarded (`[ -s file ]`) and non-fatal so a partial upload still helps; the
/// artifacts are consumed (removed) at the end.
pub fn replay_script(
    workdir: &str,
    stem: &str,
    reset_head: Option<&str>,
    has_patch: bool,
    has_tar: bool,
    done_msg: &str,
) -> String {
    let wd = sh_quote(workdir);
    let bundle = artifact_path(stem, "bundle");
    let patch = artifact_path(stem, "patch");
    let tar = artifact_path(stem, "tar");
    let reset = match reset_head {
        Some(h) => format!(
            "if [ -s {bundle} ]; then \
               git fetch {bundle} HEAD 2>&1 || git fetch {bundle} 2>&1 || true; \
               git reset --hard {} 2>&1 || true; \
             fi; ",
            sh_quote(h)
        ),
        None => String::new(),
    };
    let apply_patch = if has_patch {
        format!(
            "if [ -s {patch} ]; then \
               git apply --whitespace=nowarn {patch} 2>&1 \
                 || git apply --3way --whitespace=nowarn {patch} 2>&1 || true; \
             fi; "
        )
    } else {
        String::new()
    };
    let untar = if has_tar {
        format!("if [ -s {tar} ]; then tar xf {tar} -C {wd} 2>&1 || true; fi; ")
    } else {
        String::new()
    };
    format!(
        "cd {wd} || exit 1; {reset}{apply_patch}{untar}\
         rm -f {bundle} {patch} {tar} 2>/dev/null; \
         echo {}",
        sh_quote(done_msg)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_script_stages_are_guarded_and_trailer_is_emitted() {
        // sh_quote passes safe paths through bare and quotes the rest.
        let s = capture_script("/work/tree", "sz-snap");
        assert!(s.starts_with("cd /work/tree || exit 1;"));
        assert!(s.contains("git bundle create /tmp/sz-snap.bundle HEAD --not --remotes"));
        assert!(s.contains("git diff HEAD --binary > /tmp/sz-snap.patch"));
        assert!(s.contains("git ls-files --others --exclude-standard -z"));
        assert!(s.contains("tar -C /work/tree --null -T /tmp/sz-snap.list -czf /tmp/sz-snap.tar"));
        assert!(s.contains("SZ-HEAD"));
        assert!(s.contains("SZ-BRANCH"));
        assert!(s.contains("sha256sum"));
        let odd = capture_script("/work/my tree", "sz-snap");
        assert!(odd.starts_with("cd '/work/my tree' || exit 1;"));
    }

    #[test]
    fn parse_roundtrips_a_full_report_amid_noise() {
        let out = "warning: some git chatter\n\
                   SZ-HEAD 0123456789abcdef0123456789abcdef01234567\n\
                   SZ-BRANCH feature/x\n\
                   SZ-ART bundle 1024 "
            .to_string()
            + &"a".repeat(64)
            + "\nSZ-ART tar 99 "
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
        let out = "SZ-HEAD none\nSZ-BRANCH HEAD\n";
        let r = parse_capture_output(out).unwrap();
        assert_eq!(r.head, None);
        assert!(r.is_empty());
    }

    #[test]
    fn parse_rejects_malformed_reports() {
        // Missing the HEAD line entirely (script died early).
        assert!(parse_capture_output("SZ-BRANCH main\n").is_err());
        let head = format!("SZ-HEAD {}\n", "c".repeat(40));
        // Truncated SZ-ART line.
        assert!(parse_capture_output(&format!("{head}SZ-ART bundle 12\n")).is_err());
        // Unknown artifact name.
        let sha = "d".repeat(64);
        assert!(parse_capture_output(&format!("{head}SZ-ART exe 12 {sha}\n")).is_err());
        // Non-numeric size.
        assert!(parse_capture_output(&format!("{head}SZ-ART tar big {sha}\n")).is_err());
        // Bad hash (wrong length / non-hex).
        assert!(parse_capture_output(&format!("{head}SZ-ART tar 12 abc\n")).is_err());
        let bad = "z".repeat(64);
        assert!(parse_capture_output(&format!("{head}SZ-ART tar 12 {bad}\n")).is_err());
    }

    #[test]
    fn replay_script_includes_only_carried_stages() {
        let full = replay_script("/wd", "sz-parity", Some("abc123"), true, true, "done");
        assert!(full.contains("git fetch /tmp/sz-parity.bundle HEAD"));
        assert!(full.contains("git reset --hard abc123"));
        assert!(full.contains("git apply --whitespace=nowarn /tmp/sz-parity.patch"));
        assert!(full.contains("git apply --3way"));
        assert!(full.contains("tar xf /tmp/sz-parity.tar -C /wd"));
        assert!(
            full.contains("rm -f /tmp/sz-parity.bundle /tmp/sz-parity.patch /tmp/sz-parity.tar")
        );
        assert!(full.ends_with("echo done"));

        let none = replay_script("/wd", "sz-parity", None, false, false, "noop");
        assert!(!none.contains("git fetch"));
        assert!(!none.contains("git apply"));
        assert!(!none.contains("tar xf"));
        // Artifacts are still consumed even when nothing replays.
        assert!(none.contains("rm -f /tmp/sz-parity.bundle"));
    }

    #[test]
    fn artifact_paths_follow_the_stem() {
        assert_eq!(artifact_path("sz-snap", "bundle"), "/tmp/sz-snap.bundle");
        for n in ARTIFACT_NAMES {
            assert!(artifact_path("s", n).starts_with("/tmp/s."));
        }
    }
}
