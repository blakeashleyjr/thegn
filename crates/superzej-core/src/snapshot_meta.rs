//! Snapshot metadata for worktree hibernation: the pure key/manifest layer
//! over the durable snapshot store (`superzej-svc::snapshot`). A snapshot is
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
    /// is sanitized to `[A-Za-z0-9._-]` (everything else → `-`) so the result
    /// is safe as both a filesystem path and an S3 key; empty segments become
    /// `"_"` so the prefix always has exactly three levels.
    pub fn prefix(&self) -> String {
        format!(
            "{}/{}/{}",
            sanitize_segment(&self.repo_slug),
            sanitize_segment(&self.worktree_slug),
            sanitize_segment(&self.env)
        )
    }
}

fn sanitize_segment(s: &str) -> String {
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
    /// `<created_at:020>-<head8|nohead>`.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn m(created_at: i64, head: &str) -> SnapshotManifest {
        SnapshotManifest::new(Some(head), "main", created_at, Vec::new())
    }

    #[test]
    fn key_prefix_sanitizes_each_segment() {
        let k = SnapshotKey {
            repo_slug: "my repo".into(),
            worktree_slug: "sz/brisk-fox".into(),
            env: "hetzner:prod".into(),
        };
        assert_eq!(k.prefix(), "my-repo/sz-brisk-fox/hetzner-prod");
        // Path-traversal / empty segments can't escape or collapse the tree.
        let evil = SnapshotKey {
            repo_slug: "..".into(),
            worktree_slug: "".into(),
            env: "ok".into(),
        };
        assert_eq!(evil.prefix(), "_/_/ok");
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
}
