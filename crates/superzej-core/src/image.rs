//! The multi-arch **image model** for host provisioning: digests, image
//! references, manifest-list (image index) resolution to per-arch digests, and
//! pure delivery-strategy selection. The base image is published as an
//! amd64+arm64 manifest list; a host's inventory and the sandbox spec pin the
//! **per-arch** digest (the bytes that actually exist on that host), while the
//! list digest is provenance.

use serde::{Deserialize, Serialize};

use crate::host::{Arch, DeliveryCap, HostCaps};

/// The built-in default base image (overridden per-host by `[host.<n>] image`).
pub const DEFAULT_BASE_IMAGE: &str = "ghcr.io/superzej/superzej-sandbox:v1";
/// Pinned manifest-list digest for [`DEFAULT_BASE_IMAGE`], bumped by the image
/// publish workflow. Empty until the first publish — an unpinned ref then
/// resolves its digest at `ImageResolving` time.
pub const DEFAULT_BASE_DIGEST: &str = "";

/// A validated `sha256:<64 hex>` content digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    pub fn parse(s: &str) -> Result<Digest, String> {
        let folded = s.trim().to_ascii_lowercase();
        let hex = folded
            .strip_prefix("sha256:")
            .ok_or_else(|| format!("digest {s:?} must start with sha256:"))?;
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("digest {s:?} is not sha256:<64 hex>"));
        }
        Ok(Digest(folded))
    }
    /// Build from a raw 64-hex payload (e.g. `sha256sum` output).
    pub fn from_hex(hex: &str) -> Result<Digest, String> {
        Digest::parse(&format!("sha256:{}", hex.trim()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// The 12-char short form for labels/rows.
    pub fn short(&self) -> &str {
        &self.0["sha256:".len().."sha256:".len() + 12]
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A parsed image reference: `name[:tag][@sha256:<list digest>]`. The optional
/// digest pins the manifest list; per-arch digests come from resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub name: String,
    /// Empty ⇒ `latest` semantics are left to the runtime; superzej always
    /// writes explicit tags for its own base.
    pub tag: String,
    pub manifest_list_digest: Option<Digest>,
}

impl ImageRef {
    pub fn parse(s: &str) -> Result<ImageRef, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty image reference".into());
        }
        let (rest, digest) = match s.split_once('@') {
            Some((r, d)) => (r, Some(Digest::parse(d)?)),
            None => (s, None),
        };
        // The tag separator is a ':' AFTER the last '/', so registry ports
        // (`reg:5000/img`) survive.
        let (name, tag) = match rest.rfind(':') {
            Some(i) if i > rest.rfind('/').map_or(0, |j| j) => {
                (&rest[..i], rest[i + 1..].to_string())
            }
            _ => (rest, String::new()),
        };
        if name.is_empty() {
            return Err(format!("image reference {s:?} has no name"));
        }
        Ok(ImageRef {
            name: name.to_string(),
            tag,
            manifest_list_digest: digest,
        })
    }

    /// The default base image ref (digest-pinned once a publish has stamped
    /// [`DEFAULT_BASE_DIGEST`]).
    pub fn default_base() -> ImageRef {
        let mut r = ImageRef::parse(DEFAULT_BASE_IMAGE).expect("default base ref parses");
        if !DEFAULT_BASE_DIGEST.is_empty() {
            r.manifest_list_digest = Digest::parse(DEFAULT_BASE_DIGEST).ok();
        }
        r
    }

    /// `name:tag` (the resolvable form, digest dropped).
    pub fn name_tag(&self) -> String {
        if self.tag.is_empty() {
            self.name.clone()
        } else {
            format!("{}:{}", self.name, self.tag)
        }
    }

    /// `name@sha256:<list digest>` when pinned.
    pub fn pinned(&self) -> Option<String> {
        self.manifest_list_digest
            .as_ref()
            .map(|d| format!("{}@{}", self.name, d))
    }
}

impl std::fmt::Display for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.manifest_list_digest {
            Some(d) => write!(f, "{}@{}", self.name_tag(), d),
            None => f.write_str(&self.name_tag()),
        }
    }
}

/// The resolved multi-arch picture of one image: its manifest-list digest and
/// the per-arch image digests a host actually stores.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedImage {
    pub reference: ImageRef,
    pub list_digest: Digest,
    pub per_arch: std::collections::BTreeMap<Arch, Digest>,
}

impl ResolvedImage {
    /// Parse the raw manifest JSON fetched for `reference` (`skopeo inspect
    /// --raw` / `podman manifest inspect`). Handles the OCI image index and
    /// Docker manifest list (a `manifests` array with per-platform digests —
    /// unknown/attestation platforms are skipped), and falls back to treating a
    /// plain single manifest as `{fallback_arch: self_digest}`. `self_digest`
    /// is the digest OF the fetched document (the registry's `Docker-Content-
    /// Digest` / skopeo's `Digest` field) — a manifest never contains its own.
    pub fn parse_manifest_index(
        reference: &ImageRef,
        json: &str,
        self_digest: Digest,
        fallback_arch: Arch,
    ) -> Result<ResolvedImage, String> {
        let v: serde_json::Value =
            serde_json::from_str(json).map_err(|e| format!("manifest json: {e}"))?;
        let mut per_arch = std::collections::BTreeMap::new();
        match v.get("manifests").and_then(|m| m.as_array()) {
            Some(entries) => {
                for entry in entries {
                    let Some(digest) = entry.get("digest").and_then(|d| d.as_str()) else {
                        continue;
                    };
                    let Some(platform) = entry.get("platform") else {
                        continue;
                    };
                    let os = platform.get("os").and_then(|o| o.as_str()).unwrap_or("");
                    // Skip attestation manifests (buildkit emits os="unknown")
                    // and non-linux entries.
                    if os != "linux" {
                        continue;
                    }
                    let Some(arch) = platform
                        .get("architecture")
                        .and_then(|a| a.as_str())
                        .and_then(Arch::parse)
                    else {
                        continue;
                    };
                    let d = Digest::parse(digest)?;
                    per_arch.entry(arch).or_insert(d);
                }
                if per_arch.is_empty() {
                    return Err(format!(
                        "manifest list for {reference} has no usable linux platforms"
                    ));
                }
            }
            None => {
                // A plain single-arch manifest: the document's own digest IS the
                // image digest.
                per_arch.insert(fallback_arch, self_digest.clone());
            }
        }
        Ok(ResolvedImage {
            reference: reference.clone(),
            list_digest: self_digest,
            per_arch,
        })
    }

    pub fn digest_for(&self, arch: Arch) -> Option<&Digest> {
        self.per_arch.get(&arch)
    }

    /// What the sandbox spec pins to: `name@sha256:<PER-ARCH digest>` — the
    /// existing `image exists` prefetch then digest-verifies for free.
    pub fn spec_image(&self, arch: Arch) -> Option<String> {
        self.per_arch
            .get(&arch)
            .map(|d| format!("{}@{}", self.reference.name, d))
    }
}

/// The content-addressed LOCAL tag a delivered base image is registered under
/// on every host (`localhost/superzej/base:<digest12>`). Registry pulls keep
/// their `name@digest` association too, but stream-delivered (`podman load`)
/// images don't get one — this tag is the uniform, digest-derived run
/// reference for both, and the GC handle alongside `superzej.managed` labels.
pub fn managed_tag(digest: &Digest) -> String {
    format!("localhost/superzej/base:{}", digest.short())
}

/// What the LOCAL side can do for delivery (probed cheaply on this machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalCaps {
    /// A local container storage exists to `save` the base image from.
    pub has_podman: bool,
    pub has_skopeo: bool,
    pub has_rsync: bool,
    /// This machine can reach the registry (to resolve/pull before streaming).
    pub has_registry_egress: bool,
}

/// A concrete, ordered delivery plan candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStrategy {
    /// Stage a content-addressed oci-archive locally, stream it over the
    /// control channel with offset resume, verify, `podman load`.
    SshStream { rsync: bool },
    /// The host pulls by digest itself (`podman pull name@sha256:…`).
    RegistryPull,
    /// The host's skopeo copies from the registry into containers-storage.
    SkopeoRemoteCopy,
    /// Ship the Containerfile/context and build on the host — last resort.
    RemoteBuild,
    /// Cloud lowering: register the image as the provider's template/checkpoint.
    ProviderTemplate,
}

impl DeliveryStrategy {
    /// The capability a config preference name selects.
    fn cap(self) -> DeliveryCap {
        match self {
            DeliveryStrategy::SshStream { rsync: true } => DeliveryCap::Rsync,
            DeliveryStrategy::SshStream { rsync: false } => DeliveryCap::SshStream,
            DeliveryStrategy::RegistryPull => DeliveryCap::RegistryPull,
            DeliveryStrategy::SkopeoRemoteCopy => DeliveryCap::SkopeoRemote,
            DeliveryStrategy::RemoteBuild => DeliveryCap::RemoteBuild,
            DeliveryStrategy::ProviderTemplate => DeliveryCap::ProviderTemplate,
        }
    }
    /// Short name for events/logs.
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryStrategy::SshStream { rsync: true } => "ssh-stream+rsync",
            DeliveryStrategy::SshStream { rsync: false } => "ssh-stream",
            DeliveryStrategy::RegistryPull => "registry-pull",
            DeliveryStrategy::SkopeoRemoteCopy => "skopeo-copy",
            DeliveryStrategy::RemoteBuild => "remote-build",
            DeliveryStrategy::ProviderTemplate => "provider-template",
        }
    }
}

/// Rank the delivery strategies for a (local, host) pair: capabilities FILTER,
/// preferences REORDER. The registry-less transfer is the default happy path
/// (`SshStream` with rsync when both ends have it), registry pull and skopeo
/// are alternates, remote build is last. A cloud host (`ProviderTemplate` cap)
/// lowers to the provider template exclusively. Pure and table-tested.
pub fn select_delivery(
    local: &LocalCaps,
    host: &HostCaps,
    prefs: &[DeliveryCap],
) -> Vec<DeliveryStrategy> {
    if host.delivery.contains(&DeliveryCap::ProviderTemplate) {
        return vec![DeliveryStrategy::ProviderTemplate];
    }
    let can_stage_archive = local.has_podman || local.has_skopeo;
    let mut ranked: Vec<DeliveryStrategy> = Vec::new();
    if host.delivery.contains(&DeliveryCap::SshStream) && can_stage_archive {
        if host.delivery.contains(&DeliveryCap::Rsync) && local.has_rsync {
            ranked.push(DeliveryStrategy::SshStream { rsync: true });
        }
        ranked.push(DeliveryStrategy::SshStream { rsync: false });
    }
    if host.delivery.contains(&DeliveryCap::RegistryPull) {
        ranked.push(DeliveryStrategy::RegistryPull);
    }
    if host.delivery.contains(&DeliveryCap::SkopeoRemote) {
        ranked.push(DeliveryStrategy::SkopeoRemoteCopy);
    }
    if host.delivery.contains(&DeliveryCap::RemoteBuild) {
        ranked.push(DeliveryStrategy::RemoteBuild);
    }
    if prefs.is_empty() {
        return ranked;
    }
    // Stable reorder: preferred strategies first (in preference order), the
    // rest keep their default relative order as fallbacks.
    let mut preferred: Vec<DeliveryStrategy> = Vec::new();
    for p in prefs {
        for s in &ranked {
            if s.cap() == *p && !preferred.contains(s) {
                preferred.push(*s);
            }
        }
    }
    for s in ranked {
        if !preferred.contains(&s) {
            preferred.push(s);
        }
    }
    preferred
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const DL: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn digest_validates() {
        let d = Digest::parse(D1).unwrap();
        assert_eq!(d.as_str(), D1);
        assert_eq!(d.short(), "111111111111");
        assert!(Digest::parse("sha256:short").is_err());
        assert!(Digest::parse("md5:abc").is_err());
        assert!(Digest::parse(&D1.to_uppercase()).is_ok(), "case-folded");
        assert_eq!(
            Digest::from_hex(&D1["sha256:".len()..]).unwrap().as_str(),
            D1
        );
    }

    #[test]
    fn image_ref_parse_forms() {
        let plain = ImageRef::parse("ghcr.io/x/base").unwrap();
        assert_eq!(
            (plain.name.as_str(), plain.tag.as_str()),
            ("ghcr.io/x/base", "")
        );
        assert!(plain.manifest_list_digest.is_none());

        let tagged = ImageRef::parse("ghcr.io/x/base:v1").unwrap();
        assert_eq!(tagged.tag, "v1");
        assert_eq!(tagged.name_tag(), "ghcr.io/x/base:v1");

        let pinned = ImageRef::parse(&format!("ghcr.io/x/base:v1@{DL}")).unwrap();
        assert_eq!(pinned.tag, "v1");
        assert_eq!(pinned.pinned(), Some(format!("ghcr.io/x/base@{DL}")));
        assert_eq!(pinned.to_string(), format!("ghcr.io/x/base:v1@{DL}"));

        // Registry ports are not tags.
        let ported = ImageRef::parse("reg.local:5000/base").unwrap();
        assert_eq!(ported.name, "reg.local:5000/base");
        assert_eq!(ported.tag, "");
        let ported_tag = ImageRef::parse("reg.local:5000/base:v2").unwrap();
        assert_eq!(ported_tag.name, "reg.local:5000/base");
        assert_eq!(ported_tag.tag, "v2");

        assert!(ImageRef::parse("").is_err());
        assert!(ImageRef::parse(&format!("@{DL}")).is_err());
        assert!(ImageRef::parse("img@sha256:junk").is_err());
    }

    #[test]
    fn default_base_parses() {
        let r = ImageRef::default_base();
        assert!(!r.name.is_empty());
        assert!(!r.tag.is_empty());
    }

    #[test]
    fn oci_index_resolves_per_arch() {
        let json = format!(
            r#"{{
              "schemaVersion": 2,
              "mediaType": "application/vnd.oci.image.index.v1+json",
              "manifests": [
                {{"digest": "{D1}", "platform": {{"architecture": "amd64", "os": "linux"}}}},
                {{"digest": "{D2}", "platform": {{"architecture": "arm64", "os": "linux"}}}},
                {{"digest": "{DL}", "platform": {{"architecture": "unknown", "os": "unknown"}}}}
              ]
            }}"#
        );
        let r = ResolvedImage::parse_manifest_index(
            &ImageRef::parse("ghcr.io/x/base:v1").unwrap(),
            &json,
            Digest::parse(DL).unwrap(),
            Arch::Amd64,
        )
        .unwrap();
        assert_eq!(r.list_digest.as_str(), DL);
        assert_eq!(r.digest_for(Arch::Amd64).unwrap().as_str(), D1);
        assert_eq!(r.digest_for(Arch::Arm64).unwrap().as_str(), D2);
        assert_eq!(
            r.spec_image(Arch::Arm64).unwrap(),
            format!("ghcr.io/x/base@{D2}")
        );
        assert_eq!(r.per_arch.len(), 2, "attestation entry skipped");
    }

    #[test]
    fn docker_manifest_list_resolves() {
        let json = format!(
            r#"{{
              "schemaVersion": 2,
              "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
              "manifests": [
                {{"digest": "{D1}", "platform": {{"architecture": "amd64", "os": "linux"}}}},
                {{"digest": "{D2}", "platform": {{"architecture": "arm", "os": "linux"}}}}
              ]
            }}"#
        );
        let r = ResolvedImage::parse_manifest_index(
            &ImageRef::parse("x/y:z").unwrap(),
            &json,
            Digest::parse(DL).unwrap(),
            Arch::Amd64,
        )
        .unwrap();
        assert_eq!(r.per_arch.len(), 1, "unsupported arm(v7) skipped");
        assert_eq!(r.digest_for(Arch::Amd64).unwrap().as_str(), D1);
    }

    #[test]
    fn plain_manifest_falls_back_to_self_digest() {
        let json = r#"{"schemaVersion": 2, "config": {"digest": "sha256:beef"}, "layers": []}"#;
        let r = ResolvedImage::parse_manifest_index(
            &ImageRef::parse("x/y:z").unwrap(),
            json,
            Digest::parse(D1).unwrap(),
            Arch::Arm64,
        )
        .unwrap();
        assert_eq!(r.digest_for(Arch::Arm64).unwrap().as_str(), D1);
        assert_eq!(r.digest_for(Arch::Amd64), None);
    }

    #[test]
    fn index_with_only_alien_platforms_errors() {
        let json = format!(
            r#"{{"manifests": [{{"digest": "{D1}", "platform": {{"architecture": "riscv64", "os": "linux"}}}}]}}"#
        );
        assert!(
            ResolvedImage::parse_manifest_index(
                &ImageRef::parse("x/y").unwrap(),
                &json,
                Digest::parse(DL).unwrap(),
                Arch::Amd64,
            )
            .is_err()
        );
        assert!(
            ResolvedImage::parse_manifest_index(
                &ImageRef::parse("x/y").unwrap(),
                "not json",
                Digest::parse(DL).unwrap(),
                Arch::Amd64,
            )
            .is_err()
        );
    }

    fn host_caps(caps: &[DeliveryCap]) -> HostCaps {
        HostCaps {
            arch: Arch::Amd64,
            os: "linux".into(),
            runtime: None,
            delivery: caps.iter().copied().collect::<BTreeSet<_>>(),
            egress: Default::default(),
            can_install_runtime: false,
            disk_free_bytes: None,
            has_nix: false,
            cgroup_v2: false,
            userns: false,
            nproc: None,
            mem_total_kb: None,
        }
    }

    const FULL_LOCAL: LocalCaps = LocalCaps {
        has_podman: true,
        has_skopeo: true,
        has_rsync: true,
        has_registry_egress: true,
    };

    #[test]
    fn selection_default_rank_is_registry_less_first() {
        let host = host_caps(&[
            DeliveryCap::SshStream,
            DeliveryCap::Rsync,
            DeliveryCap::RegistryPull,
            DeliveryCap::SkopeoRemote,
            DeliveryCap::RemoteBuild,
        ]);
        assert_eq!(
            select_delivery(&FULL_LOCAL, &host, &[]),
            vec![
                DeliveryStrategy::SshStream { rsync: true },
                DeliveryStrategy::SshStream { rsync: false },
                DeliveryStrategy::RegistryPull,
                DeliveryStrategy::SkopeoRemoteCopy,
                DeliveryStrategy::RemoteBuild,
            ]
        );
    }

    #[test]
    fn selection_filters_by_caps() {
        // No local archive producer ⇒ no SshStream.
        let host = host_caps(&[DeliveryCap::SshStream, DeliveryCap::RegistryPull]);
        let no_local = LocalCaps::default();
        assert_eq!(
            select_delivery(&no_local, &host, &[]),
            vec![DeliveryStrategy::RegistryPull]
        );
        // rsync needs BOTH ends.
        let host_rsync = host_caps(&[DeliveryCap::SshStream, DeliveryCap::Rsync]);
        let local_no_rsync = LocalCaps {
            has_podman: true,
            ..LocalCaps::default()
        };
        assert_eq!(
            select_delivery(&local_no_rsync, &host_rsync, &[]),
            vec![DeliveryStrategy::SshStream { rsync: false }]
        );
    }

    #[test]
    fn selection_prefs_reorder_but_keep_fallbacks() {
        let host = host_caps(&[
            DeliveryCap::SshStream,
            DeliveryCap::RegistryPull,
            DeliveryCap::SkopeoRemote,
        ]);
        let got = select_delivery(
            &FULL_LOCAL,
            &host,
            &[DeliveryCap::RegistryPull, DeliveryCap::SkopeoRemote],
        );
        assert_eq!(
            got,
            vec![
                DeliveryStrategy::RegistryPull,
                DeliveryStrategy::SkopeoRemoteCopy,
                DeliveryStrategy::SshStream { rsync: false },
            ]
        );
    }

    #[test]
    fn selection_cloud_is_exclusive() {
        let host = HostCaps::cloud_managed(Arch::Amd64);
        assert_eq!(
            select_delivery(&FULL_LOCAL, &host, &[DeliveryCap::RegistryPull]),
            vec![DeliveryStrategy::ProviderTemplate]
        );
    }

    #[test]
    fn managed_tag_is_digest_derived() {
        let d = Digest::parse(D1).unwrap();
        assert_eq!(managed_tag(&d), "localhost/superzej/base:111111111111");
    }

    #[test]
    fn strategy_names() {
        assert_eq!(
            DeliveryStrategy::SshStream { rsync: true }.as_str(),
            "ssh-stream+rsync"
        );
        assert_eq!(DeliveryStrategy::RemoteBuild.as_str(), "remote-build");
    }
}
