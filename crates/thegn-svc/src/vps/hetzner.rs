//! Hetzner Cloud request/response shaping — **pure** functions only (URLs,
//! bodies, parsers), unit-tested without a live endpoint, mirroring the
//! `DaytonaProvider`/`SpritesProvider` split in `provider.rs`. The async HTTP
//! wrappers live in [`super::VpsProvider`].
//!
//! API: `https://api.hetzner.cloud/v1` (Bearer auth). Servers are created by
//! name with labels; we filter lists by the `managed-by=thegn` label so
//! `list()` never sees a user's unrelated servers.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};

use super::{InventoryPage, VpsInstance};

pub const DEFAULT_API_BASE: &str = "https://api.hetzner.cloud/v1";
pub const DEFAULT_TOKEN_ENV: &str = "HCLOUD_TOKEN";
/// Cheapest current shared-vCPU type (2 vCPU / 4 GB, x86) — a sane dev default.
/// (`cx22` was deprecated by Hetzner; `cx23` is its same-spec successor.)
pub const DEFAULT_SERVER_TYPE: &str = "cx23";
pub const DEFAULT_LOCATION: &str = "fsn1";
pub const DEFAULT_IMAGE: &str = "ubuntu-24.04";

/// The label every thegn-managed instance carries (reaper filter).
pub const MANAGED_LABEL: &str = "managed-by";
pub const MANAGED_VALUE: &str = "thegn";
/// The label scoping an instance to the creating host (two hosts sharing one
/// Hetzner project must never reap each other's sandboxes).
pub const HOST_LABEL: &str = "tg-host";

pub fn servers_url(base: &str) -> String {
    format!("{}/servers", base.trim_end_matches('/'))
}

pub fn server_url(base: &str, id: &str) -> String {
    format!("{}/servers/{id}", base.trim_end_matches('/'))
}

/// List only thegn-managed servers (label selector, server-side filter).
pub fn list_url(base: &str) -> String {
    format!(
        "{}/servers?label_selector={MANAGED_LABEL}%3D{MANAGED_VALUE}&per_page=50",
        base.trim_end_matches('/')
    )
}

pub fn list_page_url(base: &str, page: u32) -> String {
    format!("{}&page={page}", list_url(base))
}

pub fn ssh_keys_url(base: &str) -> String {
    format!("{}/ssh_keys", base.trim_end_matches('/'))
}

pub fn ssh_keys_page_url(base: &str, page: u32) -> String {
    format!("{}?page={page}&per_page=50", ssh_keys_url(base))
}

pub fn create_image_url(base: &str, id: &str) -> String {
    format!(
        "{}/servers/{id}/actions/create_image",
        base.trim_end_matches('/')
    )
}

pub fn server_action_url(base: &str, server_id: &str, action_id: i64) -> String {
    format!(
        "{}/servers/{server_id}/actions/{action_id}",
        base.trim_end_matches('/')
    )
}

pub fn image_url(base: &str, image_id: i64) -> String {
    format!("{}/images/{image_id}", base.trim_end_matches('/'))
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

/// Parse one strict server inventory page. Errors mention row indexes only;
/// provider row contents may contain addresses or other sensitive data.
pub fn parse_server_page(v: &serde_json::Value, page: u32) -> Result<InventoryPage<VpsInstance>> {
    parse_inventory_page(v, page, "servers", parse_server)
}

/// Parse one strict SSH-key inventory page.
pub fn parse_ssh_key_page(
    v: &serde_json::Value,
    page: u32,
) -> Result<InventoryPage<(i64, String)>> {
    let rows = v
        .get("ssh_keys")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("Hetzner inventory page is missing the ssh_keys array"))?;
    let mut items = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let key = row
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .zip(row.get("public_key").and_then(serde_json::Value::as_str))
            .map(|(id, key)| (id, key.to_string()))
            .ok_or_else(|| {
                anyhow!("Hetzner inventory page has malformed element at index {index}")
            })?;
        items.push(key);
    }
    let pagination = hetzner_pagination(v, page)?;
    Ok(InventoryPage {
        items,
        has_next: pagination.0,
        total: pagination.1,
    })
}

fn parse_inventory_page<T>(
    v: &serde_json::Value,
    page: u32,
    array_name: &str,
    parse_row: fn(&serde_json::Value) -> Option<T>,
) -> Result<InventoryPage<T>> {
    let rows = v
        .get(array_name)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("Hetzner inventory page is missing the {array_name} array"))?;
    let mut items = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        items.push(parse_row(row).ok_or_else(|| {
            anyhow!("Hetzner inventory page has malformed element at index {index}")
        })?);
    }
    let pagination = hetzner_pagination(v, page)?;
    Ok(InventoryPage {
        items,
        has_next: pagination.0,
        total: pagination.1,
    })
}

fn hetzner_pagination(v: &serde_json::Value, page: u32) -> Result<(bool, Option<u64>)> {
    let pagination = v
        .pointer("/meta/pagination")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow!("Hetzner inventory page is missing meta.pagination"))?;
    let has_next = match pagination.get("next_page") {
        Some(serde_json::Value::Null) => false,
        Some(value) if value.as_u64() == Some(u64::from(page) + 1) => true,
        _ => return Err(anyhow!("Hetzner inventory has a non-advancing page")),
    };
    let total = pagination
        .get("total_entries")
        .and_then(serde_json::Value::as_u64);
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

/// Parse the id out of a created ssh key (`{ "ssh_key": { id } }`).
pub fn parse_ssh_key_created(v: &serde_json::Value) -> Option<i64> {
    v.pointer("/ssh_key/id").and_then(|i| i.as_i64())
}

/// The create-image (snapshot) body for `thegn env image bake`.
pub fn create_image_body(description: &str) -> serde_json::Value {
    serde_json::json!({ "type": "snapshot", "description": description })
}

/// Parse both operation and result ids from a create-image response.
pub fn parse_create_image(v: &serde_json::Value) -> Option<(i64, i64)> {
    Some((
        v.pointer("/action/id")?.as_i64()?,
        v.pointer("/image/id")?.as_i64()?,
    ))
}

pub fn parse_action_status(v: &serde_json::Value) -> Option<String> {
    match v.pointer("/action/status")?.as_str()? {
        status @ ("running" | "success" | "error") => Some(status.to_string()),
        _ => None,
    }
}

pub fn parse_image_state(v: &serde_json::Value) -> Option<(String, Option<i64>)> {
    let status = v.pointer("/image/status")?.as_str()?.to_string();
    let source = match v.pointer("/image/created_from/id") {
        Some(id) => Some(id.as_i64()?),
        None => None,
    };
    Some((status, source))
}

pub fn image_belongs_to_server(created_from: Option<i64>, server_id: &str) -> bool {
    matches!(server_id.parse::<i64>(), Ok(id) if created_from == Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_page(
        rows: serde_json::Value,
        page: u32,
        next: serde_json::Value,
        total: u64,
    ) -> serde_json::Value {
        serde_json::json!({
            "servers": rows,
            "meta": {"pagination": {
                "page": page,
                "per_page": 50,
                "previous_page": if page == 1 { serde_json::Value::Null } else { serde_json::json!(page - 1) },
                "next_page": next,
                "last_page": 2,
                "total_entries": total
            }}
        })
    }

    #[test]
    fn strict_server_pages_return_both_pages_and_reject_malformed_shapes() {
        let first = server_page(
            serde_json::json!([
                {"id": 1, "name": "one"}, {"id": 2, "name": "two"}
            ]),
            1,
            serde_json::json!(2),
            3,
        );
        let last = server_page(
            serde_json::json!([{"id": 3, "name": "three"}]),
            2,
            serde_json::Value::Null,
            3,
        );
        let first = parse_server_page(&first, 1).unwrap();
        let last = parse_server_page(&last, 2).unwrap();
        assert_eq!(first.items.len(), 2);
        assert!(first.has_next);
        assert_eq!(last.items.len(), 1);
        assert!(!last.has_next);
        assert_eq!(last.total, Some(3));

        let malformed = server_page(
            serde_json::json!([{"bogus": true}]),
            1,
            serde_json::json!(2),
            1,
        );
        assert!(parse_server_page(&malformed, 1).is_err());
        let repeated = server_page(serde_json::json!([]), 1, serde_json::json!(1), 0);
        assert!(parse_server_page(&repeated, 1).is_err());
        let skipped = server_page(serde_json::json!([]), 1, serde_json::json!(3), 0);
        assert!(parse_server_page(&skipped, 1).is_err());
        assert!(parse_server_page(&serde_json::json!({"servers": []}), 1).is_err());
        let missing_servers = serde_json::json!({"meta": {"pagination": {
            "next_page": null, "total_entries": 0
        }}});
        assert!(parse_server_page(&missing_servers, 1).is_err());
    }

    #[test]
    fn strict_ssh_key_pages_reject_missing_and_malformed_rows() {
        let page = serde_json::json!({"meta": {"pagination": {
            "next_page": null, "total_entries": 1
        }}});
        assert!(parse_ssh_key_page(&page, 1).is_err());

        let malformed = serde_json::json!({"ssh_keys": [{"id": "not-an-id", "public_key": "ssh-ed25519 AAA"}],
            "meta": {"pagination": {"next_page": null, "total_entries": 1}}});
        assert!(parse_ssh_key_page(&malformed, 1).is_err());
    }

    #[test]
    fn pagination_accepts_optional_vendor_metadata() {
        for pagination in [
            serde_json::json!({"next_page": null, "total_entries": 0}),
            serde_json::json!({"page": null, "per_page": 17, "previous_page": null,
                "next_page": null, "last_page": null, "total_entries": 0}),
            serde_json::json!({"per_page": 17, "next_page": null, "last_page": 0,
                "total_entries": 0}),
        ] {
            let body = serde_json::json!({"servers": [], "meta": {"pagination": pagination}});
            let parsed = parse_server_page(&body, 1).unwrap();
            assert!(!parsed.has_next);
            assert_eq!(parsed.total, Some(0));
        }
    }

    #[test]
    fn strict_server_and_key_page_urls_pin_provider_pagination() {
        assert!(list_page_url(DEFAULT_API_BASE, 2).ends_with("&per_page=50&page=2"));
        assert!(ssh_keys_page_url(DEFAULT_API_BASE, 2).ends_with("?page=2&per_page=50"));
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
            row["created"] = timestamp;
            let instance = parse_server(&row).unwrap();
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
    fn urls_are_versioned_and_label_filtered() {
        assert_eq!(
            servers_url("https://api.hetzner.cloud/v1/"),
            "https://api.hetzner.cloud/v1/servers"
        );
        assert_eq!(
            server_url(DEFAULT_API_BASE, "42"),
            "https://api.hetzner.cloud/v1/servers/42"
        );
        // List filters server-side to thegn-managed instances only.
        assert_eq!(
            list_url(DEFAULT_API_BASE),
            "https://api.hetzner.cloud/v1/servers?label_selector=managed-by%3Dthegn&per_page=50"
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
            "tg-dev-x1",
            "cx22",
            "ubuntu-24.04",
            "fsn1",
            &[101],
            "#cloud-config\n",
            &labels,
        );
        assert_eq!(b["name"], "tg-dev-x1");
        assert_eq!(b["server_type"], "cx22");
        assert_eq!(b["image"], "ubuntu-24.04");
        assert_eq!(b["location"], "fsn1");
        assert_eq!(b["ssh_keys"], serde_json::json!([101]));
        assert_eq!(b["labels"]["managed-by"], "thegn");
        assert_eq!(b["labels"]["tg-host"], "abc123");
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
            "name": "tg-dev-x1",
            "status": "running",
            "created": "2026-07-01T12:00:00+00:00",
            "public_net": { "ipv4": { "ip": "203.0.113.7" } },
            "labels": { "managed-by": "thegn", "tg-host": "abc" }
        });
        let s = parse_server(&v).unwrap();
        assert_eq!(s.id, "42");
        assert_eq!(s.name, "tg-dev-x1");
        assert!(s.running);
        assert_eq!(s.ip.as_deref(), Some("203.0.113.7"));
        assert!(s.created.is_some());
        assert_eq!(s.labels.get("tg-host").map(String::as_str), Some("abc"));

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
        let b = ssh_key_body("thegn-vps", "ssh-ed25519 AAAAC3 thegn");
        assert_eq!(b["name"], "thegn-vps");
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
        // Key-material match (shared, vendor-neutral) ignores the comment.
        use super::super::same_pubkey;
        assert!(same_pubkey(
            "ssh-ed25519 AAAAC3 thegn",
            "ssh-ed25519 AAAAC3 imported-2024"
        ));
        assert!(!same_pubkey("ssh-ed25519 AAAAC3 x", "ssh-ed25519 BBBBB4 x"));
        assert!(!same_pubkey("garbage", "ssh-ed25519 AAAAC3 x"));
    }

    #[test]
    fn image_bake_shaping() {
        assert_eq!(create_image_body("thegn-base")["type"], "snapshot");
        assert_eq!(
            parse_create_image(&serde_json::json!({ "action": {"id": 1}, "image": { "id": 777 } })),
            Some((1, 777))
        );
        assert_eq!(parse_create_image(&serde_json::json!({})), None);
        assert_eq!(
            parse_create_image(&serde_json::json!({"action":{"id":1}})),
            None
        );
    }

    #[test]
    fn snapshot_bind_create_image_requires_action_and_image() {
        assert_eq!(
            parse_create_image(&serde_json::json!({"action":{"id":4},"image":{"id":8}})),
            Some((4, 8))
        );
        assert_eq!(
            parse_create_image(&serde_json::json!({"image":{"id":8}})),
            None
        );
        assert_eq!(
            parse_create_image(&serde_json::json!({"action":{"id":4}})),
            None
        );
    }

    #[test]
    fn snapshot_bind_image_state_reads_created_from() {
        assert_eq!(
            parse_image_state(
                &serde_json::json!({"image":{"status":"available","created_from":{"id":7}}})
            ),
            Some(("available".into(), Some(7)))
        );
        assert_eq!(
            parse_image_state(&serde_json::json!({"image":{"status":"creating"}})),
            Some(("creating".into(), None))
        );
        assert_eq!(parse_image_state(&serde_json::json!({"image":{}})), None);
    }

    #[test]
    fn snapshot_bind_action_success_refuses_image_from_another_server() {
        let (status, created_from) = parse_image_state(
            &serde_json::json!({"image":{"status":"available","created_from":{"id":8}}}),
        )
        .unwrap();
        assert_eq!(status, "available");
        assert!(!image_belongs_to_server(created_from, "7"));
    }

    #[test]
    fn snapshot_bind_urls() {
        assert_eq!(
            server_action_url(DEFAULT_API_BASE, "7", 12),
            "https://api.hetzner.cloud/v1/servers/7/actions/12"
        );
        assert_eq!(
            image_url(DEFAULT_API_BASE, 99),
            "https://api.hetzner.cloud/v1/images/99"
        );
    }

    #[test]
    fn snapshot_bind_action_status_parses_terminal_states() {
        assert_eq!(
            parse_action_status(&serde_json::json!({"action":{"status":"running"}})),
            Some("running".into())
        );
        assert_eq!(
            parse_action_status(&serde_json::json!({"action":{"status":"success"}})),
            Some("success".into())
        );
        assert_eq!(
            parse_action_status(&serde_json::json!({"action":{"status":"error"}})),
            Some("error".into())
        );
        assert_eq!(
            parse_action_status(&serde_json::json!({"action":{"status":"mystery"}})),
            None
        );
    }
}
