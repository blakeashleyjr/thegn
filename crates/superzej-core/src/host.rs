//! **Hosts** — machines that can run OCI containers, provisioned ONCE (connect →
//! probe → ensure runtime → ensure base image by digest → seed warm volumes →
//! Ready) and then shared by every worktree sandbox that lands on them. This
//! module is the pure vocabulary: identity, reach, probed capabilities, and the
//! spec-injection applied on the per-worktree fast path. The state machine lives
//! in [`crate::host_machine`]; digest/image types in [`crate::image`]; persisted
//! state in [`crate::host_db`]; all I/O (channels, probing, delivery) in the
//! service crate.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::image::Digest;
use crate::placement::SshPlacement;

/// Stable identity of a host across restarts. Canonical string forms:
/// `local`, `host:<config-name>`, `anon-ssh:<user@host>:<port>` (implicit host
/// derived from an inline `[env.*.ssh]`), `cloud:<provider>/<template>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostId(String);

impl HostId {
    pub fn local() -> HostId {
        HostId("local".into())
    }
    /// A named `[host.<name>]` config entry.
    pub fn named(name: &str) -> HostId {
        HostId(format!("host:{name}"))
    }
    /// The implicit anonymous host behind an inline `[env.*.ssh]` target. Two
    /// envs pointing at the same `user@host:port` share one host record.
    pub fn anon_ssh(target: &str, port: u16) -> HostId {
        HostId(format!("anon-ssh:{target}:{port}"))
    }
    /// A cloud-managed host: provisioning lowers to the provider's template /
    /// checkpoint primitive.
    pub fn cloud(provider: &str, template: &str) -> HostId {
        HostId(format!("cloud:{provider}/{template}"))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Round-trip a canonical string from the DB. `None` for junk.
    pub fn parse(s: &str) -> Option<HostId> {
        let ok = s == "local"
            || s.strip_prefix("host:").is_some_and(|r| !r.is_empty())
            || s.strip_prefix("anon-ssh:").is_some_and(|r| !r.is_empty())
            || s.strip_prefix("cloud:")
                .is_some_and(|r| r.contains('/') && !r.starts_with('/'));
        ok.then(|| HostId(s.to_string()))
    }
    /// The `[host.<name>]` config name, when this is a named host.
    pub fn config_name(&self) -> Option<&str> {
        self.0.strip_prefix("host:")
    }
    pub fn is_local(&self) -> bool {
        self.0 == "local"
    }
    pub fn is_cloud(&self) -> bool {
        self.0.starts_with("cloud:")
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How superzej reaches a host. `Iroh` is lowered to `Ssh` over a local
/// forwarded port by the connector before anything else sees it — probe,
/// install, delivery, and spawn are byte-identical to the SSH path.
#[derive(Debug, Clone)]
pub enum Reach {
    Local,
    Ssh(SshPlacement),
    Iroh(IrohReach),
    Cloud(CloudReach),
}

impl Reach {
    /// Terse kind for chips/rows/DB (`local`/`ssh`/`iroh`/`cloud`).
    pub fn kind(&self) -> &'static str {
        match self {
            Reach::Local => "local",
            Reach::Ssh(_) => "ssh",
            Reach::Iroh(_) => "iroh",
            Reach::Cloud(_) => "cloud",
        }
    }
}

/// A NAT'd host reached over an iroh (dumbpipe) tunnel: the remote runs
/// `dumbpipe listen-tcp --host 127.0.0.1:<ssh_port>` and superzej forwards a
/// local port to it, then speaks plain ssh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrohReach {
    /// dumbpipe node ticket (secret refs `env:VAR`/`file:PATH` are expanded at
    /// binding-resolution time; this is the resolved value).
    pub ticket: String,
    /// The sshd port the remote's listener fronts (usually 22).
    pub ssh_port: u16,
    /// SSH user for the forwarded session.
    pub user: String,
}

/// A provider-managed host (Sprites, Daytona): the "connection" is an
/// authenticated API client and provisioning lowers to template registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudReach {
    pub provider: String,
    pub api_base: String,
    /// Env var holding the API token (never the token itself).
    pub api_key_env: String,
    pub template: String,
}

/// CPU architecture of a host — decides which per-arch image digest from the
/// base image's manifest list must exist there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    Amd64,
    Arm64,
}

impl Arch {
    /// Parse a `uname -m` value.
    pub fn parse_uname(m: &str) -> Option<Arch> {
        match m.trim() {
            "x86_64" | "amd64" => Some(Arch::Amd64),
            "aarch64" | "arm64" => Some(Arch::Arm64),
            _ => None,
        }
    }
    /// The OCI platform architecture name (manifest-list entries).
    pub fn oci_name(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
        }
    }
    pub fn parse(s: &str) -> Option<Arch> {
        match s {
            "amd64" => Some(Arch::Amd64),
            "arm64" => Some(Arch::Arm64),
            other => Arch::parse_uname(other),
        }
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.oci_name())
    }
}

/// Which container runtime a host offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Podman,
    Docker,
    /// Provider-managed (cloud): there is no daemon to probe or install; the
    /// provider boots images itself.
    CloudManaged,
}

impl RuntimeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeKind::Podman => "podman",
            RuntimeKind::Docker => "docker",
            RuntimeKind::CloudManaged => "cloud",
        }
    }
}

/// A probed container runtime on a host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub kind: RuntimeKind,
    pub version: String,
    pub rootless: bool,
    /// API socket path on the host, when known (feeds the remote `--url`).
    pub socket: Option<String>,
}

/// One image-delivery capability of a (local, host) pair. Strategy selection
/// ([`crate::image::select_delivery`]) filters + ranks these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryCap {
    /// Staged oci-archive streamed over the control channel (the default).
    SshStream,
    /// rsync available on both ends: sturdier resumable transfer.
    Rsync,
    /// skopeo on the LOCAL side (better per-arch archive producer).
    SkopeoLocal,
    /// skopeo on the host (remote-side registry copy).
    SkopeoRemote,
    /// The host has egress to the image registry.
    RegistryPull,
    /// The host can build the image itself (build tools + egress).
    RemoteBuild,
    /// Cloud: "delivery" = registering the image as the provider's template.
    ProviderTemplate,
}

impl DeliveryCap {
    /// Parse a config preference name (`delivery = ["ssh-stream", ...]`).
    pub fn parse(s: &str) -> Option<DeliveryCap> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ssh-stream" | "ssh_stream" | "transfer" => Some(DeliveryCap::SshStream),
            "rsync" => Some(DeliveryCap::Rsync),
            "skopeo-local" | "skopeo_local" => Some(DeliveryCap::SkopeoLocal),
            "skopeo" | "skopeo-remote" | "skopeo_remote" => Some(DeliveryCap::SkopeoRemote),
            "registry" | "registry-pull" | "pull" => Some(DeliveryCap::RegistryPull),
            "build" | "remote-build" => Some(DeliveryCap::RemoteBuild),
            "template" | "provider-template" => Some(DeliveryCap::ProviderTemplate),
            _ => None,
        }
    }
}

/// Host egress posture, as probed (a quick reachability check) — informs
/// whether `RegistryPull`/`RemoteBuild` are even candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressMode {
    #[default]
    Full,
    Restricted,
    None,
}

/// Everything the single-shot probe learns about a host. Serialized into the
/// `hosts.caps_json` column; parsed from the probe script's KEY=VALUE output by
/// [`HostCaps::parse_probe`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostCaps {
    pub arch: Arch,
    pub os: String,
    /// `None` ⇒ no runtime found: the machine walks Installing (with consent).
    pub runtime: Option<RuntimeInfo>,
    pub delivery: BTreeSet<DeliveryCap>,
    pub egress: EgressMode,
    /// A supported package manager was found, so a consented install is possible.
    pub can_install_runtime: bool,
    /// Free bytes on the container-storage partition, when probed.
    pub disk_free_bytes: Option<u64>,
    pub has_nix: bool,
    /// cgroup v2 unified hierarchy present (resource-limit fidelity).
    #[serde(default)]
    pub cgroup_v2: bool,
    /// Unprivileged user namespaces enabled (rootless-container viability).
    #[serde(default)]
    pub userns: bool,
    /// Logical CPU count, when probed (the machine-size hint).
    #[serde(default)]
    pub nproc: Option<u32>,
    /// MemTotal in KiB, when probed.
    #[serde(default)]
    pub mem_total_kb: Option<u64>,
}

impl HostCaps {
    /// Parse the probe script's `KEY=VALUE` output (one per line; unknown keys
    /// ignored so probe and core can evolve independently). The script itself
    /// lives in the service crate; this contract is the seam:
    ///
    /// ```text
    /// ARCH=x86_64            # uname -m (required)
    /// OS=linux               # uname -s, lowercased (required)
    /// PODMAN=4.9.3           # `podman --version` short form, when present
    /// PODMAN_ROOTLESS=1      # 1 when the probe user isn't running rootful
    /// PODMAN_SOCKET=/run/user/1000/podman/podman.sock
    /// DOCKER=24.0.5          # when present (podman wins when both)
    /// PKGMGR=apt             # apt|dnf|apk|pacman|none
    /// SKOPEO=1  RSYNC=1  NIX=1
    /// DISK_FREE=123456789    # bytes free on the storage partition
    /// EGRESS=full            # full|restricted|none (default full)
    /// ```
    pub fn parse_probe(out: &str) -> Result<HostCaps, String> {
        let mut kv = std::collections::BTreeMap::new();
        for line in out.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                kv.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        let arch = kv
            .get("ARCH")
            .and_then(|a| Arch::parse_uname(a))
            .ok_or_else(|| {
                format!(
                    "probe: unsupported or missing ARCH ({:?})",
                    kv.get("ARCH").map(String::as_str).unwrap_or("<none>")
                )
            })?;
        let os = kv
            .get("OS")
            .map(|s| s.to_ascii_lowercase())
            .ok_or_else(|| "probe: missing OS".to_string())?;
        let truthy = |k: &str| kv.get(k).is_some_and(|v| v == "1" || v == "true");
        let runtime = if let Some(v) = kv.get("PODMAN").filter(|v| !v.is_empty()) {
            Some(RuntimeInfo {
                kind: RuntimeKind::Podman,
                version: v.clone(),
                rootless: truthy("PODMAN_ROOTLESS"),
                socket: kv.get("PODMAN_SOCKET").filter(|s| !s.is_empty()).cloned(),
            })
        } else {
            kv.get("DOCKER")
                .filter(|v| !v.is_empty())
                .map(|v| RuntimeInfo {
                    kind: RuntimeKind::Docker,
                    version: v.clone(),
                    rootless: false,
                    socket: kv.get("DOCKER_SOCKET").filter(|s| !s.is_empty()).cloned(),
                })
        };
        let egress = match kv.get("EGRESS").map(String::as_str) {
            Some("none") => EgressMode::None,
            Some("restricted") => EgressMode::Restricted,
            _ => EgressMode::Full,
        };
        let mut delivery: BTreeSet<DeliveryCap> = BTreeSet::new();
        // The control channel that ran the probe can stream bytes, so a remote
        // probe implies SshStream; the local side decides SkopeoLocal.
        delivery.insert(DeliveryCap::SshStream);
        if truthy("RSYNC") {
            delivery.insert(DeliveryCap::Rsync);
        }
        if truthy("SKOPEO") {
            delivery.insert(DeliveryCap::SkopeoRemote);
        }
        if egress == EgressMode::Full {
            delivery.insert(DeliveryCap::RegistryPull);
            if truthy("NIX") {
                delivery.insert(DeliveryCap::RemoteBuild);
            }
        }
        let can_install_runtime = kv
            .get("PKGMGR")
            .is_some_and(|p| matches!(p.as_str(), "apt" | "dnf" | "apk" | "pacman"));
        Ok(HostCaps {
            arch,
            os,
            runtime,
            delivery,
            egress,
            can_install_runtime,
            disk_free_bytes: kv.get("DISK_FREE").and_then(|v| v.parse().ok()),
            has_nix: truthy("NIX"),
            cgroup_v2: truthy("CGROUPV2"),
            userns: truthy("USERNS"),
            nproc: kv.get("NPROC").and_then(|v| v.parse().ok()),
            mem_total_kb: kv.get("MEM_TOTAL_KB").and_then(|v| v.parse().ok()),
        })
    }

    /// Synthesized caps for a cloud-managed host: nothing to probe or install;
    /// delivery is the provider's template primitive.
    pub fn cloud_managed(arch: Arch) -> HostCaps {
        HostCaps {
            arch,
            os: "linux".into(),
            runtime: Some(RuntimeInfo {
                kind: RuntimeKind::CloudManaged,
                version: String::new(),
                rootless: false,
                socket: None,
            }),
            delivery: BTreeSet::from([DeliveryCap::ProviderTemplate]),
            egress: EgressMode::Full,
            can_install_runtime: false,
            disk_free_bytes: None,
            has_nix: false,
            cgroup_v2: false,
            userns: false,
            nproc: None,
            mem_total_kb: None,
        }
    }
}

/// Managed warm-volume names (labelled `superzej.managed=true` on the host).
pub const VOLUME_NIX_STORE: &str = "superzej-nix-store";
pub const VOLUME_CARGO: &str = "superzej-cargo";

/// A warm named volume seeded once per host and mounted into every sandbox.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeSpec {
    pub name: String,
    /// Mount destination inside the sandbox.
    pub dest: String,
    pub seed: VolumeSeed,
}

/// How a warm volume gets its initial content.
#[derive(Debug, Clone, PartialEq)]
pub enum VolumeSeed {
    /// podman/docker copy-up: first mount at a path the base image populates
    /// copies the image content in — zero extra transfer; the image IS the seed.
    ImageCopyUp,
    /// `podman volume import` of a tarball delivered via the same resumable
    /// staged transfer as images; inventory-keyed by the tarball's digest.
    Tarball { digest: Digest },
}

impl VolumeSpec {
    /// The standard warm set for a config `volumes = [...]` list; unknown names
    /// are skipped (the config layer warns).
    pub fn from_names(names: &[String]) -> Vec<VolumeSpec> {
        names.iter().filter_map(|n| Self::by_name(n)).collect()
    }
    pub fn by_name(name: &str) -> Option<VolumeSpec> {
        match name {
            "nix-store" | VOLUME_NIX_STORE => Some(VolumeSpec {
                name: VOLUME_NIX_STORE.into(),
                dest: "/nix".into(),
                seed: VolumeSeed::ImageCopyUp,
            }),
            "cargo" | VOLUME_CARGO => Some(VolumeSpec {
                name: VOLUME_CARGO.into(),
                dest: "/home/superzej/.cargo".into(),
                seed: VolumeSeed::ImageCopyUp,
            }),
            _ => None,
        }
    }
}

/// A host provisioning failure: the step it died on, an actionable message, and
/// whether a plain retry can succeed (vs needing a user action — consent, config,
/// cache reset). This is the shape every UI/CLI surface renders (and the
/// `hosts.state_meta` JSON for a persisted `failed` state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFailure {
    pub step: HostStep,
    pub error: String,
    pub retryable: bool,
}

impl std::fmt::Display for HostFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step.as_str(), self.error)
    }
}

impl std::error::Error for HostFailure {}

/// The stable step taxonomy used by `Failed{}`, the DB event trail, and UI
/// labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostStep {
    Connect,
    Probe,
    Consent,
    Install,
    ResolveImage,
    Deliver,
    SeedVolume,
    Verify,
}

impl HostStep {
    pub fn as_str(self) -> &'static str {
        match self {
            HostStep::Connect => "connect",
            HostStep::Probe => "probe",
            HostStep::Consent => "consent",
            HostStep::Install => "install",
            HostStep::ResolveImage => "resolve_image",
            HostStep::Deliver => "deliver",
            HostStep::SeedVolume => "seed_volume",
            HostStep::Verify => "verify",
        }
    }
    pub fn parse(s: &str) -> Option<HostStep> {
        Some(match s {
            "connect" => HostStep::Connect,
            "probe" => HostStep::Probe,
            "consent" => HostStep::Consent,
            "install" => HostStep::Install,
            "resolve_image" => HostStep::ResolveImage,
            "deliver" => HostStep::Deliver,
            "seed_volume" => HostStep::SeedVolume,
            "verify" => HostStep::Verify,
            _ => return None,
        })
    }
    /// Human label for splash steps / panel rows.
    pub fn label(self) -> &'static str {
        match self {
            HostStep::Connect => "connect",
            HostStep::Probe => "probe runtime",
            HostStep::Consent => "install consent",
            HostStep::Install => "install runtime",
            HostStep::ResolveImage => "resolve image",
            HostStep::Deliver => "transfer image",
            HostStep::SeedVolume => "warm volumes",
            HostStep::Verify => "verify",
        }
    }
}

/// What a `Ready` host injects into a worktree's
/// [`SandboxSpec`](crate::sandbox::SandboxSpec) on the fast path: digest-pinned image, warm
/// volumes, and the remote OCI daemon URL. Pure data so the host crate can
/// build it off-loop and apply it at spec time.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReadyHostSpec {
    /// `name@sha256:<per-arch digest>` — makes the existing `image exists`
    /// prefetch a digest-verified no-op.
    pub image: String,
    /// Remote daemon URL for `[sandbox] oci_host`-style wrapping (`None` = local).
    pub oci_url: Option<String>,
    /// Named warm volumes as `(name, dest)` pairs.
    pub volumes: Vec<(String, String)>,
    /// Pane-entry hook (e.g. eval the synthesized devshell) prepended to the
    /// sandbox `init_script` when the spec has none of its own.
    pub init_script: Option<String>,
}

/// Inject a Ready host's assets into a sandbox spec. Existing explicit values
/// win: a user-set `spec.image`/`oci_host` is respected, and volume mounts
/// already present (by destination) are not duplicated.
pub fn apply_ready_host(spec: &mut crate::sandbox::SandboxSpec, rh: &ReadyHostSpec) {
    if spec.image.is_none() && !rh.image.is_empty() {
        spec.image = Some(rh.image.clone());
    }
    if spec.oci_host.is_none() {
        spec.oci_host = rh.oci_url.clone();
    }
    for (name, dest) in &rh.volumes {
        if !spec.volumes.iter().any(|(_, d)| d == dest) {
            spec.volumes.push((name.clone(), dest.clone()));
        }
    }
    if spec.init_script.is_none() {
        spec.init_script = rh.init_script.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_id_canonical_forms_round_trip() {
        for id in [
            HostId::local(),
            HostId::named("gpu-box"),
            HostId::anon_ssh("blake@gpu.internal", 22),
            HostId::cloud("sprites", "superzej-base"),
        ] {
            assert_eq!(HostId::parse(id.as_str()), Some(id.clone()), "{id}");
        }
        assert_eq!(HostId::named("gpu-box").config_name(), Some("gpu-box"));
        assert!(HostId::local().is_local());
        assert!(HostId::cloud("sprites", "t").is_cloud());
        assert!(!HostId::named("x").is_local());
    }

    #[test]
    fn host_id_rejects_junk() {
        for junk in ["", "host:", "anon-ssh:", "cloud:", "cloud:/x", "wat:x"] {
            assert_eq!(HostId::parse(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn anon_id_is_stable_for_same_target() {
        assert_eq!(
            HostId::anon_ssh("blake@box", 22),
            HostId::anon_ssh("blake@box", 22)
        );
        assert_ne!(
            HostId::anon_ssh("blake@box", 22),
            HostId::anon_ssh("blake@box", 2222)
        );
    }

    #[test]
    fn arch_parses_uname_and_oci_names() {
        assert_eq!(Arch::parse_uname("x86_64"), Some(Arch::Amd64));
        assert_eq!(Arch::parse_uname("aarch64"), Some(Arch::Arm64));
        assert_eq!(Arch::parse_uname(" arm64 "), Some(Arch::Arm64));
        assert_eq!(Arch::parse_uname("riscv64"), None);
        assert_eq!(Arch::parse("amd64"), Some(Arch::Amd64));
        assert_eq!(Arch::Amd64.oci_name(), "amd64");
        assert_eq!(Arch::Arm64.to_string(), "arm64");
    }

    #[test]
    fn probe_parse_podman_present() {
        let caps = HostCaps::parse_probe(
            "ARCH=x86_64\nOS=Linux\nPODMAN=4.9.3\nPODMAN_ROOTLESS=1\n\
             PODMAN_SOCKET=/run/user/1000/podman/podman.sock\nPKGMGR=apt\n\
             SKOPEO=1\nRSYNC=1\nNIX=1\nDISK_FREE=99999\nEGRESS=full\n",
        )
        .unwrap();
        assert_eq!(caps.arch, Arch::Amd64);
        assert_eq!(caps.os, "linux");
        let rt = caps.runtime.as_ref().unwrap();
        assert_eq!(rt.kind, RuntimeKind::Podman);
        assert_eq!(rt.version, "4.9.3");
        assert!(rt.rootless);
        assert_eq!(
            rt.socket.as_deref(),
            Some("/run/user/1000/podman/podman.sock")
        );
        assert!(caps.can_install_runtime);
        assert!(caps.has_nix);
        assert_eq!(caps.disk_free_bytes, Some(99999));
        for cap in [
            DeliveryCap::SshStream,
            DeliveryCap::Rsync,
            DeliveryCap::SkopeoRemote,
            DeliveryCap::RegistryPull,
            DeliveryCap::RemoteBuild,
        ] {
            assert!(caps.delivery.contains(&cap), "{cap:?}");
        }
    }

    #[test]
    fn probe_parse_docker_only_and_no_runtime() {
        let docker =
            HostCaps::parse_probe("ARCH=aarch64\nOS=linux\nDOCKER=24.0.5\nPKGMGR=none\n").unwrap();
        assert_eq!(docker.runtime.as_ref().unwrap().kind, RuntimeKind::Docker);
        assert!(!docker.can_install_runtime);

        let bare = HostCaps::parse_probe("ARCH=x86_64\nOS=linux\nPKGMGR=dnf\n").unwrap();
        assert!(bare.runtime.is_none());
        assert!(bare.can_install_runtime, "dnf ⇒ installable");
    }

    #[test]
    fn probe_parse_no_egress_drops_pull_and_build() {
        let caps = HostCaps::parse_probe("ARCH=x86_64\nOS=linux\nPODMAN=5.0\nNIX=1\nEGRESS=none\n")
            .unwrap();
        assert_eq!(caps.egress, EgressMode::None);
        assert!(!caps.delivery.contains(&DeliveryCap::RegistryPull));
        assert!(!caps.delivery.contains(&DeliveryCap::RemoteBuild));
        assert!(caps.delivery.contains(&DeliveryCap::SshStream));
    }

    #[test]
    fn probe_parse_rejects_missing_or_alien_arch() {
        assert!(HostCaps::parse_probe("OS=linux\n").is_err());
        assert!(HostCaps::parse_probe("ARCH=mips\nOS=linux\n").is_err());
        assert!(HostCaps::parse_probe("ARCH=x86_64\n").is_err());
    }

    #[test]
    fn probe_parse_ignores_unknown_keys_and_comments() {
        let caps = HostCaps::parse_probe(
            "# probe v1\nARCH=x86_64\nOS=linux\nFUTURE_KEY=whatever\n\nPODMAN=5.0\n",
        )
        .unwrap();
        assert_eq!(caps.runtime.as_ref().unwrap().version, "5.0");
    }

    #[test]
    fn cloud_caps_are_template_only() {
        let caps = HostCaps::cloud_managed(Arch::Amd64);
        assert_eq!(
            caps.runtime.as_ref().unwrap().kind,
            RuntimeKind::CloudManaged
        );
        assert_eq!(
            caps.delivery,
            BTreeSet::from([DeliveryCap::ProviderTemplate])
        );
        assert!(!caps.can_install_runtime);
    }

    #[test]
    fn caps_round_trip_json() {
        let caps = HostCaps::parse_probe("ARCH=x86_64\nOS=linux\nPODMAN=4.9\nRSYNC=1\n").unwrap();
        let json = serde_json::to_string(&caps).unwrap();
        assert_eq!(serde_json::from_str::<HostCaps>(&json).unwrap(), caps);
    }

    #[test]
    fn delivery_cap_parses_config_names() {
        assert_eq!(
            DeliveryCap::parse("ssh-stream"),
            Some(DeliveryCap::SshStream)
        );
        assert_eq!(
            DeliveryCap::parse("Registry"),
            Some(DeliveryCap::RegistryPull)
        );
        assert_eq!(
            DeliveryCap::parse("skopeo"),
            Some(DeliveryCap::SkopeoRemote)
        );
        assert_eq!(
            DeliveryCap::parse("template"),
            Some(DeliveryCap::ProviderTemplate)
        );
        assert_eq!(DeliveryCap::parse("carrier-pigeon"), None);
    }

    #[test]
    fn volume_specs_from_names() {
        let vols = VolumeSpec::from_names(&["nix-store".into(), "cargo".into(), "bogus".into()]);
        assert_eq!(vols.len(), 2);
        assert_eq!(vols[0].name, VOLUME_NIX_STORE);
        assert_eq!(vols[0].dest, "/nix");
        assert_eq!(vols[1].name, VOLUME_CARGO);
        assert!(matches!(vols[0].seed, VolumeSeed::ImageCopyUp));
    }

    #[test]
    fn host_step_round_trips_and_labels() {
        for s in [
            HostStep::Connect,
            HostStep::Probe,
            HostStep::Consent,
            HostStep::Install,
            HostStep::ResolveImage,
            HostStep::Deliver,
            HostStep::SeedVolume,
            HostStep::Verify,
        ] {
            assert_eq!(HostStep::parse(s.as_str()), Some(s));
            assert!(!s.label().is_empty());
        }
        assert_eq!(HostStep::parse("nope"), None);
    }

    #[test]
    fn failure_displays_step_and_error() {
        let f = HostFailure {
            step: HostStep::Install,
            error: "declined".into(),
            retryable: false,
        };
        assert_eq!(f.to_string(), "install: declined");
    }

    /// A minimal spec for the injection tests (SandboxSpec has no Default).
    fn blank_spec() -> crate::sandbox::SandboxSpec {
        crate::sandbox::SandboxSpec {
            backend: crate::sandbox::Backend::Podman,
            placement: crate::placement::Placement::Local,
            image: None,
            worktree: std::path::PathBuf::from("/wt/feat"),
            mounts: Vec::new(),
            env: Vec::new(),
            env_overrides: std::collections::HashMap::new(),
            env_block: Vec::new(),
            network: crate::config::Network::Nat,
            network_allow: Vec::new(),
            network_block: Vec::new(),
            read_only_root: false,
            no_new_privileges: false,
            pids_limit: None,
            drop_capabilities: Vec::new(),
            add_capabilities: Vec::new(),
            ports: Vec::new(),
            gpu: None,
            limits: crate::sandbox::SandboxLimits::default(),
            volumes: Vec::new(),
            compose: None,
            init_script: None,
            file_access: crate::config::FileAccess::Worktree,
            devenv: false,
            devenv_path: None,
            name: "superzej-test".into(),
            vpn: None,
            oci_host: None,
        }
    }

    #[test]
    fn apply_ready_host_respects_explicit_values() {
        let mut spec = blank_spec();
        let rh = ReadyHostSpec {
            image: "ghcr.io/x/base@sha256:abc".into(),
            oci_url: Some("ssh://blake@box/run/podman.sock".into()),
            volumes: vec![
                (VOLUME_NIX_STORE.into(), "/nix".into()),
                (VOLUME_CARGO.into(), "/home/superzej/.cargo".into()),
            ],
            init_script: Some(". /synth/env".into()),
        };
        apply_ready_host(&mut spec, &rh);
        assert_eq!(spec.image.as_deref(), Some("ghcr.io/x/base@sha256:abc"));
        assert_eq!(spec.init_script.as_deref(), Some(". /synth/env"));
        assert_eq!(
            spec.oci_host.as_deref(),
            Some("ssh://blake@box/run/podman.sock")
        );
        assert_eq!(spec.volumes.len(), 2);

        // Explicit user values win; volumes dedup by destination.
        let mut spec2 = blank_spec();
        spec2.image = Some("mine:latest".into());
        spec2.oci_host = Some("unix:///mine.sock".into());
        spec2.volumes = vec![("custom-nix".into(), "/nix".into())];
        apply_ready_host(&mut spec2, &rh);
        assert_eq!(spec2.image.as_deref(), Some("mine:latest"));
        assert_eq!(spec2.oci_host.as_deref(), Some("unix:///mine.sock"));
        assert_eq!(spec2.volumes.len(), 2, "only the missing dest was added");
        assert!(spec2.volumes.iter().any(|(n, _)| n == "custom-nix"));
    }

    #[test]
    fn reach_kind_strings() {
        assert_eq!(Reach::Local.kind(), "local");
        assert_eq!(
            Reach::Iroh(IrohReach {
                ticket: "t".into(),
                ssh_port: 22,
                user: "u".into()
            })
            .kind(),
            "iroh"
        );
        assert_eq!(
            Reach::Cloud(CloudReach {
                provider: "sprites".into(),
                api_base: "https://api.sprites.dev".into(),
                api_key_env: "SPRITES_TOKEN".into(),
                template: "base".into()
            })
            .kind(),
            "cloud"
        );
    }
}
