//! DigitalOcean request/response shaping — **pure** functions only (URLs,
//! bodies, parsers), unit-tested without a live endpoint, mirroring
//! [`super::hetzner`]. The async HTTP wrappers live in [`super::VpsProvider`],
//! dispatched through the `super::VpsShaper` trait.
//!
//! API: `https://api.digitalocean.com/v2` (Bearer auth). Two shapes differ from
//! Hetzner and are handled here so the rest of the VPS machinery stays
//! vendor-agnostic:
//!
//! - **Tags are flat strings**, not `key=value` labels. thegn's managed
//!   marker + host scoping are encoded as the tag strings [`MANAGED_TAG`] and
//!   `tg-host:<hash>`, and [`parse_droplet`] reconstructs the same
//!   `managed-by`/`tg-host` label map the reaper reads.
//! - **Public IPv4** comes from `networks.v4[]` where `type == "public"`, not a
//!   fixed pointer.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};

use super::{HOST_KEY, InventoryPage, MANAGED_KEY, MANAGED_VAL, VpsInstance};

pub const DEFAULT_API_BASE: &str = "https://api.digitalocean.com/v2";
/// Terraform/`doctl` convention. `doctl` also honours `DIGITALOCEAN_ACCESS_TOKEN`;
/// a user on that can point `[env.<name>.provider] api_key_env` at it.
pub const DEFAULT_TOKEN_ENV: &str = "DIGITALOCEAN_TOKEN";
/// Cheapest shared-vCPU regular Droplet with enough RAM for a dev shell.
pub const DEFAULT_SIZE: &str = "s-1vcpu-2gb";
pub const DEFAULT_REGION: &str = "nyc3";
pub const DEFAULT_IMAGE: &str = "ubuntu-24-04-x64";

/// The single tag every thegn-managed Droplet carries — the server-side
/// `list()` filter (`?tag_name=`), the reaper's coarse scope.
pub const MANAGED_TAG: &str = "tg-managed";
/// Host scoping is a `tg-host:<hash>` tag (DO tags allow colons); the reaper
/// filters on the reconstructed `tg-host` label.
pub const HOST_TAG_PREFIX: &str = "tg-host:";

pub fn droplets_url(base: &str) -> String {
    format!("{}/droplets", base.trim_end_matches('/'))
}

pub fn droplet_url(base: &str, id: &str) -> String {
    format!("{}/droplets/{id}", base.trim_end_matches('/'))
}

/// List only thegn-managed Droplets (single-tag server-side filter, mirroring
/// Hetzner's `label_selector`). Host scoping is applied client-side by the reaper.
pub fn list_url(base: &str) -> String {
    format!(
        "{}/droplets?tag_name={MANAGED_TAG}&per_page=200",
        base.trim_end_matches('/')
    )
}

pub fn list_page_url(base: &str, page: u32) -> String {
    format!("{}&page={page}", list_url(base))
}

pub fn ssh_keys_url(base: &str) -> String {
    format!("{}/account/keys", base.trim_end_matches('/'))
}

pub fn ssh_keys_page_url(base: &str, page: u32) -> String {
    format!("{}?page={page}&per_page=200", ssh_keys_url(base))
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

/// DO tag strings for thegn's `managed-by` marker + `tg-host` scoping,
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

/// Reconstruct the `managed-by`/`tg-host` label map from a Droplet's flat tags,
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

/// The snapshot action body (`thegn env ln`). DO names snapshots, not
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

/// Parse one strict Droplet inventory page. Errors mention row indexes only;
/// provider row contents may contain addresses or other sensitive data.
pub fn parse_server_page(v: &serde_json::Value, _page: u32) -> Result<InventoryPage<VpsInstance>> {
    parse_inventory_page(v, "droplets", parse_droplet)
}

/// Parse one strict SSH-key inventory page.
pub fn parse_ssh_key_page(
    v: &serde_json::Value,
    _page: u32,
) -> Result<InventoryPage<(i64, String)>> {
    let rows = v
        .get("ssh_keys")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("DigitalOcean inventory page is missing the ssh_keys array"))?;
    let mut items = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let key = row
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .zip(row.get("public_key").and_then(serde_json::Value::as_str))
            .map(|(id, key)| (id, key.to_string()))
            .ok_or_else(|| {
                anyhow!("DigitalOcean inventory page has malformed element at index {index}")
            })?;
        items.push(key);
    }
    let (has_next, total) = digitalocean_pagination(v)?;
    Ok(InventoryPage {
        items,
        has_next,
        total,
    })
}

fn parse_inventory_page<T>(
    v: &serde_json::Value,
    array_name: &str,
    parse_row: fn(&serde_json::Value) -> Option<T>,
) -> Result<InventoryPage<T>> {
    let rows = v
        .get(array_name)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("DigitalOcean inventory page is missing the {array_name} array"))?;
    let mut items = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        items.push(parse_row(row).ok_or_else(|| {
            anyhow!("DigitalOcean inventory page has malformed element at index {index}")
        })?);
    }
    let (has_next, total) = digitalocean_pagination(v)?;
    Ok(InventoryPage {
        items,
        has_next,
        total,
    })
}

fn digitalocean_pagination(v: &serde_json::Value) -> Result<(bool, Option<u64>)> {
    if v.get("meta")
        .and_then(serde_json::Value::as_object)
        .is_none()
    {
        return Err(anyhow!("DigitalOcean inventory page is missing meta"));
    }
    let has_next = match v.get("links") {
        None | Some(serde_json::Value::Null) => false,
        Some(links) => {
            if links.as_object().is_none() {
                return Err(anyhow!("DigitalOcean inventory page has malformed links"));
            }
            let Some(pages) = links.get("pages") else {
                return Ok((
                    false,
                    v.pointer("/meta/total").and_then(serde_json::Value::as_u64),
                ));
            };
            let pages = pages
                .as_object()
                .ok_or_else(|| anyhow!("DigitalOcean inventory page has malformed links.pages"))?;
            match pages.get("next") {
                None | Some(serde_json::Value::Null) => false,
                Some(serde_json::Value::String(_)) => true,
                _ => {
                    return Err(anyhow!(
                        "DigitalOcean inventory page has malformed links.pages.next"
                    ));
                }
            }
        }
    };
    let total = v.pointer("/meta/total").and_then(serde_json::Value::as_u64);
    Ok((has_next, total))
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

#[derive(Debug, PartialEq, Eq)]
pub struct DoAction {
    pub id: String,
    pub status: String,
    pub kind: String,
    pub resource_id: String,
}

fn parse_id(v: &serde_json::Value) -> Option<String> {
    v.as_i64()
        .map(|n| n.to_string())
        .or_else(|| v.as_str().filter(|id| !id.is_empty()).map(str::to_string))
}

/// Parse the complete action identity needed to bind a snapshot to its droplet.
pub fn parse_action_detail(v: &serde_json::Value) -> Option<DoAction> {
    let a = v.get("action")?;
    Some(DoAction {
        id: parse_id(a.get("id")?)?,
        status: a.get("status")?.as_str()?.to_string(),
        kind: a.get("type")?.as_str()?.to_string(),
        resource_id: parse_id(a.get("resource_id")?)?,
    })
}

/// Parse the legacy action id and status pair used by non-snapshot callers.
pub fn parse_action(v: &serde_json::Value) -> Option<(String, String)> {
    let action = v.get("action")?;
    Some((
        parse_id(action.get("id")?)?,
        action.get("status")?.as_str()?.to_string(),
    ))
}

/// Parse every snapshot id off a Droplet get (`droplet.snapshot_ids[]`).
pub fn parse_snapshot_ids(droplet_get: &serde_json::Value) -> Option<Vec<String>> {
    droplet_get
        .pointer("/droplet/snapshot_ids")
        .and_then(|a| a.as_array())
        .map(|ids| ids.iter().map(parse_id).collect())?
}

#[derive(Debug, PartialEq, Eq)]
pub enum SnapshotBindError {
    NoNewSnapshot,
    AmbiguousNewSnapshots(usize),
}

impl std::fmt::Display for SnapshotBindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoNewSnapshot => f.write_str("no new snapshot id appeared on the droplet"),
            Self::AmbiguousNewSnapshots(count) => {
                write!(
                    f,
                    "{count} new snapshot ids appeared (concurrent snapshot?)"
                )
            }
        }
    }
}

impl std::error::Error for SnapshotBindError {}

/// No-new-id is the only bind result that can become valid on a later inventory read.
pub fn should_retry_snapshot_bind(
    error: &SnapshotBindError,
    budget_remaining: std::time::Duration,
) -> bool {
    matches!(error, SnapshotBindError::NoNewSnapshot) && !budget_remaining.is_zero()
}

/// Bind a completed snapshot action to the sole id newly visible on the droplet.
pub fn new_snapshot_id(before: &[String], after: &[String]) -> Result<String, SnapshotBindError> {
    let added: Vec<&String> = after
        .iter()
        .enumerate()
        .filter(|(index, id)| !before.contains(id) && !after[..*index].contains(id))
        .map(|(_, id)| id)
        .collect();
    let Some(id) = added.first() else {
        return Err(SnapshotBindError::NoNewSnapshot);
    };
    if added.len() != 1 {
        return Err(SnapshotBindError::AmbiguousNewSnapshots(added.len()));
    }
    Ok((*id).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn droplet_page(rows: serde_json::Value, next: bool, total: u64) -> serde_json::Value {
        let mut value = serde_json::json!({"droplets": rows, "meta": {"total": total}});
        if next {
            value["links"] = serde_json::json!({"pages": {
                "next": "https://api.digitalocean.com/v2/droplets?page=2"
            }});
        }
        value
    }

    #[test]
    fn strict_server_pages_return_both_pages_and_reject_malformed_shapes() {
        let first = droplet_page(
            serde_json::json!([{"id": 1, "name": "one"}, {"id": 2, "name": "two"}]),
            true,
            3,
        );
        let last = droplet_page(serde_json::json!([{"id": 3, "name": "three"}]), false, 3);
        let first = parse_server_page(&first, 1).unwrap();
        let last = parse_server_page(&last, 2).unwrap();
        assert_eq!(first.items.len(), 2);
        assert!(first.has_next);
        assert_eq!(last.items.len(), 1);
        assert!(!last.has_next);
        assert_eq!(last.total, Some(3));

        let malformed = droplet_page(serde_json::json!([{"bogus": true}]), true, 1);
        assert!(parse_server_page(&malformed, 1).is_err());
        assert!(parse_server_page(&serde_json::json!({"droplets": []}), 1).is_err());
        let missing_droplets = serde_json::json!({"meta": {"total": 0}});
        assert!(parse_server_page(&missing_droplets, 1).is_err());
    }

    #[test]
    fn strict_ssh_key_pages_reject_missing_and_malformed_rows() {
        let page = serde_json::json!({"meta": {"total": 0}});
        assert!(parse_ssh_key_page(&page, 1).is_err());

        let malformed = serde_json::json!({"ssh_keys": [{"id": "not-an-id", "public_key": "ssh-ed25519 AAA"}],
            "meta": {"total": 1}});
        assert!(parse_ssh_key_page(&malformed, 1).is_err());
    }

    #[test]
    fn strict_server_and_key_page_urls_pin_provider_pagination() {
        assert!(list_page_url(DEFAULT_API_BASE, 2).ends_with("&per_page=200&page=2"));
        assert!(ssh_keys_page_url(DEFAULT_API_BASE, 2).ends_with("?page=2&per_page=200"));
    }

    #[test]
    fn unknown_and_future_provider_creation_times_never_authorize_reaping() {
        for timestamp in [
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!("bad"),
            serde_json::json!("1960-01-01T00:00:00Z"),
            serde_json::json!("2099-01-01T00:00:00Z"),
        ] {
            let mut row = serde_json::json!({"id":42,"name":"fixture"});
            row["created_at"] = timestamp;
            let instance = parse_droplet(&row).unwrap();
            let decision = thegn_core::time_policy::resource_expiry(
                1_800_000_000,
                instance.created,
                60,
                Some(1200),
            );
            assert!(matches!(
                decision,
                thegn_core::time_policy::ResourceExpiry::Quarantine(_)
            ));
        }
    }

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
            "https://api.digitalocean.com/v2/droplets?tag_name=tg-managed&per_page=200"
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
        assert_eq!(tags, vec!["tg-managed", "tg-host:abc123"]);
        // Parsing the tags back reconstructs the same label map the reaper reads.
        assert_eq!(labels_from_tags(&tags), labels);
        // Unrelated user tags are ignored.
        let mixed = vec!["env:prod".into(), "tg-managed".into(), "tg-host:h9".into()];
        let back = labels_from_tags(&mixed);
        assert_eq!(back.get("managed-by").map(String::as_str), Some("thegn"));
        assert_eq!(back.get("tg-host").map(String::as_str), Some("h9"));
    }

    #[test]
    fn create_body_carries_name_region_size_image_keys_and_tags() {
        let mut labels = BTreeMap::new();
        labels.insert(MANAGED_KEY.to_string(), MANAGED_VAL.to_string());
        labels.insert(HOST_KEY.to_string(), "abc123".to_string());
        let b = create_body(
            "tg-dev-x1",
            "s-1vcpu-2gb",
            "ubuntu-24-04-x64",
            "nyc3",
            &[289794],
            "#cloud-config\n",
            &labels,
        );
        assert_eq!(b["name"], "tg-dev-x1");
        assert_eq!(b["size"], "s-1vcpu-2gb");
        assert_eq!(b["image"], "ubuntu-24-04-x64");
        assert_eq!(b["region"], "nyc3");
        assert_eq!(b["ssh_keys"], serde_json::json!([289794]));
        assert_eq!(
            b["tags"],
            serde_json::json!(["tg-managed", "tg-host:abc123"])
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
            "name": "tg-dev-x1",
            "status": "active",
            "created_at": "2026-07-01T12:00:00Z",
            "networks": { "v4": [
                { "ip_address": "10.0.0.2", "type": "private" },
                { "ip_address": "203.0.113.7", "type": "public" }
            ]},
            "tags": ["tg-managed", "tg-host:abc"]
        });
        let s = parse_droplet(&v).unwrap();
        assert_eq!(s.id, "3164494");
        assert_eq!(s.name, "tg-dev-x1");
        assert!(s.running);
        assert_eq!(s.ip.as_deref(), Some("203.0.113.7"));
        assert!(s.created.is_some());
        assert_eq!(s.labels.get("tg-host").map(String::as_str), Some("abc"));

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
        let b = ssh_key_body("thegn-managed", "ssh-ed25519 AAAAC3 thegn");
        assert_eq!(b["name"], "thegn-managed");
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
        assert_eq!(snapshot_body("thegn-base")["type"], "snapshot");
        assert_eq!(snapshot_body("thegn-base")["name"], "thegn-base");
        assert_eq!(shutdown_body()["type"], "shutdown");
        assert_eq!(
            parse_action(
                &serde_json::json!({ "action": { "id": 36804636, "status": "in-progress" } })
            ),
            Some(("36804636".to_string(), "in-progress".to_string()))
        );
        assert_eq!(
            parse_snapshot_ids(&serde_json::json!({
                "droplet": { "snapshot_ids": [111, 222] }
            })),
            Some(vec!["111".to_string(), "222".to_string()])
        );
        assert_eq!(snapshot_image("snapshot:777"), Some("777"));
        assert_eq!(snapshot_image("ubuntu-24-04-x64"), None);
    }

    #[test]
    fn snapshot_bind_picks_the_single_new_id_regardless_of_order() {
        assert_eq!(
            new_snapshot_id(
                &["111".into(), "222".into()],
                &["333".into(), "111".into(), "222".into()]
            ),
            Ok("333".into())
        );
    }

    #[test]
    fn snapshot_bind_refuses_no_new_id() {
        assert_eq!(
            new_snapshot_id(&["111".into(), "222".into()], &["222".into(), "111".into()]),
            Err(SnapshotBindError::NoNewSnapshot)
        );
    }

    #[test]
    fn snapshot_bind_refuses_two_new_ids() {
        assert_eq!(
            new_snapshot_id(&["111".into()], &["222".into(), "333".into(), "111".into()]),
            Err(SnapshotBindError::AmbiguousNewSnapshots(2))
        );
    }

    #[test]
    fn snapshot_bind_errors_are_user_readable() {
        assert_eq!(
            SnapshotBindError::NoNewSnapshot.to_string(),
            "no new snapshot id appeared on the droplet"
        );
        assert_eq!(
            SnapshotBindError::AmbiguousNewSnapshots(2).to_string(),
            "2 new snapshot ids appeared (concurrent snapshot?)"
        );
    }

    #[test]
    fn snapshot_bind_retries_only_no_new_id_while_budget_remains() {
        let step = std::time::Duration::from_secs(5);
        assert!(should_retry_snapshot_bind(
            &SnapshotBindError::NoNewSnapshot,
            step
        ));
        assert!(!should_retry_snapshot_bind(
            &SnapshotBindError::AmbiguousNewSnapshots(2),
            step
        ));
        assert!(!should_retry_snapshot_bind(
            &SnapshotBindError::NoNewSnapshot,
            std::time::Duration::ZERO
        ));
    }

    #[test]
    fn snapshot_bind_strict_snapshot_ids_reject_non_ids() {
        assert_eq!(
            parse_snapshot_ids(&serde_json::json!({"droplet":{"snapshot_ids":[1,"2"]}})),
            Some(vec!["1".into(), "2".into()])
        );
        assert_eq!(
            parse_snapshot_ids(&serde_json::json!({"droplet":{"snapshot_ids":[1,null]}})),
            None
        );
    }

    #[test]
    fn snapshot_bind_action_detail_requires_type_and_resource() {
        assert_eq!(
            parse_action_detail(
                &serde_json::json!({"action":{"id":1,"status":"completed","type":"snapshot","resource_id":7}})
            ),
            Some(DoAction {
                id: "1".into(),
                status: "completed".into(),
                kind: "snapshot".into(),
                resource_id: "7".into()
            })
        );
        assert_eq!(
            parse_action_detail(
                &serde_json::json!({"action":{"id":1,"status":"completed","resource_id":7}})
            ),
            None
        );
    }
}
