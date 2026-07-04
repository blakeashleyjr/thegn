//! Hetzner Cloud request/response shaping — **pure** functions only (URLs,
//! bodies, parsers), unit-tested without a live endpoint, mirroring the
//! `DaytonaProvider`/`SpritesProvider` split in `provider.rs`. The async HTTP
//! wrappers live in [`super::VpsProvider`].
//!
//! API: `https://api.hetzner.cloud/v1` (Bearer auth). Servers are created by
//! name with labels; we filter lists by the `managed-by=superzej` label so
//! `list()` never sees a user's unrelated servers.

use std::collections::BTreeMap;

use super::VpsInstance;

pub const DEFAULT_API_BASE: &str = "https://api.hetzner.cloud/v1";
pub const DEFAULT_TOKEN_ENV: &str = "HCLOUD_TOKEN";
/// Cheapest current shared-vCPU type (2 vCPU / 4 GB) — a sane dev default.
pub const DEFAULT_SERVER_TYPE: &str = "cx22";
pub const DEFAULT_LOCATION: &str = "fsn1";
pub const DEFAULT_IMAGE: &str = "ubuntu-24.04";

/// The label every superzej-managed instance carries (reaper filter).
pub const MANAGED_LABEL: &str = "managed-by";
pub const MANAGED_VALUE: &str = "superzej";
/// The label scoping an instance to the creating host (two hosts sharing one
/// Hetzner project must never reap each other's sandboxes).
pub const HOST_LABEL: &str = "sz-host";

pub fn servers_url(base: &str) -> String {
    format!("{}/servers", base.trim_end_matches('/'))
}

pub fn server_url(base: &str, id: &str) -> String {
    format!("{}/servers/{id}", base.trim_end_matches('/'))
}

/// List only superzej-managed servers (label selector, server-side filter).
pub fn list_url(base: &str) -> String {
    format!(
        "{}/servers?label_selector={MANAGED_LABEL}%3D{MANAGED_VALUE}&per_page=50",
        base.trim_end_matches('/')
    )
}

pub fn ssh_keys_url(base: &str) -> String {
    format!("{}/ssh_keys", base.trim_end_matches('/'))
}

pub fn create_image_url(base: &str, id: &str) -> String {
    format!(
        "{}/servers/{id}/actions/create_image",
        base.trim_end_matches('/')
    )
}

pub fn shutdown_url(base: &str, id: &str) -> String {
    format!(
        "{}/servers/{id}/actions/shutdown",
        base.trim_end_matches('/')
    )
}

/// A `template` of `snapshot:<id>` selects a baked snapshot image; anything
/// else is an image name/slug. Returns the snapshot id when present.
pub fn snapshot_image(template: &str) -> Option<&str> {
    template.trim().strip_prefix("snapshot:").map(str::trim)
}

/// The create body. `image` may be a name (`ubuntu-24.04`) or a numeric
/// snapshot id (sent as a JSON number — the API rejects an all-digit string
/// for a snapshot). `user_data` is raw cloud-config (Hetzner takes it verbatim,
/// no base64).
pub fn create_body(
    name: &str,
    server_type: &str,
    image: &str,
    location: &str,
    ssh_key_ids: &[i64],
    user_data: &str,
    labels: &BTreeMap<String, String>,
) -> serde_json::Value {
    let image_v: serde_json::Value = match image.parse::<i64>() {
        Ok(n) => n.into(),
        Err(_) => image.into(),
    };
    let mut body = serde_json::json!({
        "name": name,
        "server_type": server_type,
        "image": image_v,
        "location": location,
        "ssh_keys": ssh_key_ids,
        "labels": labels,
    });
    if !user_data.trim().is_empty() {
        body["user_data"] = user_data.into();
    }
    body
}

/// Parse one server object into a [`VpsInstance`]. `created` is unix seconds
/// (the reaper's age input); a missing/unparsable timestamp is `None`.
pub fn parse_server(v: &serde_json::Value) -> Option<VpsInstance> {
    let id = v.get("id").and_then(|i| i.as_i64())?;
    let name = v.get("name").and_then(|n| n.as_str())?.to_string();
    let running = v.get("status").and_then(|s| s.as_str()) == Some("running");
    let ip = v
        .pointer("/public_net/ipv4/ip")
        .and_then(|i| i.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let created = v
        .get("created")
        .and_then(|c| c.as_str())
        .and_then(|c| chrono::DateTime::parse_from_rfc3339(c).ok())
        .map(|t| t.timestamp());
    let labels = v
        .get("labels")
        .and_then(|l| l.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Some(VpsInstance {
        id: id.to_string(),
        name,
        ip,
        running,
        created,
        labels,
    })
}

/// Parse a create response (`{ "server": { … } }`).
pub fn parse_create(v: &serde_json::Value) -> Option<VpsInstance> {
    parse_server(v.get("server")?)
}

/// Parse a get-by-id response (`{ "server": { … } }`).
pub fn parse_get(v: &serde_json::Value) -> Option<VpsInstance> {
    parse_create(v)
}

/// Parse a list response (`{ "servers": [ … ] }`).
pub fn parse_server_list(v: &serde_json::Value) -> Vec<VpsInstance> {
    v.get("servers")
        .and_then(|s| s.as_array())
        .map(|a| a.iter().filter_map(parse_server).collect())
        .unwrap_or_default()
}

pub fn ssh_key_body(name: &str, pubkey: &str) -> serde_json::Value {
    serde_json::json!({ "name": name, "public_key": pubkey })
}

/// Parse `{ "ssh_keys": [ { id, public_key } ] }` into `(id, public_key)` pairs.
pub fn parse_ssh_keys(v: &serde_json::Value) -> Vec<(i64, String)> {
    v.get("ssh_keys")
        .and_then(|k| k.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|k| {
                    Some((
                        k.get("id")?.as_i64()?,
                        k.get("public_key")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the id out of a created ssh key (`{ "ssh_key": { id } }`).
pub fn parse_ssh_key_created(v: &serde_json::Value) -> Option<i64> {
    v.pointer("/ssh_key/id").and_then(|i| i.as_i64())
}

/// The create-image (snapshot) body for `superzej env image bake`.
pub fn create_image_body(description: &str) -> serde_json::Value {
    serde_json::json!({ "type": "snapshot", "description": description })
}

/// Parse the snapshot image id from a create-image response
/// (`{ "action": …, "image": { id } }`).
pub fn parse_image_created(v: &serde_json::Value) -> Option<i64> {
    v.pointer("/image/id").and_then(|i| i.as_i64())
}

/// Whether two OpenSSH public-key lines carry the same key material (compare
/// `type + blob`, ignoring the trailing comment — the registered key's comment
/// rarely matches ours).
pub fn same_pubkey(a: &str, b: &str) -> bool {
    let core = |s: &str| {
        let mut it = s.split_whitespace();
        match (it.next(), it.next()) {
            (Some(t), Some(b)) => Some((t.to_string(), b.to_string())),
            _ => None,
        }
    };
    match (core(a), core(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_versioned_and_label_filtered() {
        assert_eq!(
            servers_url("https://api.hetzner.cloud/v1/"),
            "https://api.hetzner.cloud/v1/servers"
        );
        assert_eq!(
            server_url(DEFAULT_API_BASE, "42"),
            "https://api.hetzner.cloud/v1/servers/42"
        );
        // List filters server-side to superzej-managed instances only.
        assert_eq!(
            list_url(DEFAULT_API_BASE),
            "https://api.hetzner.cloud/v1/servers?label_selector=managed-by%3Dsuperzej&per_page=50"
        );
        assert_eq!(
            create_image_url(DEFAULT_API_BASE, "7"),
            "https://api.hetzner.cloud/v1/servers/7/actions/create_image"
        );
        assert_eq!(
            shutdown_url(DEFAULT_API_BASE, "7"),
            "https://api.hetzner.cloud/v1/servers/7/actions/shutdown"
        );
    }

    #[test]
    fn create_body_carries_name_type_image_keys_and_labels() {
        let mut labels = BTreeMap::new();
        labels.insert(MANAGED_LABEL.to_string(), MANAGED_VALUE.to_string());
        labels.insert(HOST_LABEL.to_string(), "abc123".to_string());
        let b = create_body(
            "sz-dev-x1",
            "cx22",
            "ubuntu-24.04",
            "fsn1",
            &[101],
            "#cloud-config\n",
            &labels,
        );
        assert_eq!(b["name"], "sz-dev-x1");
        assert_eq!(b["server_type"], "cx22");
        assert_eq!(b["image"], "ubuntu-24.04");
        assert_eq!(b["location"], "fsn1");
        assert_eq!(b["ssh_keys"], serde_json::json!([101]));
        assert_eq!(b["labels"]["managed-by"], "superzej");
        assert_eq!(b["labels"]["sz-host"], "abc123");
        assert_eq!(b["user_data"], "#cloud-config\n");
        // Empty user_data is omitted entirely.
        let b2 = create_body("n", "cx22", "ubuntu-24.04", "fsn1", &[], "", &labels);
        assert!(b2.get("user_data").is_none());
    }

    #[test]
    fn snapshot_template_selects_numeric_image() {
        assert_eq!(snapshot_image("snapshot:123"), Some("123"));
        assert_eq!(snapshot_image(" snapshot: 456 "), Some("456"));
        assert_eq!(snapshot_image("ubuntu-24.04"), None);
        // A numeric image is sent as a JSON number (the API requires it for
        // snapshot ids), a name as a string.
        let labels = BTreeMap::new();
        let by_id = create_body("n", "cx22", "123", "fsn1", &[], "", &labels);
        assert_eq!(by_id["image"], 123);
        let by_name = create_body("n", "cx22", "ubuntu-24.04", "fsn1", &[], "", &labels);
        assert_eq!(by_name["image"], "ubuntu-24.04");
    }

    #[test]
    fn parse_server_extracts_ip_status_created_and_labels() {
        let v = serde_json::json!({
            "id": 42,
            "name": "sz-dev-x1",
            "status": "running",
            "created": "2026-07-01T12:00:00+00:00",
            "public_net": { "ipv4": { "ip": "203.0.113.7" } },
            "labels": { "managed-by": "superzej", "sz-host": "abc" }
        });
        let s = parse_server(&v).unwrap();
        assert_eq!(s.id, "42");
        assert_eq!(s.name, "sz-dev-x1");
        assert!(s.running);
        assert_eq!(s.ip.as_deref(), Some("203.0.113.7"));
        assert!(s.created.is_some());
        assert_eq!(s.labels.get("sz-host").map(String::as_str), Some("abc"));

        // Booting: not running, no ip yet — the create poll keeps waiting.
        let boot = serde_json::json!({ "id": 43, "name": "b", "status": "initializing" });
        let s = parse_server(&boot).unwrap();
        assert!(!s.running);
        assert!(s.ip.is_none());
        assert!(s.created.is_none());
    }

    #[test]
    fn parse_create_and_list_unwrap_envelopes() {
        let created =
            serde_json::json!({ "server": { "id": 1, "name": "a", "status": "initializing" } });
        assert_eq!(parse_create(&created).unwrap().name, "a");
        let list = serde_json::json!({ "servers": [
            { "id": 1, "name": "a", "status": "running" },
            { "id": 2, "name": "b", "status": "off" },
            { "bogus": true }
        ]});
        let names: Vec<String> = parse_server_list(&list)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["a", "b"]);
        assert!(parse_server_list(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn ssh_key_shaping_round_trips() {
        let b = ssh_key_body("superzej-vps", "ssh-ed25519 AAAAC3 superzej");
        assert_eq!(b["name"], "superzej-vps");
        let listed = serde_json::json!({ "ssh_keys": [
            { "id": 9, "public_key": "ssh-ed25519 AAAAC3 other-comment" }
        ]});
        assert_eq!(
            parse_ssh_keys(&listed),
            vec![(9, "ssh-ed25519 AAAAC3 other-comment".to_string())]
        );
        assert_eq!(
            parse_ssh_key_created(&serde_json::json!({ "ssh_key": { "id": 12 } })),
            Some(12)
        );
        // Key-material match ignores the comment.
        assert!(same_pubkey(
            "ssh-ed25519 AAAAC3 superzej",
            "ssh-ed25519 AAAAC3 imported-2024"
        ));
        assert!(!same_pubkey("ssh-ed25519 AAAAC3 x", "ssh-ed25519 BBBBB4 x"));
        assert!(!same_pubkey("garbage", "ssh-ed25519 AAAAC3 x"));
    }

    #[test]
    fn image_bake_shaping() {
        assert_eq!(create_image_body("superzej-base")["type"], "snapshot");
        assert_eq!(
            parse_image_created(
                &serde_json::json!({ "action": {"id": 1}, "image": { "id": 777 } })
            ),
            Some(777)
        );
        assert_eq!(parse_image_created(&serde_json::json!({})), None);
    }
}
