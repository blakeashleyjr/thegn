//! DigitalOcean request/response shaping — **pure** functions only (URLs,
//! bodies, parsers), unit-tested without a live endpoint, mirroring
//! [`super::hetzner`]. The async HTTP wrappers live in [`super::VpsProvider`],
//! dispatched through the `super::VpsShaper` trait.
//!
//! API: `https://api.digitalocean.com/v2` (Bearer auth). Two shapes differ from
//! Hetzner and are handled here so the rest of the VPS machinery stays
//! vendor-agnostic:
//!
//! - **Tags are flat strings**, not `key=value` labels. superzej's managed
//!   marker + host scoping are encoded as the tag strings [`MANAGED_TAG`] and
//!   `sz-host:<hash>`, and [`parse_droplet`] reconstructs the same
//!   `managed-by`/`sz-host` label map the reaper reads.
//! - **Public IPv4** comes from `networks.v4[]` where `type == "public"`, not a
//!   fixed pointer.

use std::collections::BTreeMap;

use super::{HOST_KEY, MANAGED_KEY, MANAGED_VAL, VpsInstance};

pub const DEFAULT_API_BASE: &str = "https://api.digitalocean.com/v2";
/// Terraform/`doctl` convention. `doctl` also honours `DIGITALOCEAN_ACCESS_TOKEN`;
/// a user on that can point `[env.<name>.provider] api_key_env` at it.
pub const DEFAULT_TOKEN_ENV: &str = "DIGITALOCEAN_TOKEN";
/// Cheapest shared-vCPU regular Droplet with enough RAM for a dev shell.
pub const DEFAULT_SIZE: &str = "s-1vcpu-2gb";
pub const DEFAULT_REGION: &str = "nyc3";
pub const DEFAULT_IMAGE: &str = "ubuntu-24-04-x64";

/// The single tag every superzej-managed Droplet carries — the server-side
/// `list()` filter (`?tag_name=`), the reaper's coarse scope.
pub const MANAGED_TAG: &str = "sz-managed";
/// Host scoping is a `sz-host:<hash>` tag (DO tags allow colons); the reaper
/// filters on the reconstructed `sz-host` label.
pub const HOST_TAG_PREFIX: &str = "sz-host:";

pub fn droplets_url(base: &str) -> String {
    format!("{}/droplets", base.trim_end_matches('/'))
}

pub fn droplet_url(base: &str, id: &str) -> String {
    format!("{}/droplets/{id}", base.trim_end_matches('/'))
}

/// List only superzej-managed Droplets (single-tag server-side filter, mirroring
/// Hetzner's `label_selector`). Host scoping is applied client-side by the reaper.
pub fn list_url(base: &str) -> String {
    format!(
        "{}/droplets?tag_name={MANAGED_TAG}&per_page=200",
        base.trim_end_matches('/')
    )
}

pub fn ssh_keys_url(base: &str) -> String {
    format!("{}/account/keys", base.trim_end_matches('/'))
}

/// The per-Droplet action endpoint (snapshot / shutdown live here).
pub fn droplet_actions_url(base: &str, id: &str) -> String {
    format!("{}/droplets/{id}/actions", base.trim_end_matches('/'))
}

/// The global action-status endpoint — snapshot is async, so `snapshot()` polls
/// this until the action completes.
pub fn action_url(base: &str, action_id: &str) -> String {
    format!("{}/actions/{action_id}", base.trim_end_matches('/'))
}

/// A `template` of `snapshot:<id>` selects a baked snapshot image; anything else
/// is an image slug. Returns the snapshot id when present.
pub fn snapshot_image(template: &str) -> Option<&str> {
    template.trim().strip_prefix("snapshot:").map(str::trim)
}

/// DO tag strings for superzej's `managed-by` marker + `sz-host` scoping,
/// derived from the vendor-neutral label map `super::VpsProvider::labels`
/// builds. The inverse of [`labels_from_tags`].
pub fn tags_from_labels(labels: &BTreeMap<String, String>) -> Vec<String> {
    let mut tags = Vec::new();
    if labels.get(MANAGED_KEY).map(String::as_str) == Some(MANAGED_VAL) {
        tags.push(MANAGED_TAG.to_string());
    }
    if let Some(host) = labels.get(HOST_KEY) {
        tags.push(format!("{HOST_TAG_PREFIX}{host}"));
    }
    tags
}

/// Reconstruct the `managed-by`/`sz-host` label map from a Droplet's flat tags,
/// so the reaper's label-based scoping works unchanged across vendors.
pub fn labels_from_tags(tags: &[String]) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    for t in tags {
        if t == MANAGED_TAG {
            labels.insert(MANAGED_KEY.to_string(), MANAGED_VAL.to_string());
        } else if let Some(host) = t.strip_prefix(HOST_TAG_PREFIX) {
            labels.insert(HOST_KEY.to_string(), host.to_string());
        }
    }
    labels
}

/// The create body. `image` may be a slug (`ubuntu-24-04-x64`) or a numeric
/// snapshot id (sent as a JSON number — DO accepts either). `ssh_keys` are
/// account-key numeric ids. `user_data` is raw cloud-config.
pub fn create_body(
    name: &str,
    size: &str,
    image: &str,
    region: &str,
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
        "region": region,
        "size": size,
        "image": image_v,
        "ssh_keys": ssh_key_ids,
        "tags": tags_from_labels(labels),
    });
    if !user_data.trim().is_empty() {
        body["user_data"] = user_data.into();
    }
    body
}

/// The snapshot action body (`superzej env ln`). DO names snapshots, not
/// describes them.
pub fn snapshot_body(name: &str) -> serde_json::Value {
    serde_json::json!({ "type": "snapshot", "name": name })
}

/// The graceful power-off action body (the pre-snapshot quiesce).
pub fn shutdown_body() -> serde_json::Value {
    serde_json::json!({ "type": "shutdown" })
}

/// Parse one Droplet object into a [`VpsInstance`]. Status `active` ⇒ running;
/// the public IPv4 is the `networks.v4[]` entry whose `type` is `public`.
pub fn parse_droplet(v: &serde_json::Value) -> Option<VpsInstance> {
    let id = v.get("id").and_then(|i| i.as_i64())?;
    let name = v.get("name").and_then(|n| n.as_str())?.to_string();
    let running = v.get("status").and_then(|s| s.as_str()) == Some("active");
    let ip = v
        .pointer("/networks/v4")
        .and_then(|a| a.as_array())
        .and_then(|a| {
            a.iter()
                .find(|n| n.get("type").and_then(|t| t.as_str()) == Some("public"))
                .and_then(|n| n.get("ip_address").and_then(|i| i.as_str()))
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
    let created = v
        .get("created_at")
        .and_then(|c| c.as_str())
        .and_then(|c| chrono::DateTime::parse_from_rfc3339(c).ok())
        .map(|t| t.timestamp());
    let tags: Vec<String> = v
        .get("tags")
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some(VpsInstance {
        id: id.to_string(),
        name,
        ip,
        running,
        created,
        labels: labels_from_tags(&tags),
    })
}

/// Parse a create/get response (`{ "droplet": { … } }`).
pub fn parse_droplet_envelope(v: &serde_json::Value) -> Option<VpsInstance> {
    parse_droplet(v.get("droplet")?)
}

/// Parse a list response (`{ "droplets": [ … ] }`).
pub fn parse_droplet_list(v: &serde_json::Value) -> Vec<VpsInstance> {
    v.get("droplets")
        .and_then(|d| d.as_array())
        .map(|a| a.iter().filter_map(parse_droplet).collect())
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

/// Parse the id of a created key (`{ "ssh_key": { id } }`).
pub fn parse_ssh_key_created(v: &serde_json::Value) -> Option<i64> {
    v.pointer("/ssh_key/id").and_then(|i| i.as_i64())
}

/// Parse `(action_id, status)` from an action response (`{ "action": { id,
/// status } }`). Snapshot returns `in-progress`; `snapshot()` polls until
/// `completed`.
pub fn parse_action(v: &serde_json::Value) -> Option<(String, String)> {
    let a = v.get("action")?;
    let id = a.get("id").and_then(|i| {
        i.as_i64()
            .map(|n| n.to_string())
            .or_else(|| i.as_str().map(str::to_string))
    })?;
    let status = a.get("status").and_then(|s| s.as_str())?.to_string();
    Some((id, status))
}

/// The newest snapshot id off a Droplet get (`droplet.snapshot_ids[]`) — the
/// `template = "snapshot:<id>"` value `env ln` prints once the action completes.
pub fn parse_latest_snapshot_id(droplet_get: &serde_json::Value) -> Option<String> {
    droplet_get
        .pointer("/droplet/snapshot_ids")
        .and_then(|a| a.as_array())
        .and_then(|a| a.last())
        .and_then(|v| {
            v.as_i64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(str::to_string))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_versioned_and_tag_filtered() {
        assert_eq!(
            droplets_url("https://api.digitalocean.com/v2/"),
            "https://api.digitalocean.com/v2/droplets"
        );
        assert_eq!(
            droplet_url(DEFAULT_API_BASE, "42"),
            "https://api.digitalocean.com/v2/droplets/42"
        );
        assert_eq!(
            list_url(DEFAULT_API_BASE),
            "https://api.digitalocean.com/v2/droplets?tag_name=sz-managed&per_page=200"
        );
        assert_eq!(
            droplet_actions_url(DEFAULT_API_BASE, "7"),
            "https://api.digitalocean.com/v2/droplets/7/actions"
        );
        assert_eq!(
            action_url(DEFAULT_API_BASE, "99"),
            "https://api.digitalocean.com/v2/actions/99"
        );
        assert_eq!(
            ssh_keys_url(DEFAULT_API_BASE),
            "https://api.digitalocean.com/v2/account/keys"
        );
    }

    #[test]
    fn tags_round_trip_labels() {
        let mut labels = BTreeMap::new();
        labels.insert(MANAGED_KEY.to_string(), MANAGED_VAL.to_string());
        labels.insert(HOST_KEY.to_string(), "abc123".to_string());
        let tags = tags_from_labels(&labels);
        assert_eq!(tags, vec!["sz-managed", "sz-host:abc123"]);
        // Parsing the tags back reconstructs the same label map the reaper reads.
        assert_eq!(labels_from_tags(&tags), labels);
        // Unrelated user tags are ignored.
        let mixed = vec!["env:prod".into(), "sz-managed".into(), "sz-host:h9".into()];
        let back = labels_from_tags(&mixed);
        assert_eq!(back.get("managed-by").map(String::as_str), Some("superzej"));
        assert_eq!(back.get("sz-host").map(String::as_str), Some("h9"));
    }

    #[test]
    fn create_body_carries_name_region_size_image_keys_and_tags() {
        let mut labels = BTreeMap::new();
        labels.insert(MANAGED_KEY.to_string(), MANAGED_VAL.to_string());
        labels.insert(HOST_KEY.to_string(), "abc123".to_string());
        let b = create_body(
            "sz-dev-x1",
            "s-1vcpu-2gb",
            "ubuntu-24-04-x64",
            "nyc3",
            &[289794],
            "#cloud-config\n",
            &labels,
        );
        assert_eq!(b["name"], "sz-dev-x1");
        assert_eq!(b["size"], "s-1vcpu-2gb");
        assert_eq!(b["image"], "ubuntu-24-04-x64");
        assert_eq!(b["region"], "nyc3");
        assert_eq!(b["ssh_keys"], serde_json::json!([289794]));
        assert_eq!(
            b["tags"],
            serde_json::json!(["sz-managed", "sz-host:abc123"])
        );
        assert_eq!(b["user_data"], "#cloud-config\n");
        // Empty user_data is omitted; a numeric snapshot id is sent as a number.
        let b2 = create_body("n", "s", "555", "nyc3", &[], "", &labels);
        assert!(b2.get("user_data").is_none());
        assert_eq!(b2["image"], 555);
    }

    #[test]
    fn parse_droplet_extracts_public_ip_status_created_and_tags() {
        let v = serde_json::json!({
            "id": 3164494,
            "name": "sz-dev-x1",
            "status": "active",
            "created_at": "2026-07-01T12:00:00Z",
            "networks": { "v4": [
                { "ip_address": "10.0.0.2", "type": "private" },
                { "ip_address": "203.0.113.7", "type": "public" }
            ]},
            "tags": ["sz-managed", "sz-host:abc"]
        });
        let s = parse_droplet(&v).unwrap();
        assert_eq!(s.id, "3164494");
        assert_eq!(s.name, "sz-dev-x1");
        assert!(s.running);
        assert_eq!(s.ip.as_deref(), Some("203.0.113.7"));
        assert!(s.created.is_some());
        assert_eq!(s.labels.get("sz-host").map(String::as_str), Some("abc"));

        // Booting: status new, no public network yet — the create poll waits.
        let boot = serde_json::json!({ "id": 5, "name": "b", "status": "new" });
        let s = parse_droplet(&boot).unwrap();
        assert!(!s.running);
        assert!(s.ip.is_none());
    }

    #[test]
    fn parse_envelopes_and_list() {
        let created = serde_json::json!({ "droplet": { "id": 1, "name": "a", "status": "new" } });
        assert_eq!(parse_droplet_envelope(&created).unwrap().name, "a");
        let list = serde_json::json!({ "droplets": [
            { "id": 1, "name": "a", "status": "active" },
            { "id": 2, "name": "b", "status": "off" },
            { "bogus": true }
        ]});
        let names: Vec<String> = parse_droplet_list(&list)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["a", "b"]);
        assert!(parse_droplet_list(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn ssh_key_shaping() {
        let b = ssh_key_body("superzej-managed", "ssh-ed25519 AAAAC3 superzej");
        assert_eq!(b["name"], "superzej-managed");
        let listed = serde_json::json!({ "ssh_keys": [
            { "id": 289794, "public_key": "ssh-ed25519 AAAAC3 other" }
        ]});
        assert_eq!(
            parse_ssh_keys(&listed),
            vec![(289794, "ssh-ed25519 AAAAC3 other".to_string())]
        );
        assert_eq!(
            parse_ssh_key_created(&serde_json::json!({ "ssh_key": { "id": 12 } })),
            Some(12)
        );
    }

    #[test]
    fn snapshot_action_and_id_shaping() {
        assert_eq!(snapshot_body("superzej-base")["type"], "snapshot");
        assert_eq!(snapshot_body("superzej-base")["name"], "superzej-base");
        assert_eq!(shutdown_body()["type"], "shutdown");
        assert_eq!(
            parse_action(
                &serde_json::json!({ "action": { "id": 36804636, "status": "in-progress" } })
            ),
            Some(("36804636".to_string(), "in-progress".to_string()))
        );
        assert_eq!(
            parse_latest_snapshot_id(&serde_json::json!({
                "droplet": { "snapshot_ids": [111, 222] }
            })),
            Some("222".to_string())
        );
        assert_eq!(snapshot_image("snapshot:777"), Some("777"));
        assert_eq!(snapshot_image("ubuntu-24-04-x64"), None);
    }
}
