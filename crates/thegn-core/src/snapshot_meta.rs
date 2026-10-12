//! Snapshot metadata for worktree hibernation: the pure key/manifest layer
//! over the durable snapshot store (`thegn-svc::snapshot`). A snapshot is
//! the artifact triple captured from a sandbox worktree just before its
//! compute is destroyed ([`crate::syncstate`]), plus this manifest describing
//! it. The manifest is written LAST — a snapshot without one is invisible
//! (torn write ⇒ garbage to collect, never a restore candidate).

use serde::{Deserialize, Serialize};

/// Addresses one worktree's snapshot lineage in the store:
/// `<repo>/<worktree>/<env>` after sanitization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotKey {
    pub repo_slug: String,
    pub worktree_slug: String,
    pub env: String,
}

impl SnapshotKey {
    /// The store path/key prefix for this worktree's snapshots. Each segment
    /// is encoded losslessly while keeping `[A-Za-z0-9._-]` bytes unchanged,
    /// so the result is safe as both a filesystem path and an S3 key.
    pub fn prefix(&self) -> String {
        format!(
            "{}/{}/{}",
            encode_segment(&self.repo_slug),
            encode_segment(&self.worktree_slug),
            encode_segment(&self.env)
        )
    }

    /// The pre-encoder key used by snapshots created before lossless prefixes.
    pub fn legacy(&self) -> SnapshotKey {
        SnapshotKey {
            repo_slug: legacy_segment(&self.repo_slug),
            worktree_slug: legacy_segment(&self.worktree_slug),
            env: legacy_segment(&self.env),
        }
    }
}

fn encode_segment(s: &str) -> String {
    if s.is_empty() {
        return "~".into();
    }
    if s.bytes().all(|b| b == b'.') {
        return "~2E".repeat(s.len());
    }
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(out, "~{byte:02X}").expect("writing to String cannot fail");
        }
    }
    out
}

fn legacy_segment(s: &str) -> String {
    let out: String = s
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // "." / ".." would escape or alias directories; an empty segment would
    // collapse the hierarchy.
    if out.is_empty() || out.chars().all(|c| c == '.') {
        "_".into()
    } else {
        out
    }
}

/// One stored artifact: its short name ([`crate::syncstate::ARTIFACT_NAMES`])
/// plus size and sha256 as verified on the HOST after download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

/// The manifest describing one snapshot. `id` sorts chronologically by
/// construction (zero-padded epoch seconds first), so lexicographic order ==
/// capture order across store backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    /// `<created_at:020>-<head8|nohead>[-<generation>]`.
    pub id: String,
    /// HEAD of the sandbox worktree at capture time (empty for unborn).
    pub head: String,
    /// Branch name at capture time (`"HEAD"` when detached).
    pub branch: String,
    /// Capture time, epoch seconds.
    pub created_at: i64,
    /// The non-empty artifacts this snapshot carries.
    pub artifacts: Vec<ArtifactMeta>,
}

impl SnapshotManifest {
    pub fn new(
        head: Option<&str>,
        branch: &str,
        created_at: i64,
        artifacts: Vec<ArtifactMeta>,
    ) -> Self {
        let head8: String = head.unwrap_or("").chars().take(8).collect::<String>();
        let tag = if head8.is_empty() {
            "nohead".into()
        } else {
            head8
        };
        SnapshotManifest {
            id: format!("{:020}-{tag}", created_at.max(0)),
            head: head.unwrap_or("").to_string(),
            branch: branch.to_string(),
            created_at,
            artifacts,
        }
    }

    pub fn artifact(&self, name: &str) -> Option<&ArtifactMeta> {
        self.artifacts.iter().find(|a| a.name == name)
    }

    /// Add a lowercase hexadecimal capture generation to the chronological id.
    pub fn with_generation(mut self, generation: &str) -> Self {
        debug_assert!(
            generation
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "snapshot generation must be lowercase hexadecimal"
        );
        self.id.push('-');
        self.id.push_str(generation);
        self
    }

    /// Verify a set of `(name, bytes, sha256)` observations (e.g. re-hashed
    /// after a store round-trip) against this manifest: every manifest
    /// artifact must be present and match exactly. Extra observations are an
    /// error too — the store returned something this manifest never wrote.
    pub fn verify(&self, got: &[(String, u64, String)]) -> Result<(), String> {
        for a in &self.artifacts {
            let hit = got
                .iter()
                .find(|(n, _, _)| n == &a.name)
                .ok_or_else(|| format!("snapshot {}: artifact {} missing", self.id, a.name))?;
            if hit.1 != a.bytes {
                return Err(format!(
                    "snapshot {}: artifact {} is {} bytes, manifest says {}",
                    self.id, a.name, hit.1, a.bytes
                ));
            }
            if !hit.2.eq_ignore_ascii_case(&a.sha256) {
                return Err(format!(
                    "snapshot {}: artifact {} checksum mismatch",
                    self.id, a.name
                ));
            }
        }
        if let Some((n, _, _)) = got.iter().find(|(n, _, _)| self.artifact(n).is_none()) {
            return Err(format!(
                "snapshot {}: unexpected artifact {n} not in manifest",
                self.id
            ));
        }
        Ok(())
    }
}

/// Retention: given every manifest under one [`SnapshotKey`], the ids to
/// DELETE so only the `keep` newest remain. `keep` is clamped to ≥ 1 — pruning
/// the only copy of just-captured work is never correct.
pub fn retention_prune(manifests: &[SnapshotManifest], keep: usize) -> Vec<String> {
    let keep = keep.max(1);
    if manifests.len() <= keep {
        return Vec::new();
    }
    let mut ordered: Vec<&SnapshotManifest> = manifests.iter().collect();
    // Newest first: created_at, then id as the deterministic tiebreak.
    ordered.sort_by(|a, b| (b.created_at, &b.id).cmp(&(a.created_at, &a.id)));
    ordered[keep..].iter().map(|m| m.id.clone()).collect()
}

/// Retention that always keeps the just-published manifest, even when its id
/// sorts below another capture with the same timestamp.
pub fn retention_prune_keeping(
    manifests: &[SnapshotManifest],
    keep: usize,
    current_id: &str,
) -> Vec<String> {
    let keep = keep.max(1);
    let current_is_listed = manifests.iter().any(|m| m.id == current_id);
    let keep_others = if current_is_listed {
        keep.saturating_sub(1)
    } else {
        keep
    };
    let mut ordered: Vec<&SnapshotManifest> =
        manifests.iter().filter(|m| m.id != current_id).collect();
    ordered.sort_by(|a, b| (b.created_at, &b.id).cmp(&(a.created_at, &a.id)));
    ordered
        .iter()
        .skip(keep_others)
        .map(|m| m.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(created_at: i64, head: &str) -> SnapshotManifest {
        SnapshotManifest::new(Some(head), "main", created_at, Vec::new())
    }

    #[test]
    fn key_prefix_encodes_each_segment_losslessly() {
        let k = SnapshotKey {
            repo_slug: "my repo".into(),
            worktree_slug: "tg/brisk-fox".into(),
            env: "hetzner:prod".into(),
        };
        assert_eq!(k.prefix(), "my~20repo/tg~2Fbrisk-fox/hetzner~3Aprod");
        assert_eq!(encode_segment("~é"), "~7E~C3~A9");
        let colliding = ["my repo", "my-repo", "my/repo"].map(|repo_slug| SnapshotKey {
            repo_slug: repo_slug.into(),
            ..k.clone()
        });
        assert_ne!(colliding[0].prefix(), colliding[1].prefix());
        assert_ne!(colliding[0].prefix(), colliding[2].prefix());
        assert_ne!(colliding[1].prefix(), colliding[2].prefix());

        // Traversal, empty, all-dot, and ordinary underscore segments stay
        // distinct and pass the filesystem store's component contract.
        let evil = SnapshotKey {
            repo_slug: "..".into(),
            worktree_slug: "".into(),
            env: "ok".into(),
        };
        let encoded = evil.prefix();
        assert_eq!(encoded, "~2E~2E/~/ok");
        for segment in encoded.split('/') {
            assert!(!segment.is_empty());
            assert!(!segment.contains(['/', '\\']));
            assert_ne!(segment, ".");
            assert_ne!(segment, "..");
        }
        let underscore = SnapshotKey {
            repo_slug: "_".into(),
            ..evil.clone()
        };
        assert_ne!(evil.prefix(), underscore.prefix());
        let empty = SnapshotKey {
            repo_slug: "".into(),
            ..evil.clone()
        };
        assert_ne!(empty.prefix(), underscore.prefix());
        let dots = SnapshotKey {
            repo_slug: "..".into(),
            ..evil.clone()
        };
        assert_ne!(empty.prefix(), dots.prefix());
        assert_eq!(k.legacy().prefix(), "my-repo/tg-brisk-fox/hetzner-prod");
        let legacy_empty = SnapshotKey {
            repo_slug: "".into(),
            worktree_slug: "..".into(),
            env: "ok".into(),
        };
        assert_eq!(legacy_empty.legacy().prefix(), "_/_/ok");
    }

    #[test]
    fn manifest_id_sorts_chronologically_and_carries_head() {
        let a = m(5, "abcdef1234567890");
        let b = m(1_700_000_000, "1234567890abcdef");
        assert!(a.id < b.id, "{} !< {}", a.id, b.id);
        assert!(a.id.ends_with("-abcdef12"));
        let unborn = SnapshotManifest::new(None, "HEAD", 7, Vec::new());
        assert!(unborn.id.ends_with("-nohead"));
        assert_eq!(unborn.head, "");
    }

    #[test]
    fn generation_makes_same_second_same_head_ids_distinct() {
        let first = m(42, "abcdef1234567890").with_generation("0123456789abcdef");
        let second = m(42, "abcdef1234567890").with_generation("fedcba9876543210");
        assert_ne!(first.id, second.id);
        assert!(first.id.starts_with("00000000000000000042-abcdef12-"));
    }

    #[test]
    fn manifest_roundtrips_json() {
        let m = SnapshotManifest::new(
            Some("abc123"),
            "feature/x",
            42,
            vec![ArtifactMeta {
                name: "bundle".into(),
                bytes: 10,
                sha256: "f".repeat(64),
            }],
        );
        let s = serde_json::to_string(&m).unwrap();
        let back: SnapshotManifest = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn verify_accepts_exact_match_and_rejects_drift() {
        let man = SnapshotManifest::new(
            Some("abc"),
            "main",
            1,
            vec![
                ArtifactMeta {
                    name: "bundle".into(),
                    bytes: 10,
                    sha256: "a".repeat(64),
                },
                ArtifactMeta {
                    name: "tar".into(),
                    bytes: 20,
                    sha256: "b".repeat(64),
                },
            ],
        );
        let ok = vec![
            ("bundle".to_string(), 10, "A".repeat(64)), // case-insensitive hash
            ("tar".to_string(), 20, "b".repeat(64)),
        ];
        assert!(man.verify(&ok).is_ok());
        // Missing artifact.
        assert!(man.verify(&ok[..1]).is_err());
        // Wrong size.
        let mut bad = ok.clone();
        bad[0].1 = 11;
        assert!(man.verify(&bad).is_err());
        // Wrong hash.
        let mut bad = ok.clone();
        bad[1].2 = "c".repeat(64);
        assert!(man.verify(&bad).is_err());
        // Extra artifact the manifest never wrote.
        let mut extra = ok.clone();
        extra.push(("patch".to_string(), 1, "d".repeat(64)));
        assert!(man.verify(&extra).is_err());
    }

    #[test]
    fn retention_keeps_newest_and_clamps_to_one() {
        let all = vec![m(1, "a1"), m(3, "a3"), m(2, "a2"), m(4, "a4")];
        let del = retention_prune(&all, 2);
        // Keep 4 and 3; delete 2 then 1 (older last in newest-first tail order).
        assert_eq!(del, vec![all[2].id.clone(), all[0].id.clone()]);
        assert!(retention_prune(&all, 10).is_empty());
        // keep = 0 clamps to 1: everything but the newest goes.
        let del0 = retention_prune(&all, 0);
        assert_eq!(del0.len(), 3);
        assert!(!del0.contains(&all[3].id));
    }

    #[test]
    fn retention_ties_break_deterministically_by_id() {
        let a = SnapshotManifest::new(Some("aaaa1111"), "main", 5, Vec::new());
        let b = SnapshotManifest::new(Some("bbbb2222"), "main", 5, Vec::new());
        let del = retention_prune(&[a.clone(), b.clone()], 1);
        // Same created_at: the lexicographically larger id wins the keep slot.
        assert_eq!(del, vec![a.id]);
    }

    #[test]
    fn retention_keeps_current_capture_when_same_second_id_sorts_lower() {
        let current = m(42, "abcdef1234567890").with_generation("0000000000000000");
        let previous = m(42, "abcdef1234567890").with_generation("ffffffffffffffff");
        let all = [current.clone(), previous.clone()];

        assert_eq!(
            retention_prune_keeping(&all, 1, &current.id),
            vec![previous.id]
        );
    }
}
