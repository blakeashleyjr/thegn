use super::*;

const ED25519: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB test";
const ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBP5nnHf/PY1XXPLK2bHSPp9shLHEfiv5XGKDVM01BYvxt1ViFYUZSSRBYQXjCzU7qBc7eFqa7730F4v9AeUiadY= e";
const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDcToULsc4HNU3zPdezikOE+Yy70WZxJPeXXBX8PFyXNBCtmwjyzpgXfuQoh3NWKeePawo7WDVpsui/p5dOjVhltwZ0An6O+SKm9k5FfdeJmD4ROyQf493rev9lFziF6P1fNgQBoJnAZADv4czY7Vm2QynG/xEZX9J+PM+LebmYZRW5NqzhzIrbtKf+FY4QD+v2VWvDm6jyUBr6s0oxGPm9mcDGh/ZxaNNig2GCOf7ce91VBt1wlCAAX9jF5MzK21Nwl9dj9+0i5htppuNoGPN43JdS0PUx+9h8YlhnT7JpCPMUvnuNuRNBB0iZms/9bWDrEOUv0qGWyiYTy+n2cVLD r";

fn encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut buf = [0u8; 3];
        buf[..chunk.len()].copy_from_slice(chunk);
        let n = u32::from_be_bytes([0, buf[0], buf[1], buf[2]]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn wire_string(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
}

fn fly_input<'a>() -> FlyInput<'a> {
    FlyInput {
        api_base: "http://127.0.0.1:8080/v1",
        graphql_url: "http://127.0.0.1:8080/graphql",
        org_slug: "",
        name: Some("sandbox-1"),
        region: "",
        size: "",
        image: "",
        max_instances: 0,
        max_lifetime_secs: 0,
        pubkey: Some(ED25519),
        metadata: None,
        iroh: None,
    }
}

fn vps_input<'a>() -> VpsInput<'a> {
    VpsInput {
        kind: "hetzner",
        api_base: "",
        name: Some("sandbox-1"),
        region: "",
        size: "",
        image: "",
        max_instances: 0,
        max_lifetime_secs: 0,
        pubkey: Some(ED25519),
        metadata: None,
    }
}

#[test]
fn fly_sizes_are_shared_unknown_is_typed_and_auto_is_the_default() {
    assert_eq!(FlySize::parse("shared-cpu-2x").unwrap().guest().2, 512);
    for (raw, expected) in [
        ("", "shared-cpu-2x"),
        ("auto", "shared-cpu-2x"),
        ("performance-4x", "performance-4x"),
    ] {
        let spec = admit_fly(FlyInput {
            size: raw,
            ..fly_input()
        })
        .unwrap();
        assert_eq!(spec.size.as_str(), expected);
    }
    for raw in ["typo", "shared-cpu-3x", "SHARED-CPU-2X", " shared-cpu-2x"] {
        let error = admit_fly(FlyInput {
            size: raw,
            ..fly_input()
        })
        .unwrap_err();
        assert_eq!(error.field, "size", "{raw:?}");
    }
}

#[test]
fn fly_defaults_are_resolved_only_for_absent_fields() {
    let spec = admit_fly(fly_input()).unwrap();
    assert_eq!(spec.region, "iad");
    assert_eq!(spec.org_slug, "personal");
    assert_eq!(spec.image, FlyImage::Default);
    let spec = admit_fly(FlyInput {
        region: "ams",
        org_slug: "acme-co",
        ..fly_input()
    })
    .unwrap();
    assert_eq!(
        (spec.region.as_str(), spec.org_slug.as_str()),
        ("ams", "acme-co")
    );
}

#[test]
fn names_follow_the_bounded_hostname_grammar() {
    let exact = "n".repeat(MAX_PROVIDER_NAME_BYTES);
    assert!(
        admit_vps(VpsInput {
            name: Some(&exact),
            ..vps_input()
        })
        .is_ok()
    );
    let over = "n".repeat(MAX_PROVIDER_NAME_BYTES + 1);
    for name in [
        over.as_str(),
        "",
        " sandbox",
        "sandbox ",
        "-sandbox",
        "sandbox-",
        "a..b",
        "a/b",
        "a?b",
        "a#b",
        "a:b",
        "a_b",
        "a@b",
        "sändbox",
    ] {
        assert_eq!(
            admit_vps(VpsInput {
                name: Some(name),
                ..vps_input()
            })
            .unwrap_err()
            .field,
            "name",
            "{name:?}"
        );
        assert_eq!(
            admit_fly(FlyInput {
                name: Some(name),
                ..fly_input()
            })
            .unwrap_err()
            .field,
            "name",
            "{name:?}"
        );
    }
    assert!(
        admit_vps(VpsInput {
            name: Some("thegn-tg-quick-dagger-a1b2c3.dev"),
            ..vps_input()
        })
        .is_ok()
    );
}

#[test]
fn region_org_and_size_are_lowercase_slugs() {
    for region in ["iad", "fsn1", "nyc3", "us-east-1"] {
        assert!(
            admit_vps(VpsInput {
                region,
                ..vps_input()
            })
            .is_ok(),
            "{region:?}"
        );
    }
    for region in ["IAD", "1ad", "ia/d", "iad?", "ia d", "iad-", "a:b"] {
        assert_eq!(
            admit_fly(FlyInput {
                region,
                ..fly_input()
            })
            .unwrap_err()
            .field,
            "region",
            "{region:?}"
        );
    }
    for org in ["Acme", "acme/x", "acme?", "-acme"] {
        assert_eq!(
            admit_fly(FlyInput {
                org_slug: org,
                ..fly_input()
            })
            .unwrap_err()
            .field,
            "org"
        );
    }
    for (size, expected) in [
        ("", None),
        ("auto", None),
        ("cx23", Some("cx23")),
        ("s-1vcpu-2gb", Some("s-1vcpu-2gb")),
    ] {
        let spec = admit_vps(VpsInput {
            size,
            ..vps_input()
        })
        .unwrap();
        assert_eq!(spec.size.as_deref(), expected);
    }
    for size in ["CX23", "cx23/", "cx 23", "cx23?x", "-cx"] {
        assert_eq!(
            admit_vps(VpsInput {
                size,
                ..vps_input()
            })
            .unwrap_err()
            .field,
            "size"
        );
    }
}

#[test]
fn endpoints_accept_well_formed_hosts_and_refuse_everything_else() {
    for endpoint in [
        "",
        "https://api.machines.dev/v1",
        "https://api.hetzner.cloud/v1/",
        "http://127.0.0.1:8080/v1",
        "http://[::1]:8080/v1",
        "http://[::1]",
        "http://localhost:1/x%2Fy",
        "https://example.com",
    ] {
        assert!(
            admit_fly(FlyInput {
                api_base: endpoint,
                graphql_url: endpoint,
                ..fly_input()
            })
            .is_ok(),
            "{endpoint:?}"
        );
    }
    for endpoint in [
        "127.0.0.1:8080",
        "ftp://example.com",
        "HTTPS://example.com",
        "https://user:token@example.invalid/api",
        "https://example.invalid/api#fragment",
        "https://example.invalid/api?token=1",
        "http://127.0.0.1:0/api",
        "http://[garbage]/",
        "http://[::1]80/",
        "http://[::1]:/",
        "http://[::1",
        "http://ex%41mple.com/",
        "http://example.com:99999/",
        "http://example.com:+80/",
        "http://999.1.1.1/",
        "http://exa_mple.com/",
        "http://-bad.com/",
        "http://example.com\\@evil/",
        "http://example.com/a b",
        "http://example.com/%zz",
        "http://example.com/<x>",
        "http://:80/",
        "http://a:b:c/",
        "http://",
        "https:///path",
    ] {
        assert_eq!(
            admit_fly(FlyInput {
                api_base: endpoint,
                ..fly_input()
            })
            .unwrap_err()
            .field,
            "api_base",
            "{endpoint:?}"
        );
        assert_eq!(
            admit_fly(FlyInput {
                graphql_url: endpoint,
                ..fly_input()
            })
            .unwrap_err()
            .field,
            "graphql_url",
            "{endpoint:?}"
        );
        assert_eq!(
            admit_vps(VpsInput {
                api_base: endpoint,
                ..vps_input()
            })
            .unwrap_err()
            .field,
            "api_base",
            "{endpoint:?}"
        );
    }
    let long = format!(
        "https://example.com/{}",
        "a".repeat(MAX_PROVIDER_ENDPOINT_BYTES)
    );
    assert!(
        admit_fly(FlyInput {
            api_base: &long,
            ..fly_input()
        })
        .is_err()
    );
}

#[test]
fn fly_image_references_follow_the_oci_grammar() {
    let digest = format!("localhost:5000/a/b@sha256:{}", "a".repeat(64));
    for (image, prebaked) in [
        ("ubuntu:24.04", false),
        ("library/ubuntu", false),
        ("registry.fly.io/x:deployment-2", false),
        ("image:registry.example/dev:1", true),
        ("image:ghcr.io/org/thegn-sandbox_img:v1.2", true),
        (digest.as_str(), false),
    ] {
        let spec = admit_fly(FlyInput {
            image,
            ..fly_input()
        })
        .unwrap_or_else(|error| panic!("{image:?}: {error}"));
        assert_eq!(spec.image.is_prebaked(), prebaked, "{image:?}");
        assert_eq!(
            spec.image.resolve("default"),
            image.strip_prefix("image:").unwrap_or(image)
        );
    }
    for image in [
        "snapshot:42",
        "image:",
        "image:Bad/Ref",
        "Ubuntu",
        "a//b",
        "/a",
        "a/",
        "a/b:",
        "a/b:-bad",
        "reg.io/a?b",
        "reg.io/a#b",
        "a@sha256:short",
        "a@md5:abcd",
        "reg.io:0/a",
        "reg.io:x/a",
        "a/b/../c",
        "a/b c",
        "a/b---c_",
        "a___b",
    ] {
        assert_eq!(
            admit_fly(FlyInput {
                image,
                ..fly_input()
            })
            .unwrap_err()
            .field,
            "image",
            "{image:?}"
        );
    }
    let over = "a".repeat(MAX_PROVIDER_IMAGE_BYTES + 1);
    assert!(
        admit_fly(FlyInput {
            image: &over,
            ..fly_input()
        })
        .is_err()
    );
}

#[test]
fn vps_snapshot_is_numeric_positive_and_never_falls_back_to_an_image() {
    let spec = admit_vps(VpsInput {
        image: "snapshot:007",
        ..vps_input()
    })
    .unwrap();
    assert_eq!(spec.image, VpsImage::Snapshot("7".into()));
    for (image, snapshot) in [
        ("ubuntu-24.04", false),
        ("ubuntu-24-04-x64", false),
        ("123", false),
    ] {
        let spec = admit_vps(VpsInput {
            image,
            ..vps_input()
        })
        .unwrap();
        assert_eq!(spec.image.is_snapshot(), snapshot);
        assert_eq!(spec.image.resolve("default"), image);
    }
    for image in [
        "snapshot:",
        "snapshot:nope",
        "snapshot:0",
        "snapshot:-1",
        "snapshot:+1",
        "snapshot:9223372036854775808",
        "snapshot:12345678901234567890",
        "image:ubuntu",
        "docker:ubuntu",
        "Ubuntu",
        "a/b",
        "-a",
        "a b",
    ] {
        assert_eq!(
            admit_vps(VpsInput {
                image,
                ..vps_input()
            })
            .unwrap_err()
            .field,
            "image",
            "{image:?}"
        );
    }
}

#[test]
fn public_keys_are_parsed_structurally() {
    for key in [
        ED25519,
        ECDSA,
        RSA,
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB",
    ] {
        assert!(
            admit_vps(VpsInput {
                pubkey: Some(key),
                ..vps_input()
            })
            .is_ok(),
            "{key:?}"
        );
    }

    let mut ed = Vec::new();
    wire_string(&mut ed, b"ssh-ed25519");
    wire_string(&mut ed, &[7u8; 32]);
    let good = format!("ssh-ed25519 {}", encode(&ed));
    assert!(
        admit_vps(VpsInput {
            pubkey: Some(&good),
            ..vps_input()
        })
        .is_ok()
    );

    let mut short = Vec::new();
    wire_string(&mut short, b"ssh-ed25519");
    wire_string(&mut short, &[7u8; 31]);
    let mut trailing = ed.clone();
    trailing.push(0);
    let mut mismatch = Vec::new();
    wire_string(&mut mismatch, b"ssh-rsa");
    wire_string(&mut mismatch, &[7u8; 32]);
    let mut weak_rsa = Vec::new();
    wire_string(&mut weak_rsa, b"ssh-rsa");
    wire_string(&mut weak_rsa, &[1, 0, 1]);
    let mut modulus = vec![0x41u8; 128];
    modulus[127] |= 1;
    wire_string(&mut weak_rsa, &modulus);
    let mut bad_point = Vec::new();
    wire_string(&mut bad_point, b"ecdsa-sha2-nistp256");
    wire_string(&mut bad_point, b"nistp256");
    wire_string(&mut bad_point, &[0x05; 65]);

    let noncanonical = {
        // `B` instead of `A` in the final sextet sets discarded low bits.
        let mut text = encode(&ed[..ed.len() - 1]);
        let pad = text.pop();
        let last = text.pop();
        assert_eq!(pad, Some('='));
        assert!(last.is_some());
        text.push('B');
        text.push('=');
        text
    };
    let cases = [
        "ssh-ed25519 MOCKKEY thegn".to_owned(),
        "ssh-ed25519 ==== thegn".to_owned(),
        "ssh-ed25519".to_owned(),
        "not-a-public-key".to_owned(),
        format!("ssh-ed25519 {}", encode(&short)),
        format!("ssh-ed25519 {}", encode(&trailing)),
        format!("ssh-ed25519 {}", encode(&mismatch)),
        format!("ssh-rsa {}", encode(&mismatch)),
        format!("ssh-rsa {}", encode(&weak_rsa)),
        format!("ecdsa-sha2-nistp256 {}", encode(&bad_point)),
        format!("ssh-ed25519 {noncanonical}"),
        format!("ssh-dss {}", encode(&ed)),
        format!("ssh-ed25519 {}\u{7}", encode(&ed)),
        format!("ssh-ed25519 {}", "A".repeat(MAX_PROVIDER_KEY_BYTES + 4)),
    ];
    for key in &cases {
        assert_eq!(
            admit_vps(VpsInput {
                pubkey: Some(key),
                ..vps_input()
            })
            .unwrap_err()
            .field,
            "pubkey",
            "{key:?}"
        );
    }
}

#[test]
fn metadata_requires_management_labels_and_bounded_shape() {
    let mut metadata = BTreeMap::new();
    metadata.insert("managed-by".to_owned(), "thegn".to_owned());
    metadata.insert("tg-host".to_owned(), "abc123".to_owned());
    assert!(
        admit_fly(FlyInput {
            metadata: Some(&metadata),
            ..fly_input()
        })
        .is_ok()
    );
    for (key, value) in [
        ("managed-by", "other"),
        ("tg-host", ""),
        ("bad key", "x"),
        ("x", "has space"),
        ("x", "a/b"),
    ] {
        let mut bad = metadata.clone();
        bad.insert(key.to_owned(), value.to_owned());
        assert_eq!(
            admit_vps(VpsInput {
                metadata: Some(&bad),
                ..vps_input()
            })
            .unwrap_err()
            .field,
            "metadata",
            "{key:?}={value:?}"
        );
    }
    let mut missing = metadata.clone();
    missing.remove("tg-host");
    assert!(validate_metadata(&missing).is_err());
    let mut wide = metadata.clone();
    for index in 0..MAX_PROVIDER_METADATA_ENTRIES {
        wide.insert(format!("k{index}"), "v".to_owned());
    }
    assert!(validate_metadata(&wide).is_err());
}

#[test]
fn iroh_injection_is_validated_and_bound_to_the_sandbox_name() {
    let token = format!("tgi_{}", "ab".repeat(24));
    let home = "c".repeat(64);
    let valid = IrohInjectionInput {
        home_node: &home,
        sandbox_auth: &token,
        sandbox_id: "sandbox-1",
    };
    assert!(
        admit_fly(FlyInput {
            iroh: Some(valid),
            ..fly_input()
        })
        .is_ok()
    );
    let cases: [(&str, &str, &str, &str); 5] = [
        (&home, "tgi_short", "sandbox-1", "iroh.sandbox_auth"),
        (
            &home,
            "secret-token-without-prefix-0000000000000000",
            "sandbox-1",
            "iroh.sandbox_auth",
        ),
        ("HOME NODE", &token, "sandbox-1", "iroh.home_node"),
        ("", &token, "sandbox-1", "iroh.home_node"),
        (&home, &token, "other-sandbox", "iroh.sandbox_id"),
    ];
    for (home_node, sandbox_auth, sandbox_id, field) in cases {
        let error = admit_fly(FlyInput {
            iroh: Some(IrohInjectionInput {
                home_node,
                sandbox_auth,
                sandbox_id,
            }),
            ..fly_input()
        })
        .unwrap_err();
        assert_eq!(error.field, field);
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(&token) && !rendered.contains("secret-token"));
    }
}

#[test]
fn numeric_guardrails_accept_exact_limits_and_reject_one_over() {
    assert!(
        admit_vps(VpsInput {
            max_instances: MAX_PROVIDER_INSTANCES,
            max_lifetime_secs: crate::time_policy::MAX_DURATION_SECS,
            ..vps_input()
        })
        .is_ok()
    );
    assert_eq!(
        admit_vps(VpsInput {
            max_instances: MAX_PROVIDER_INSTANCES + 1,
            ..vps_input()
        })
        .unwrap_err()
        .field,
        "max_instances"
    );
    assert_eq!(
        admit_fly(FlyInput {
            max_lifetime_secs: crate::time_policy::MAX_DURATION_SECS + 1,
            ..fly_input()
        })
        .unwrap_err()
        .field,
        "max_lifetime_secs"
    );
    assert_eq!(
        validate_vps_config("hetzner", "", "", "", "", MAX_PROVIDER_INSTANCES + 1, 0)
            .unwrap_err()
            .field,
        "max_instances"
    );
    assert_eq!(
        validate_vps_config("linode", "", "", "", "", 0, 0)
            .unwrap_err()
            .field,
        "provider"
    );
}

#[test]
fn errors_and_debug_never_echo_values_or_secrets() {
    let canary = "canary-7f3e";
    let endpoint = format!("https://{canary}:{canary}@example.invalid/v1");
    let error = admit_fly(FlyInput {
        api_base: &endpoint,
        ..fly_input()
    })
    .unwrap_err();
    let image = format!("snapshot:{canary}");
    let image_error = admit_vps(VpsInput {
        image: &image,
        ..vps_input()
    })
    .unwrap_err();
    for rendered in [
        error.to_string(),
        format!("{error:?}"),
        image_error.to_string(),
        format!("{image_error:?}"),
    ] {
        assert!(!rendered.contains(canary), "{rendered}");
    }
    let spec = admit_fly(FlyInput {
        image: "image:registry.example/canary-7f3e:1",
        region: "ams",
        ..fly_input()
    })
    .unwrap();
    let debug = format!("{spec:?}");
    assert!(!debug.contains(canary) && !debug.contains("AAAAC3") && !debug.contains("ams"));
    let vps = admit_vps(VpsInput {
        region: "fsn1",
        image: "snapshot:42",
        ..vps_input()
    })
    .unwrap();
    let debug = format!("{vps:?}");
    assert!(!debug.contains("fsn1") && !debug.contains("42") && !debug.contains("AAAAC3"));
}

#[test]
fn config_validation_uses_the_same_grammar() {
    let mut cfg = crate::config::Config::default();
    let mut env = crate::config::EnvConfig::default();
    env.provider.provider = "fly".into();
    env.provider.size = "shared-cpu-3x".into();
    cfg.env.insert("flyenv".into(), env.clone());
    env.provider.provider = "digitalocean".into();
    env.provider.size = String::new();
    env.provider.template = "image:ubuntu".into();
    cfg.env.insert("doenv".into(), env);
    let errors = validate_config(&cfg);
    assert!(
        errors
            .iter()
            .any(|error| error.starts_with("env.flyenv.provider.size")),
        "{errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|error| error.starts_with("env.doenv.provider.image")),
        "{errors:?}"
    );
}

#[test]
fn base64_decoding_is_strict_and_canonical() {
    assert_eq!(decode_base64("AA=="), Some(vec![0]));
    assert_eq!(decode_base64("AAA="), Some(vec![0, 0]));
    assert_eq!(decode_base64("AAAA"), Some(vec![0, 0, 0]));
    for raw in [
        "", "A", "AA=", "AB==", "AAB=", "A===", "AA==AAAA", "AA-_", "AA A",
    ] {
        assert_eq!(decode_base64(raw), None, "{raw:?}");
    }
    assert_eq!(
        decode_base64(&encode(b"thegn")).as_deref(),
        Some(&b"thegn"[..])
    );
}
