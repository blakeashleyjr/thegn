//! Fly Machines API request/response shaping — **pure** functions only (URLs,
//! bodies, parsers), unit-tested without a live endpoint, mirroring the
//! `hetzner`/`digitalocean` split in `crate::vps`.
//!
//! API: `https://api.machines.dev/v1` (Bearer auth). A Machine belongs to a Fly
//! **app**, so the lifecycle is: ensure the app exists → create the machine →
//! poll it to `started`. Machines are container-native and get a private 6PN
//! address (`private_ip`, `fdaa:…`) reachable over the org WireGuard mesh — the
//! CLI-free transport (see [`super::wireguard`]) dials that address.
//!
//! Scoping is by **machine metadata** (`managed-by=superzej`, `sz-host=<hash>`),
//! the container-native analogue of Hetzner labels / DO tags; `list()` filters
//! on it client-side (the Machines list endpoint has no server-side selector).

use std::collections::BTreeMap;

pub const DEFAULT_API_BASE: &str = "https://api.machines.dev/v1";
pub const DEFAULT_TOKEN_ENV: &str = "FLY_API_TOKEN";
/// A small always-on preset (shared-cpu-1x ≈ 1 shared vCPU / 256 MB); dev shells
/// usually want more RAM, so callers typically override `size`.
pub const DEFAULT_SIZE: &str = "shared-cpu-2x";
pub const DEFAULT_REGION: &str = "iad";
/// Stock base image: a full-ish userland with an sshd the provisioning pipeline
/// reaches over the 6PN transport. Overridable via `template`.
pub const DEFAULT_IMAGE: &str = "ubuntu:24.04";

/// Metadata keys — the vendor-neutral scoping [`crate::vps::MANAGED_KEY`] mirrors.
pub const MANAGED_KEY: &str = "managed-by";
pub const MANAGED_VAL: &str = "superzej";
pub const HOST_KEY: &str = "sz-host";

pub fn apps_url(base: &str) -> String {
    format!("{}/apps", base.trim_end_matches('/'))
}

pub fn app_url(base: &str, app: &str) -> String {
    format!("{}/apps/{app}", base.trim_end_matches('/'))
}

pub fn machines_url(base: &str, app: &str) -> String {
    format!("{}/apps/{app}/machines", base.trim_end_matches('/'))
}

pub fn machine_url(base: &str, app: &str, id: &str) -> String {
    format!("{}/apps/{app}/machines/{id}", base.trim_end_matches('/'))
}

pub fn machine_action_url(base: &str, app: &str, id: &str, action: &str) -> String {
    format!(
        "{}/apps/{app}/machines/{id}/{action}",
        base.trim_end_matches('/')
    )
}

/// The `.../machines/{id}/wait?state=<state>&timeout=<secs>` long-poll used
/// after create/start.
pub fn machine_wait_url(base: &str, app: &str, id: &str, state: &str, timeout_secs: u32) -> String {
    format!(
        "{}/apps/{app}/machines/{id}/wait?state={state}&timeout={timeout_secs}",
        base.trim_end_matches('/')
    )
}

/// The create-app body. Fly app names are globally unique, so superzej derives a
/// stable per-sandbox name (see `super::app_name`).
pub fn create_app_body(app_name: &str, org_slug: &str) -> serde_json::Value {
    serde_json::json!({ "app_name": app_name, "org_slug": org_slug })
}

/// The stop-action body: SIGTERM (sshd-as-PID1 exits on it — the default SIGINT
/// it ignores, so the machine would only stop on the kill timeout), bounded.
pub fn stop_body() -> serde_json::Value {
    serde_json::json!({ "signal": "SIGTERM", "timeout": "30s" })
}

/// A `template` of `image:<ref>` or a bare registry ref selects the image; unlike
/// a VPS there is no snapshot concept (Fly speed comes from small images).
pub fn image_ref(template: &str) -> Option<&str> {
    let t = template.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.strip_prefix("image:").map(str::trim).unwrap_or(t))
    }
}

/// Map a Fly size preset (`shared-cpu-2x`, `performance-1x`, …) to a guest
/// `{cpu_kind, cpus, memory_mb}`. Unknown presets fall back to shared-cpu-1x so a
/// typo degrades to the cheapest machine, never a create failure.
pub fn guest_for_size(size: &str) -> serde_json::Value {
    let s = size.trim();
    let (kind, cpus, mem) = match s {
        "shared-cpu-1x" => ("shared", 1, 256),
        "shared-cpu-2x" => ("shared", 2, 512),
        "shared-cpu-4x" => ("shared", 4, 1024),
        "shared-cpu-8x" => ("shared", 8, 2048),
        "performance-1x" => ("performance", 1, 2048),
        "performance-2x" => ("performance", 2, 4096),
        "performance-4x" => ("performance", 4, 8192),
        _ => ("shared", 1, 256),
    };
    serde_json::json!({ "cpu_kind": kind, "cpus": cpus, "memory_mb": mem })
}

/// The internal + external ssh port (per-sandbox app ⇒ a dedicated IPv4, so the
/// standard port is unambiguous).
pub const SSH_PORT: u16 = 22;

/// Guest init (`config.init.exec`, runs as PID 1): a Fly machine has no cloud-init
/// and the stock image has no sshd, so this installs the same prereqs the VPS
/// cloud-init does (openssh-server + curl/ca-certificates), preps docker for the
/// **`vfs`** storage driver (Fly's rootfs can't nest overlayfs — the default
/// overlay driver fails), fixes key perms, and execs sshd in the foreground so
/// the machine stays up as an ssh box (the same model superzej gives a VPS).
/// Verified live. `superzej env image-bake` can later fold this into an image.
pub const SSHD_INIT: &str = "set -e; export DEBIAN_FRONTEND=noninteractive; \
apt-get update -qq; \
apt-get install -y -qq openssh-server curl ca-certificates >/dev/null 2>&1; \
mkdir -p /run/sshd /etc/docker; \
printf '{\"features\":{\"containerd-snapshotter\":false},\"storage-driver\":\"vfs\"}\\n' > /etc/docker/daemon.json; \
chmod 700 /root/.ssh; chmod 600 /root/.ssh/authorized_keys; \
ssh-keygen -A; exec /usr/sbin/sshd -D -e";

/// The create-machine body. Since a Fly machine has no plain public-IP ssh and
/// no cloud-init, superzej reaches it like a VPS: `authorized_key` rides in via a
/// Machines `files` entry (base64), a `tcp/22` service exposes sshd on the app's
/// dedicated IPv4, and [`SSHD_INIT`] brings sshd up. Reachability then reuses the
/// VPS `ssh_shim` verbatim — no WireGuard, no vendor CLI.
pub fn create_machine_body(
    name: &str,
    region: &str,
    image: &str,
    size: &str,
    authorized_key: &str,
    metadata: &BTreeMap<String, String>,
) -> serde_json::Value {
    let meta: serde_json::Map<String, serde_json::Value> = metadata
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    let authkeys_b64 = super::b64(authorized_key.trim().as_bytes());
    serde_json::json!({
        "name": name,
        "region": region,
        "config": {
            "image": image,
            "guest": guest_for_size(size),
            "metadata": meta,
            "auto_destroy": false,
            // Don't let Fly auto-restart a machine superzej parked (scale-to-zero).
            "restart": { "policy": "no" },
            "files": [
                { "guest_path": "/root/.ssh/authorized_keys", "raw_value": authkeys_b64 }
            ],
            "services": [
                {
                    "protocol": "tcp",
                    "internal_port": SSH_PORT,
                    "ports": [ { "port": SSH_PORT } ]
                }
            ],
            "init": { "exec": ["/bin/sh", "-c", SSHD_INIT] }
        }
    })
}

/// One Machine as parsed from the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlyMachine {
    pub id: String,
    pub name: String,
    /// `created` | `starting` | `started` | `stopping` | `stopped` | `destroyed`.
    pub state: String,
    /// The 6PN private address (`fdaa:…`) the transport dials.
    pub private_ip: Option<String>,
    pub region: Option<String>,
    pub metadata: BTreeMap<String, String>,
}

impl FlyMachine {
    pub fn is_started(&self) -> bool {
        self.state == "started"
    }
}

pub fn parse_machine(v: &serde_json::Value) -> Option<FlyMachine> {
    let id = v.get("id").and_then(|i| i.as_str())?.to_string();
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or_default()
        .to_string();
    let state = v
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    let private_ip = v
        .get("private_ip")
        .and_then(|i| i.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let region = v.get("region").and_then(|r| r.as_str()).map(str::to_string);
    let metadata = v
        .pointer("/config/metadata")
        .and_then(|m| m.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Some(FlyMachine {
        id,
        name,
        state,
        private_ip,
        region,
        metadata,
    })
}

/// Parse a list response (`[ {machine}, … ]`), keeping only superzej-managed
/// machines (client-side metadata filter — the endpoint has no selector).
pub fn parse_machine_list(v: &serde_json::Value) -> Vec<FlyMachine> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(parse_machine)
                .filter(|m| m.metadata.get(MANAGED_KEY).map(String::as_str) == Some(MANAGED_VAL))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_app_scoped() {
        assert_eq!(
            apps_url(DEFAULT_API_BASE),
            "https://api.machines.dev/v1/apps"
        );
        assert_eq!(
            machines_url(DEFAULT_API_BASE, "sz-app"),
            "https://api.machines.dev/v1/apps/sz-app/machines"
        );
        assert_eq!(
            machine_url(DEFAULT_API_BASE, "sz-app", "abc"),
            "https://api.machines.dev/v1/apps/sz-app/machines/abc"
        );
        assert_eq!(
            machine_action_url(DEFAULT_API_BASE, "sz-app", "abc", "stop"),
            "https://api.machines.dev/v1/apps/sz-app/machines/abc/stop"
        );
        assert_eq!(
            machine_wait_url(DEFAULT_API_BASE, "sz-app", "abc", "started", 60),
            "https://api.machines.dev/v1/apps/sz-app/machines/abc/wait?state=started&timeout=60"
        );
    }

    #[test]
    fn image_ref_strips_prefix() {
        assert_eq!(image_ref("image:ubuntu:24.04"), Some("ubuntu:24.04"));
        assert_eq!(
            image_ref("registry.fly.io/x:deployment-1"),
            Some("registry.fly.io/x:deployment-1")
        );
        assert_eq!(image_ref("  "), None);
    }

    #[test]
    fn guest_maps_known_presets_and_falls_back() {
        assert_eq!(guest_for_size("shared-cpu-2x")["memory_mb"], 512);
        assert_eq!(guest_for_size("performance-1x")["cpu_kind"], "performance");
        // Unknown → cheapest shared machine, not a failure.
        assert_eq!(guest_for_size("nonsense")["cpu_kind"], "shared");
        assert_eq!(guest_for_size("nonsense")["cpus"], 1);
    }

    #[test]
    fn create_machine_body_scopes_metadata_and_wires_sshd() {
        let mut meta = BTreeMap::new();
        meta.insert(MANAGED_KEY.to_string(), MANAGED_VAL.to_string());
        meta.insert(HOST_KEY.to_string(), "h9".to_string());
        let b = create_machine_body(
            "sz-fly-1",
            "iad",
            "ubuntu:24.04",
            "shared-cpu-2x",
            "ssh-ed25519 AAAAKEY superzej",
            &meta,
        );
        assert_eq!(b["name"], "sz-fly-1");
        assert_eq!(b["config"]["image"], "ubuntu:24.04");
        assert_eq!(b["config"]["guest"]["memory_mb"], 512);
        assert_eq!(b["config"]["metadata"]["sz-host"], "h9");
        // sshd wiring: key file (base64), tcp/22 service, init that starts sshd.
        assert_eq!(
            b["config"]["files"][0]["guest_path"],
            "/root/.ssh/authorized_keys"
        );
        let decoded =
            super::super::b64_decode(b["config"]["files"][0]["raw_value"].as_str().unwrap())
                .unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "ssh-ed25519 AAAAKEY superzej"
        );
        assert_eq!(b["config"]["services"][0]["internal_port"], 22);
        assert_eq!(b["config"]["services"][0]["ports"][0]["port"], 22);
        assert!(
            b["config"]["init"]["exec"][2]
                .as_str()
                .unwrap()
                .contains("sshd")
        );
        assert!(
            b["config"]["init"]["exec"][2]
                .as_str()
                .unwrap()
                .contains("vfs")
        );
    }

    #[test]
    fn parse_machine_extracts_state_ip_and_metadata() {
        let v = serde_json::json!({
            "id": "17811953",
            "name": "sz-fly-1",
            "state": "started",
            "region": "iad",
            "private_ip": "fdaa:0:1:a7b:1:2:3:4",
            "config": { "metadata": { "managed-by": "superzej", "sz-host": "h9" } }
        });
        let m = parse_machine(&v).unwrap();
        assert_eq!(m.id, "17811953");
        assert!(m.is_started());
        assert_eq!(m.private_ip.as_deref(), Some("fdaa:0:1:a7b:1:2:3:4"));
        assert_eq!(m.metadata.get("sz-host").map(String::as_str), Some("h9"));
    }

    #[test]
    fn list_keeps_only_managed_machines() {
        let list = serde_json::json!([
            { "id": "a", "name": "mine", "state": "started",
              "config": { "metadata": { "managed-by": "superzej" } } },
            { "id": "b", "name": "someone-elses", "state": "started",
              "config": { "metadata": { "managed-by": "other" } } },
            { "id": "c", "name": "unmanaged", "state": "started" }
        ]);
        let names: Vec<String> = parse_machine_list(&list)
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, vec!["mine"]);
    }
}
