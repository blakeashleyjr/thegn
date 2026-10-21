//! Versioned fingerprint of security-significant sandbox policy inputs.
//!
//! This hashes spec inputs rather than derived runtime flags. If an unchanged
//! input is lowered differently into create options (for example, a changed
//! `--dns` flag form), bump the `v1` prefix before shipping that lowering change.
//! The encoding is pure, length-prefixed, and independent of process state.

use sha2::{Digest, Sha256};

use crate::sandbox::SandboxSpec;

/// Return the lowercase SHA-256 fingerprint for the effective sandbox policy.
pub fn policy_fingerprint(spec: &SandboxSpec, vpn_join: bool) -> String {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(b"v1\0");

    fn field(encoded: &mut Vec<u8>, name: &str, value: &str) {
        encoded.extend_from_slice(name.as_bytes());
        encoded.push(0);
        encoded.extend_from_slice(value.len().to_string().as_bytes());
        encoded.push(0);
        encoded.extend_from_slice(value.as_bytes());
    }

    fn sorted_values<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
        let mut values: Vec<&str> = values.into_iter().collect();
        values.sort_unstable();
        values.dedup();
        values
    }

    fn optional(encoded: &mut Vec<u8>, name: &str, value: Option<&str>) {
        match value {
            Some(value) => {
                field(encoded, name, "some");
                field(encoded, name, value);
            }
            None => field(encoded, name, "none"),
        }
    }

    field(&mut encoded, "backend", spec.backend.label());
    optional(
        &mut encoded,
        "oci_runtime",
        spec.oci_runtime.as_deref().map(str::trim),
    );
    optional(&mut encoded, "image", spec.image.as_deref());

    if let Some(build) = &spec.build {
        field(&mut encoded, "build", "some");
        field(&mut encoded, "build.dockerfile", &build.dockerfile);
        field(&mut encoded, "build.context", &build.context);
        for (key, value) in &build.args {
            field(&mut encoded, "build.arg.key", key);
            field(&mut encoded, "build.arg.value", value);
        }
        optional(&mut encoded, "build.target", build.target.as_deref());
    } else {
        field(&mut encoded, "build", "none");
    }

    field(&mut encoded, "network", spec.network.as_str());
    field(
        &mut encoded,
        "vpn_join",
        if vpn_join { "true" } else { "false" },
    );
    if let Some(vpn) = &spec.vpn {
        field(&mut encoded, "vpn", "some");
        field(&mut encoded, "vpn.provider", vpn.provider.as_str());
        field(&mut encoded, "vpn.mode", vpn.mode.as_str());
        field(&mut encoded, "vpn.dns_mode", vpn.dns_mode.as_str());
        optional(
            &mut encoded,
            "vpn.sidecar_image",
            vpn.sidecar_image.as_deref(),
        );
    } else {
        field(&mut encoded, "vpn", "none");
    }

    for value in sorted_values(spec.network_allow.iter().map(String::as_str)) {
        field(&mut encoded, "network_allow", value);
    }
    for value in sorted_values(spec.network_block.iter().map(String::as_str)) {
        field(&mut encoded, "network_block", value);
    }

    // Hash every admitted mount, including mounts filtered from OCI argv: both
    // create and reuse hash the same spec view, so filtering here would weaken
    // the policy identity unnecessarily.
    let mut mounts: Vec<_> = spec
        .mounts
        .iter()
        .map(|mount| {
            (
                mount.host.as_str(),
                mount.dest.as_str(),
                mount.ro,
                mount.cache,
            )
        })
        .collect();
    mounts.sort_unstable();
    for (host, dest, ro, cache) in mounts {
        field(&mut encoded, "mount.host", host);
        field(&mut encoded, "mount.dest", dest);
        field(&mut encoded, "mount.ro", if ro { "true" } else { "false" });
        field(
            &mut encoded,
            "mount.cache",
            if cache { "true" } else { "false" },
        );
    }
    let mut volumes: Vec<_> = spec
        .volumes
        .iter()
        .map(|(volume, destination)| (volume.as_str(), destination.as_str()))
        .collect();
    volumes.sort_unstable();
    for (volume, destination) in volumes {
        field(&mut encoded, "volume", volume);
        field(&mut encoded, "volume.destination", destination);
    }
    for value in sorted_values(spec.env_block.iter().map(String::as_str)) {
        field(&mut encoded, "env_block", value);
    }

    field(
        &mut encoded,
        "read_only_root",
        if spec.read_only_root { "true" } else { "false" },
    );
    field(
        &mut encoded,
        "no_new_privileges",
        if spec.no_new_privileges {
            "true"
        } else {
            "false"
        },
    );
    optional(
        &mut encoded,
        "pids_limit",
        spec.pids_limit.map(|value| value.to_string()).as_deref(),
    );
    for value in sorted_values(spec.drop_capabilities.iter().map(String::as_str)) {
        field(&mut encoded, "drop_capability", value);
    }
    for value in sorted_values(spec.add_capabilities.iter().map(String::as_str)) {
        field(&mut encoded, "add_capability", value);
    }
    optional(&mut encoded, "limits.cpu", spec.limits.cpu.as_deref());
    optional(&mut encoded, "limits.memory", spec.limits.memory.as_deref());
    for value in sorted_values(spec.ports.iter().map(String::as_str)) {
        field(&mut encoded, "port", value);
    }
    optional(&mut encoded, "gpu", spec.gpu.as_deref());
    field(
        &mut encoded,
        "seal_home",
        if spec.seal_home.is_some() {
            "true"
        } else {
            "false"
        },
    );
    // FileAccess has no stable label method yet; its Debug form is the enum's
    // explicit variant name and is covered by the v1 bump rule above.
    field(
        &mut encoded,
        "file_access",
        &format!("{:?}", spec.file_access),
    );
    field(
        &mut encoded,
        "devenv",
        if spec.devenv { "true" } else { "false" },
    );
    optional(&mut encoded, "devenv_path", spec.devenv_path.as_deref());

    // env/env_overrides are per-launch secret values and must not enter a
    // reusable policy identity.
    // name is a locator, not container policy.
    // worktree is represented by the admitted mount set.
    // placement/oci_host are excluded because the probe uses that same daemon.
    // daemon_persistent controls host bwrap lifetime, not OCI container policy.
    // init_script is per-launch initialization, not the admitted OCI policy.
    // limits.cpu_total/memory_total are aggregate slice limits, not container policy.
    // compose specs return before the OCI reuse probe.

    let digest = Sha256::digest(encoded);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        write!(&mut hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::policy_fingerprint;
    use crate::config::{FileAccess, Network};
    use crate::config::{VpnDnsMode, VpnMode, VpnOnError, VpnProviderKind};
    use crate::placement::{Placement, SshPlacement, TransportKind};
    use crate::sandbox::{Backend, Mount, SandboxLimits, SandboxSpec, VpnParams, VpnSpec};
    use crate::sandbox_build::SandboxBuild;
    use std::path::PathBuf;
    use std::time::Duration;

    fn spec() -> SandboxSpec {
        SandboxSpec {
            backend: Backend::Podman,
            placement: Placement::Local,
            image: Some("img:latest".into()),
            worktree: PathBuf::from("/wt/feat"),
            mounts: vec![Mount {
                host: "/wt/feat".into(),
                dest: "/work".into(),
                ro: false,
                cache: false,
            }],
            env: Vec::new(),
            env_overrides: std::collections::HashMap::new(),
            env_block: Vec::new(),
            network: Network::Nat,
            network_allow: Vec::new(),
            network_block: Vec::new(),
            read_only_root: false,
            no_new_privileges: false,
            pids_limit: None,
            drop_capabilities: Vec::new(),
            add_capabilities: Vec::new(),
            file_access: FileAccess::WorktreePlusCaches,
            ports: Vec::new(),
            gpu: None,
            limits: SandboxLimits::default(),
            volumes: Vec::new(),
            compose: None,
            build: None,
            init_script: None,
            devenv: false,
            devenv_path: None,
            name: "thegn-repo-feat".into(),
            vpn: None,
            oci_host: None,
            oci_runtime: None,
            daemon_persistent: false,
            seal_home: None,
            volume_owners: Vec::new(),
        }
    }

    #[test]
    fn fingerprint_changes_on_network_nat_to_none() {
        let original = spec();
        let changed = SandboxSpec {
            network: Network::None,
            ..original.clone()
        };
        assert_ne!(
            policy_fingerprint(&original, false),
            policy_fingerprint(&changed, false)
        );
    }

    #[test]
    fn fingerprint_changes_on_mount_rw_to_ro() {
        let original = spec();
        let changed = SandboxSpec {
            mounts: vec![Mount {
                ro: true,
                ..original.mounts[0].clone()
            }],
            ..original.clone()
        };
        assert_ne!(
            policy_fingerprint(&original, false),
            policy_fingerprint(&changed, false)
        );
    }

    #[test]
    fn fingerprint_changes_on_mount_dest_only() {
        let original = spec();
        let changed = SandboxSpec {
            mounts: vec![Mount {
                dest: "/other".into(),
                ..original.mounts[0].clone()
            }],
            ..original.clone()
        };
        assert_ne!(
            policy_fingerprint(&original, false),
            policy_fingerprint(&changed, false)
        );
    }

    #[test]
    fn fingerprint_changes_on_dropped_gnupg_mount() {
        let original = SandboxSpec {
            mounts: vec![
                spec().mounts[0].clone(),
                Mount {
                    host: "/home/test/.gnupg".into(),
                    dest: "/home/test/.gnupg".into(),
                    ro: false,
                    cache: false,
                },
            ],
            ..spec()
        };
        let changed = SandboxSpec {
            mounts: vec![original.mounts[0].clone()],
            ..original.clone()
        };
        assert_ne!(
            policy_fingerprint(&original, false),
            policy_fingerprint(&changed, false)
        );
    }

    #[test]
    fn fingerprint_changes_on_image_runtime_caps_limits() {
        let original = spec();
        let cases = [
            SandboxSpec {
                image: Some("img:other".into()),
                ..original.clone()
            },
            SandboxSpec {
                oci_runtime: Some("runsc".into()),
                ..original.clone()
            },
            SandboxSpec {
                drop_capabilities: vec!["ALL".into()],
                ..original.clone()
            },
            SandboxSpec {
                limits: SandboxLimits {
                    cpu: Some("2".into()),
                    ..SandboxLimits::default()
                },
                ..original.clone()
            },
        ];
        for changed in cases {
            assert_ne!(
                policy_fingerprint(&original, false),
                policy_fingerprint(&changed, false)
            );
        }
    }

    #[test]
    fn fingerprint_changes_on_build_and_vpn_policy() {
        let original = spec();
        let build = SandboxSpec {
            build: Some(SandboxBuild {
                dockerfile: "/repo/Dockerfile".into(),
                context: "/repo".into(),
                args: [("FEATURE".into(), "on".into())].into_iter().collect(),
                target: Some("dev".into()),
            }),
            ..original.clone()
        };
        assert_ne!(
            policy_fingerprint(&original, false),
            policy_fingerprint(&build, false)
        );

        let vpn = SandboxSpec {
            vpn: Some(VpnSpec {
                provider: VpnProviderKind::Tailscale,
                mode: VpnMode::Sidecar,
                on_error: VpnOnError::Fail,
                dns_mode: VpnDnsMode::Tunnel,
                ready_timeout: Duration::from_secs(30),
                ephemeral: true,
                sidecar_image: Some("vpn:latest".into()),
                hostname: "sandbox".into(),
                tunnel_only: false,
                params: VpnParams::Custom(Default::default()),
            }),
            ..original.clone()
        };
        assert_ne!(
            policy_fingerprint(&original, false),
            policy_fingerprint(&vpn, false)
        );
    }

    #[test]
    fn fingerprint_changes_on_seal_and_vpn_join() {
        let original = spec();
        let sealed = SandboxSpec {
            seal_home: Some("/home/test".into()),
            ..original.clone()
        };
        assert_ne!(
            policy_fingerprint(&original, false),
            policy_fingerprint(&sealed, false)
        );
        assert_ne!(
            policy_fingerprint(&original, false),
            policy_fingerprint(&original, true)
        );
    }

    #[test]
    fn fingerprint_changes_on_allow_block_lists() {
        let original = spec();
        let allow = SandboxSpec {
            network_allow: vec!["example.test".into()],
            ..original.clone()
        };
        let block = SandboxSpec {
            network_block: vec!["example.test".into()],
            ..original.clone()
        };
        assert_ne!(
            policy_fingerprint(&original, false),
            policy_fingerprint(&allow, false)
        );
        assert_ne!(
            policy_fingerprint(&original, false),
            policy_fingerprint(&block, false)
        );
    }

    #[test]
    fn fingerprint_stable_under_reordering() {
        let original = SandboxSpec {
            network_allow: vec!["b.test".into(), "a.test".into()],
            drop_capabilities: vec!["B".into(), "A".into()],
            mounts: vec![
                spec().mounts[0].clone(),
                Mount {
                    host: "/cache".into(),
                    dest: "/cache".into(),
                    ro: true,
                    cache: true,
                },
            ],
            ..spec()
        };
        let reordered = SandboxSpec {
            network_allow: vec!["a.test".into(), "b.test".into()],
            drop_capabilities: vec!["A".into(), "B".into()],
            mounts: original.mounts.iter().rev().cloned().collect(),
            ..original.clone()
        };
        assert_eq!(
            policy_fingerprint(&original, false),
            policy_fingerprint(&reordered, false)
        );
    }

    #[test]
    fn fingerprint_ignores_env_values_name_and_placement() {
        let original = spec();
        let changed = SandboxSpec {
            env: vec![("TOKEN".into(), "secret".into())],
            env_overrides: [("TOKEN".into(), "other".into())].into_iter().collect(),
            name: "another-name".into(),
            placement: Placement::Ssh(SshPlacement::plain(
                "sandbox.example".into(),
                22,
                false,
                TransportKind::Ssh,
            )),
            ..original.clone()
        };
        assert_eq!(
            policy_fingerprint(&original, false),
            policy_fingerprint(&changed, false)
        );
    }

    #[test]
    fn fingerprint_is_versioned_hex_64() {
        let fingerprint = policy_fingerprint(&spec(), false);
        assert_eq!(fingerprint.len(), 64);
        assert!(
            fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }
}
