//! Content-addressed **host inventory**: which images (by per-arch digest) and
//! warm-volume seeds are present on each host. This single map is what turns
//! re-provisioning into an `image exists` check instead of a transfer. Rows are
//! persisted in `host_inventory` (see [`crate::host_db`]) but are HINTS — the
//! fast path re-verifies on the host once the probe TTL lapses; boot always
//! goes through a digest check.

use crate::host::{Arch, HostId};
use crate::image::Digest;

/// What kind of artifact an inventory row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ArtifactKind {
    Image,
    Volume,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::Image => "image",
            ArtifactKind::Volume => "volume",
        }
    }
    pub fn parse(s: &str) -> Option<ArtifactKind> {
        match s {
            "image" => Some(ArtifactKind::Image),
            "volume" => Some(ArtifactKind::Volume),
            _ => None,
        }
    }
}

/// The identity of one artifact on one host. Images key on the **per-arch**
/// image digest (never the manifest-list digest: the list names a set, the
/// arch digest names the bytes on THIS host); volumes key on their seed hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InventoryKey {
    pub host: HostId,
    pub kind: ArtifactKind,
    pub digest: Digest,
    pub arch: Arch,
}

/// One present artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct InventoryEntry {
    pub key: InventoryKey,
    /// Friendly handle: `ghcr.io/x/base:v1` for images, the volume name (or a
    /// provider checkpoint/snapshot id) for volumes.
    pub ref_name: String,
    /// Unix seconds when the artifact landed.
    pub present_at: i64,
    /// Unix seconds of the last on-host verification (`image exists` by
    /// digest); `None` ⇒ never re-verified since delivery.
    pub verified_at: Option<i64>,
    pub size_bytes: Option<u64>,
}

/// Pure reconciliation: which of `wanted` the host still has to acquire.
/// Order-preserving over `wanted`; duplicates in `wanted` collapse.
pub fn missing(wanted: &[InventoryKey], present: &[InventoryEntry]) -> Vec<InventoryKey> {
    let mut out: Vec<InventoryKey> = Vec::new();
    for k in wanted {
        if present.iter().any(|e| &e.key == k) || out.contains(k) {
            continue;
        }
        out.push(k.clone());
    }
    out
}

/// Staleness: a Ready host whose artifact was last verified longer ago than
/// `ttl_secs` must be re-verified (a cheap on-host digest check) before the
/// fast path may trust it. Falls back to `present_at` when never verified.
pub fn needs_reverify(entry: &InventoryEntry, now: i64, ttl_secs: i64) -> bool {
    let last = entry.verified_at.unwrap_or(entry.present_at);
    now.saturating_sub(last) > ttl_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(c: char) -> Digest {
        Digest::parse(&format!("sha256:{}", c.to_string().repeat(64))).unwrap()
    }

    fn key(c: char) -> InventoryKey {
        InventoryKey {
            host: HostId::named("box"),
            kind: ArtifactKind::Image,
            digest: digest(c),
            arch: Arch::Amd64,
        }
    }

    fn entry(c: char, present_at: i64, verified_at: Option<i64>) -> InventoryEntry {
        InventoryEntry {
            key: key(c),
            ref_name: "ghcr.io/x/base:v1".into(),
            present_at,
            verified_at,
            size_bytes: Some(1),
        }
    }

    #[test]
    fn missing_reports_only_absent_keys() {
        let wanted = vec![key('1'), key('2'), key('2'), key('3')];
        let present = vec![entry('2', 10, None)];
        let got = missing(&wanted, &present);
        assert_eq!(got, vec![key('1'), key('3')], "present + dup dropped");
        assert!(missing(&[], &present).is_empty());
        assert_eq!(missing(&wanted, &[]).len(), 3);
    }

    #[test]
    fn missing_distinguishes_arch_and_kind() {
        let mut arm = key('1');
        arm.arch = Arch::Arm64;
        let mut vol = key('1');
        vol.kind = ArtifactKind::Volume;
        let present = vec![entry('1', 10, None)];
        assert_eq!(missing(&[arm.clone()], &present), vec![arm]);
        assert_eq!(missing(&[vol.clone()], &present), vec![vol]);
    }

    #[test]
    fn reverify_uses_verified_then_present() {
        let fresh = entry('1', 0, Some(90));
        assert!(!needs_reverify(&fresh, 100, 60), "verified 10s ago");
        let stale = entry('1', 0, Some(10));
        assert!(needs_reverify(&stale, 100, 60), "verified 90s ago");
        let never = entry('1', 95, None);
        assert!(!needs_reverify(&never, 100, 60), "delivered 5s ago");
        let old = entry('1', 10, None);
        assert!(needs_reverify(&old, 100, 60));
    }

    #[test]
    fn artifact_kind_round_trips() {
        for k in [ArtifactKind::Image, ArtifactKind::Volume] {
            assert_eq!(ArtifactKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(ArtifactKind::parse("blob"), None);
    }
}
