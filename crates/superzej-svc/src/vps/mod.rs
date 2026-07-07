//! Cheap-VPS execution backends (`[env.<name>.provider] provider = "hetzner"`,
//! DigitalOcean/Vultr to follow): provision a commodity VPS via the provider's
//! REST API, reach it over plain ssh, and run the standard provisioning
//! pipeline on it. Unlike Sprites there is **no suspend/checkpoint** — a
//! powered-off instance still bills — so the only free state is *destroyed*:
//! the warm-pool recycle path falls through to destroy (no `checkpoints` cap),
//! and the leak-safety ledger ([`registry`]) + label-scoped reaper make sure a
//! crash can't leave an instance billing forever.
//!
//! Module layout mirrors `provider.rs`: pure request shaping per provider
//! ([`hetzner`]), the ssh exec/files transport ([`ssh_shim`]), cloud-init
//! user-data ([`cloudinit`]), the instance ledger ([`registry`]), and the async
//! [`VpsProvider`] driving them.

pub mod cloudinit;
pub mod digitalocean;
pub mod hetzner;
pub mod registry;
pub mod ssh_shim;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};

use crate::provider::{ExecKind, FileEntry, ProviderFiles, RemoteProvider, SandboxHandle};
use superzej_core::remote::SshTarget;

/// The vendor-neutral instance labels every managed VPS carries. Hetzner sends
/// them as a native `key=value` label object; DigitalOcean (flat tags) encodes
/// and reconstructs them (see [`digitalocean::tags_from_labels`]). The reaper
/// scopes on [`HOST_KEY`], so both vendors must round-trip it.
pub const MANAGED_KEY: &str = "managed-by";
pub const MANAGED_VAL: &str = "superzej";
pub const HOST_KEY: &str = "sz-host";

/// Which VPS vendor an env targets. One enum arm per implemented adapter; each
/// pairs with a pure shaping module ([`hetzner`], [`digitalocean`]) reached via
/// `VpsKind::shaper` so the async driver holds no vendor `match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpsKind {
    Hetzner,
    DigitalOcean,
}

impl VpsKind {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "hetzner" => Some(VpsKind::Hetzner),
            "digitalocean" => Some(VpsKind::DigitalOcean),
            _ => None,
        }
    }

    pub fn api_base_default(self) -> &'static str {
        match self {
            VpsKind::Hetzner => hetzner::DEFAULT_API_BASE,
            VpsKind::DigitalOcean => digitalocean::DEFAULT_API_BASE,
        }
    }

    pub fn token_env_default(self) -> &'static str {
        match self {
            VpsKind::Hetzner => hetzner::DEFAULT_TOKEN_ENV,
            VpsKind::DigitalOcean => digitalocean::DEFAULT_TOKEN_ENV,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            VpsKind::Hetzner => "hetzner",
            VpsKind::DigitalOcean => "digitalocean",
        }
    }

    /// The pure request/response shaper for this vendor.
    pub(crate) fn shaper(self) -> &'static dyn VpsShaper {
        match self {
            VpsKind::Hetzner => &HETZNER_SHAPER,
            VpsKind::DigitalOcean => &DIGITALOCEAN_SHAPER,
        }
    }
}

/// Whether `name` names a VPS provider kind (the svc-side mirror of
/// `superzej_core::config::vps_provider_kind` — keep the two lists in sync).
pub fn is_vps_provider(name: &str) -> bool {
    VpsKind::parse(name).is_some()
}

/// Whether two OpenSSH public-key lines carry the same key material (compare
/// `type + blob`, ignoring the trailing comment — a registered key's comment
/// rarely matches ours). Vendor-neutral; used by `VpsProvider::ensure_ssh_key`.
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

/// Per-vendor **pure** request/response shaping (URLs, bodies, parsers) — one
/// impl per [`VpsKind`], reached via `VpsKind::shaper`. The async
/// [`VpsProvider`] calls only through this trait, so a new vendor is a new
/// module + a new arm, never a new `match` in the driver.
pub(crate) trait VpsShaper: Sync {
    fn default_region(&self) -> &'static str;
    fn default_size(&self) -> &'static str;
    fn default_image(&self) -> &'static str;
    /// `Some(id)` when `template` is `snapshot:<id>` (a baked image ⇒ keys-only
    /// cloud-init).
    fn snapshot_image<'a>(&self, template: &'a str) -> Option<&'a str>;

    fn servers_url(&self, base: &str) -> String;
    fn server_url(&self, base: &str, id: &str) -> String;
    fn list_url(&self, base: &str) -> String;
    fn ssh_keys_url(&self, base: &str) -> String;
    /// The endpoint + body for a graceful power-off (pre-snapshot quiesce).
    fn shutdown_url(&self, base: &str, id: &str) -> String;
    fn shutdown_body(&self) -> serde_json::Value;

    #[allow(clippy::too_many_arguments)]
    fn create_body(
        &self,
        name: &str,
        size: &str,
        image: &str,
        region: &str,
        ssh_key_ids: &[i64],
        user_data: &str,
        labels: &BTreeMap<String, String>,
    ) -> serde_json::Value;
    fn ssh_key_body(&self, name: &str, pubkey: &str) -> serde_json::Value;

    fn parse_create(&self, v: &serde_json::Value) -> Option<VpsInstance>;
    fn parse_get(&self, v: &serde_json::Value) -> Option<VpsInstance>;
    fn parse_server_list(&self, v: &serde_json::Value) -> Vec<VpsInstance>;
    fn parse_ssh_keys(&self, v: &serde_json::Value) -> Vec<(i64, String)>;
    fn parse_ssh_key_created(&self, v: &serde_json::Value) -> Option<i64>;
}

struct HetznerShaper;
static HETZNER_SHAPER: HetznerShaper = HetznerShaper;

impl VpsShaper for HetznerShaper {
    fn default_region(&self) -> &'static str {
        hetzner::DEFAULT_LOCATION
    }
    fn default_size(&self) -> &'static str {
        hetzner::DEFAULT_SERVER_TYPE
    }
    fn default_image(&self) -> &'static str {
        hetzner::DEFAULT_IMAGE
    }
    fn snapshot_image<'a>(&self, template: &'a str) -> Option<&'a str> {
        hetzner::snapshot_image(template)
    }
    fn servers_url(&self, base: &str) -> String {
        hetzner::servers_url(base)
    }
    fn server_url(&self, base: &str, id: &str) -> String {
        hetzner::server_url(base, id)
    }
    fn list_url(&self, base: &str) -> String {
        hetzner::list_url(base)
    }
    fn ssh_keys_url(&self, base: &str) -> String {
        hetzner::ssh_keys_url(base)
    }
    fn shutdown_url(&self, base: &str, id: &str) -> String {
        hetzner::shutdown_url(base, id)
    }
    fn shutdown_body(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn create_body(
        &self,
        name: &str,
        size: &str,
        image: &str,
        region: &str,
        ssh_key_ids: &[i64],
        user_data: &str,
        labels: &BTreeMap<String, String>,
    ) -> serde_json::Value {
        hetzner::create_body(name, size, image, region, ssh_key_ids, user_data, labels)
    }
    fn ssh_key_body(&self, name: &str, pubkey: &str) -> serde_json::Value {
        hetzner::ssh_key_body(name, pubkey)
    }
    fn parse_create(&self, v: &serde_json::Value) -> Option<VpsInstance> {
        hetzner::parse_create(v)
    }
    fn parse_get(&self, v: &serde_json::Value) -> Option<VpsInstance> {
        hetzner::parse_get(v)
    }
    fn parse_server_list(&self, v: &serde_json::Value) -> Vec<VpsInstance> {
        hetzner::parse_server_list(v)
    }
    fn parse_ssh_keys(&self, v: &serde_json::Value) -> Vec<(i64, String)> {
        hetzner::parse_ssh_keys(v)
    }
    fn parse_ssh_key_created(&self, v: &serde_json::Value) -> Option<i64> {
        hetzner::parse_ssh_key_created(v)
    }
}

struct DigitalOceanShaper;
static DIGITALOCEAN_SHAPER: DigitalOceanShaper = DigitalOceanShaper;

impl VpsShaper for DigitalOceanShaper {
    fn default_region(&self) -> &'static str {
        digitalocean::DEFAULT_REGION
    }
    fn default_size(&self) -> &'static str {
        digitalocean::DEFAULT_SIZE
    }
    fn default_image(&self) -> &'static str {
        digitalocean::DEFAULT_IMAGE
    }
    fn snapshot_image<'a>(&self, template: &'a str) -> Option<&'a str> {
        digitalocean::snapshot_image(template)
    }
    fn servers_url(&self, base: &str) -> String {
        digitalocean::droplets_url(base)
    }
    fn server_url(&self, base: &str, id: &str) -> String {
        digitalocean::droplet_url(base, id)
    }
    fn list_url(&self, base: &str) -> String {
        digitalocean::list_url(base)
    }
    fn ssh_keys_url(&self, base: &str) -> String {
        digitalocean::ssh_keys_url(base)
    }
    fn shutdown_url(&self, base: &str, id: &str) -> String {
        digitalocean::droplet_actions_url(base, id)
    }
    fn shutdown_body(&self) -> serde_json::Value {
        digitalocean::shutdown_body()
    }
    fn create_body(
        &self,
        name: &str,
        size: &str,
        image: &str,
        region: &str,
        ssh_key_ids: &[i64],
        user_data: &str,
        labels: &BTreeMap<String, String>,
    ) -> serde_json::Value {
        digitalocean::create_body(name, size, image, region, ssh_key_ids, user_data, labels)
    }
    fn ssh_key_body(&self, name: &str, pubkey: &str) -> serde_json::Value {
        digitalocean::ssh_key_body(name, pubkey)
    }
    fn parse_create(&self, v: &serde_json::Value) -> Option<VpsInstance> {
        digitalocean::parse_droplet_envelope(v)
    }
    fn parse_get(&self, v: &serde_json::Value) -> Option<VpsInstance> {
        digitalocean::parse_droplet_envelope(v)
    }
    fn parse_server_list(&self, v: &serde_json::Value) -> Vec<VpsInstance> {
        digitalocean::parse_droplet_list(v)
    }
    fn parse_ssh_keys(&self, v: &serde_json::Value) -> Vec<(i64, String)> {
        digitalocean::parse_ssh_keys(v)
    }
    fn parse_ssh_key_created(&self, v: &serde_json::Value) -> Option<i64> {
        digitalocean::parse_ssh_key_created(v)
    }
}

/// A stable, short label identifying THIS host, attached to every instance as
/// the `sz-host` label so two superzej hosts sharing one cloud account never
/// reap each other's sandboxes. Machine-id hash, hostname fallback.
pub fn host_label() -> String {
    let seed = std::fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown-host".to_string());
    superzej_core::util::short_hash(&seed, 10)
}

/// One provider instance as parsed from the vendor API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VpsInstance {
    /// The vendor's instance id (numeric for Hetzner, stringified).
    pub id: String,
    pub name: String,
    pub ip: Option<String>,
    pub running: bool,
    /// Creation time (unix seconds) — the reaper's age input.
    pub created: Option<i64>,
    pub labels: BTreeMap<String, String>,
}

/// Everything needed to drive one named instance (resolved host-side from
/// `[env.<name>.provider]` + the managed keypair).
#[derive(Debug, Clone)]
pub struct VpsSpec {
    pub kind: VpsKind,
    /// API base (empty ⇒ the kind's default).
    pub api_base: String,
    /// The resolved API token.
    pub token: String,
    /// The instance name to manage (the resolved sandbox id).
    pub name: String,
    /// Vendor region/location (empty ⇒ kind default).
    pub region: String,
    /// Vendor size/plan/server-type (empty ⇒ kind default).
    pub size: String,
    /// Image name or `snapshot:<id>` (empty ⇒ kind default stock image).
    pub image: String,
    /// Hard cap on concurrently-managed instances (0 ⇒ default 5) — the
    /// spend guardrail enforced at create.
    pub max_instances: u32,
    /// Ceiling on instance lifetime in seconds (0 ⇒ off) — the reaper enforces
    /// it from the instance's created timestamp.
    pub max_lifetime_secs: u64,
    /// Managed private key path + its OpenSSH public line (injected at create).
    pub key_path: PathBuf,
    pub pubkey: String,
    /// Test hook: skip the reachability wait after create (mock servers can't
    /// answer ssh). Never set outside tests.
    #[doc(hidden)]
    pub skip_ready_wait: bool,
}

impl VpsSpec {
    fn api_base(&self) -> String {
        let b = self.api_base.trim().trim_end_matches('/');
        if b.is_empty() {
            self.kind.api_base_default().to_string()
        } else {
            b.to_string()
        }
    }

    fn region(&self) -> &str {
        let r = self.region.trim();
        if r.is_empty() {
            self.kind.shaper().default_region()
        } else {
            r
        }
    }

    fn size(&self) -> &str {
        let s = self.size.trim();
        if s.is_empty() {
            self.kind.shaper().default_size()
        } else {
            s
        }
    }

    /// `(image argument, is_snapshot)` — a baked snapshot skips the cloud-init
    /// prereq installs.
    fn image(&self) -> (String, bool) {
        let shaper = self.kind.shaper();
        if let Some(id) = shaper.snapshot_image(&self.image) {
            return (id.to_string(), true);
        }
        let i = self.image.trim();
        if i.is_empty() {
            (shaper.default_image().to_string(), false)
        } else {
            (i.to_string(), false)
        }
    }

    fn max_instances(&self) -> usize {
        if self.max_instances == 0 {
            5
        } else {
            self.max_instances as usize
        }
    }
}

/// The remote user provisioning + attach run as. Stock cloud images boot with
/// root + the injected key; the pipeline resolves `$HOME` dynamically.
pub const VPS_USER: &str = "root";

/// The async driver: REST lifecycle + ssh exec/files for one named instance.
pub struct VpsProvider {
    spec: VpsSpec,
    client: reqwest::Client,
    /// Resolved public IP, cached per provider instance (registry, then API).
    ip: Mutex<Option<String>>,
}

impl VpsProvider {
    pub fn new(spec: VpsSpec) -> Self {
        VpsProvider {
            spec,
            client: crate::provider::provider_http_client(),
            ip: Mutex::new(None),
        }
    }

    pub fn spec(&self) -> &VpsSpec {
        &self.spec
    }

    fn shaper(&self) -> &'static dyn VpsShaper {
        self.spec.kind.shaper()
    }

    fn labels(&self) -> BTreeMap<String, String> {
        let mut l = BTreeMap::new();
        l.insert(MANAGED_KEY.into(), MANAGED_VAL.into());
        l.insert(HOST_KEY.into(), host_label());
        l
    }

    async fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        let resp = self
            .client
            .get(url)
            .bearer_auth(&self.spec.token)
            .send()
            .await
            .with_context(|| format!("vps: GET {url}"))?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            return Err(anyhow!("vps GET {url} failed ({status}): {body}"));
        }
        Ok(body)
    }

    async fn post_json(&self, url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let resp = self
            .client
            .post(url)
            .bearer_auth(&self.spec.token)
            .json(body)
            .send()
            .await
            .with_context(|| format!("vps: POST {url}"))?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            return Err(anyhow!("vps POST {url} failed ({status}): {body}"));
        }
        Ok(body)
    }

    /// All superzej-managed instances (label-filtered server-side), with
    /// ips/creation times — the reaper's view.
    pub async fn list_detailed(&self) -> Result<Vec<VpsInstance>> {
        let body = self
            .get_json(&self.shaper().list_url(&self.spec.api_base()))
            .await?;
        Ok(self.shaper().parse_server_list(&body))
    }

    /// Find one managed instance by name.
    async fn find_by_name(&self, name: &str) -> Result<Option<VpsInstance>> {
        Ok(self
            .list_detailed()
            .await?
            .into_iter()
            .find(|s| s.name == name))
    }

    /// Ensure the managed public key is registered, returning its vendor id.
    async fn ensure_ssh_key(&self) -> Result<i64> {
        let base = self.spec.api_base();
        let shaper = self.shaper();
        let listed = self.get_json(&shaper.ssh_keys_url(&base)).await?;
        if let Some((id, _)) = shaper
            .parse_ssh_keys(&listed)
            .into_iter()
            .find(|(_, pk)| same_pubkey(pk, &self.spec.pubkey))
        {
            return Ok(id);
        }
        // Name the registered key by its material fingerprint, not a fixed
        // "superzej-managed": vendor key names must be unique, so a fixed name
        // would 409 the moment the managed key material differs from an
        // already-registered same-named key (key rotation, or a stale key from a
        // reinstall). Keyed by fingerprint, distinct material ⇒ distinct name,
        // and identical material is already served by the `same_pubkey` fast path.
        let fp = superzej_core::util::short_hash(self.spec.pubkey.trim(), 8);
        let created = self
            .post_json(
                &shaper.ssh_keys_url(&base),
                &shaper.ssh_key_body(&format!("superzej-managed-{fp}"), self.spec.pubkey.trim()),
            )
            .await?;
        shaper
            .parse_ssh_key_created(&created)
            .ok_or_else(|| anyhow!("vps: no ssh key id in response: {created}"))
    }

    /// Resolve the instance's public IP: cache → registry → API (then persist).
    pub async fn resolve_ip(&self, name: &str) -> Result<String> {
        if let Some(ip) = self.ip.lock().unwrap().clone().filter(|i| !i.is_empty()) {
            return Ok(ip);
        }
        if let Some(rec) = registry::read(name)
            && rec.state == "ready"
            && !rec.ip.is_empty()
        {
            *self.ip.lock().unwrap() = Some(rec.ip.clone());
            return Ok(rec.ip);
        }
        let inst = self
            .find_by_name(name)
            .await?
            .ok_or_else(|| anyhow!("vps: instance {name} not found at the provider"))?;
        let ip = inst
            .ip
            .ok_or_else(|| anyhow!("vps: instance {name} has no public IPv4 yet"))?;
        let _ = registry::write(&registry::VpsRecord {
            name: name.to_string(),
            provider: self.spec.kind.as_str().into(),
            state: "ready".into(),
            instance_id: inst.id,
            ip: ip.clone(),
            created_at: inst.created.unwrap_or_else(superzej_core::util::now),
        });
        *self.ip.lock().unwrap() = Some(ip.clone());
        Ok(ip)
    }

    async fn shim(&self, name: &str) -> Result<ssh_shim::SshShim> {
        let ip = self.resolve_ip(name).await?;
        Ok(ssh_shim::SshShim {
            name: name.to_string(),
            ip,
            user: VPS_USER.into(),
            key_path: self.spec.key_path.clone(),
        })
    }

    /// Wait until the instance reports running + has an IP. Returns the
    /// instance. 2s poll, bounded.
    async fn wait_running(&self, id: &str) -> Result<VpsInstance> {
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(180);
        let base = self.spec.api_base();
        let start = std::time::Instant::now();
        loop {
            let body = self.get_json(&self.shaper().server_url(&base, id)).await?;
            if let Some(s) = self.shaper().parse_get(&body)
                && s.running
                && s.ip.is_some()
            {
                return Ok(s);
            }
            if start.elapsed() >= BUDGET {
                return Err(anyhow!(
                    "vps: instance {id} not running after {}s",
                    BUDGET.as_secs()
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    /// Wait for sshd (TCP :22) then for cloud-init to settle, so `create()`
    /// only returns an instance the pipeline can actually exec into.
    async fn wait_reachable(&self, name: &str, ip: &str) -> Result<()> {
        const TCP_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);
        let start = std::time::Instant::now();
        loop {
            match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                tokio::net::TcpStream::connect((ip, 22u16)),
            )
            .await
            {
                Ok(Ok(_)) => break,
                _ if start.elapsed() >= TCP_BUDGET => {
                    return Err(anyhow!(
                        "vps: {name} ({ip}) sshd not reachable after {}s",
                        TCP_BUDGET.as_secs()
                    ));
                }
                _ => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
            }
        }
        // cloud-init settle: tolerate absence (baked images) and "done with
        // warnings" (exit 2). Bounded — first boot installs docker on stock
        // images. Best-effort by design: a timeout here surfaces as provision
        // step failures with real diagnostics, not a create() hard-fail.
        let shim = ssh_shim::SshShim {
            name: name.to_string(),
            ip: ip.to_string(),
            user: VPS_USER.into(),
            key_path: self.spec.key_path.clone(),
        };
        let script = "command -v cloud-init >/dev/null 2>&1 && \
                      cloud-init status --wait >/dev/null 2>&1; true"
            .to_string();
        let argv = vec!["/bin/sh".to_string(), "-lc".to_string(), script];
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(240),
            shim.run_exec(&argv, None, &[]),
        )
        .await;
        Ok(())
    }

    /// Power the instance off via the API (graceful ACPI shutdown + poll) —
    /// the pre-snapshot quiesce for `superzej env image bake`.
    pub async fn poweroff(&self, name: &str) -> Result<()> {
        let inst = self
            .find_by_name(name)
            .await?
            .ok_or_else(|| anyhow!("vps: instance {name} not found"))?;
        let base = self.spec.api_base();
        let shaper = self.shaper();
        self.post_json(
            &shaper.shutdown_url(&base, &inst.id),
            &shaper.shutdown_body(),
        )
        .await?;
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(120);
        let start = std::time::Instant::now();
        loop {
            let body = self.get_json(&shaper.server_url(&base, &inst.id)).await?;
            let running = shaper.parse_get(&body).map(|s| s.running).unwrap_or(false);
            if !running {
                return Ok(());
            }
            if start.elapsed() >= BUDGET {
                return Err(anyhow!("vps: {name} still running after shutdown"));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    /// Snapshot the (stopped) instance, returning the vendor image id — the
    /// `template = "snapshot:<id>"` value `image bake` prints. Hetzner returns
    /// the id synchronously; DigitalOcean's snapshot is an async action (see
    /// `Self::snapshot_do`).
    pub async fn snapshot(&self, name: &str, description: &str) -> Result<String> {
        let inst = self
            .find_by_name(name)
            .await?
            .ok_or_else(|| anyhow!("vps: instance {name} not found"))?;
        let base = self.spec.api_base();
        match self.spec.kind {
            VpsKind::Hetzner => {
                let body = self
                    .post_json(
                        &hetzner::create_image_url(&base, &inst.id),
                        &hetzner::create_image_body(description),
                    )
                    .await?;
                hetzner::parse_image_created(&body)
                    .map(|id| id.to_string())
                    .ok_or_else(|| anyhow!("vps: no image id in snapshot response: {body}"))
            }
            VpsKind::DigitalOcean => self.snapshot_do(&base, &inst.id, description).await,
        }
    }

    /// DigitalOcean snapshots are asynchronous: POST the snapshot action, poll it
    /// to completion, then read the newest `snapshot_ids` off the Droplet.
    async fn snapshot_do(&self, base: &str, id: &str, name: &str) -> Result<String> {
        let action = self
            .post_json(
                &digitalocean::droplet_actions_url(base, id),
                &digitalocean::snapshot_body(name),
            )
            .await?;
        let (action_id, mut status) = digitalocean::parse_action(&action)
            .ok_or_else(|| anyhow!("vps: no action in DO snapshot response: {action}"))?;
        // Snapshotting a fresh dev image takes minutes; poll bounded.
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(600);
        let start = std::time::Instant::now();
        while status != "completed" {
            if status == "errored" {
                return Err(anyhow!("vps: DO snapshot action {action_id} errored"));
            }
            if start.elapsed() >= BUDGET {
                return Err(anyhow!(
                    "vps: DO snapshot action {action_id} not complete after {}s",
                    BUDGET.as_secs()
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let a = self
                .get_json(&digitalocean::action_url(base, &action_id))
                .await?;
            status = digitalocean::parse_action(&a)
                .map(|(_, s)| s)
                .unwrap_or(status);
        }
        let droplet = self.get_json(&digitalocean::droplet_url(base, id)).await?;
        digitalocean::parse_latest_snapshot_id(&droplet)
            .ok_or_else(|| anyhow!("vps: no snapshot id on droplet {id} after snapshot"))
    }
}

impl RemoteProvider for VpsProvider {
    async fn create(&self) -> Result<SandboxHandle> {
        let name = self.spec.name.trim().to_string();
        if name.is_empty() {
            return Err(anyhow!("vps: the sandbox name is empty"));
        }
        // Spend guardrail: never mint past the cap. Ledger-based (covers
        // in-flight creates the API can't see yet).
        let managed = registry::list().len();
        if managed >= self.spec.max_instances() {
            return Err(anyhow!(
                "vps: {managed} managed instances already exist (max_instances = {}); \
                 destroy one or raise `[env.<name>.provider] max_instances`",
                self.spec.max_instances()
            ));
        }
        let key_id = self.ensure_ssh_key().await?;
        let (image, is_snapshot) = self.spec.image();
        let user_data = cloudinit::user_data(&self.spec.pubkey, !is_snapshot);

        // Intent BEFORE the POST — the crash-between-create-and-record leak
        // window closes here; the reaper reconciles `creating` records.
        registry::write(&registry::VpsRecord {
            name: name.clone(),
            provider: self.spec.kind.as_str().into(),
            state: "creating".into(),
            instance_id: String::new(),
            ip: String::new(),
            created_at: superzej_core::util::now(),
        })?;

        let base = self.spec.api_base();
        let shaper = self.shaper();
        let body = shaper.create_body(
            &name,
            self.spec.size(),
            &image,
            self.spec.region(),
            &[key_id],
            &user_data,
            &self.labels(),
        );
        let created = match self.post_json(&shaper.servers_url(&base), &body).await {
            Ok(v) => v,
            Err(e) => {
                // A definite API rejection means no instance exists — clear the
                // intent record. A transport error is ambiguous: keep the record
                // for the reaper to reconcile.
                if e.to_string().contains("failed (4") {
                    registry::remove(&name);
                }
                return Err(e);
            }
        };
        let inst = shaper
            .parse_create(&created)
            .ok_or_else(|| anyhow!("vps: no server in create response: {created}"))?;

        let (ip, instance_id, created_at) = if self.spec.skip_ready_wait {
            (
                inst.ip.unwrap_or_default(),
                inst.id,
                inst.created.unwrap_or_else(superzej_core::util::now),
            )
        } else {
            let ready = self.wait_running(&inst.id).await?;
            let ip = ready.ip.clone().unwrap_or_default();
            self.wait_reachable(&name, &ip).await?;
            (
                ip,
                ready.id,
                ready.created.unwrap_or_else(superzej_core::util::now),
            )
        };
        registry::write(&registry::VpsRecord {
            name: name.clone(),
            provider: self.spec.kind.as_str().into(),
            state: "ready".into(),
            instance_id,
            ip: ip.clone(),
            created_at,
        })?;
        if !ip.is_empty() {
            *self.ip.lock().unwrap() = Some(ip.clone());
        }
        Ok(SandboxHandle {
            id: name,
            exec: ExecKind::Ssh(SshTarget {
                host: ip,
                port: 22,
                forward_agent: false,
            }),
        })
    }

    async fn destroy(&self, id: &str) -> Result<()> {
        // Resolve name → vendor instance id (registry first, then the API).
        let instance_id = match registry::read(id).filter(|r| !r.instance_id.is_empty()) {
            Some(r) => Some(r.instance_id),
            None => self.find_by_name(id).await.ok().flatten().map(|s| s.id),
        };
        let Some(iid) = instance_id else {
            // Nothing at the provider — clear any lingering ledger entry.
            registry::remove(id);
            return Ok(());
        };
        // Retry transient statuses: a leaked VPS bills forever (same policy as
        // the sprites destroy).
        const ATTEMPTS: u32 = 3;
        let base = self.spec.api_base();
        let url = self.shaper().server_url(&base, &iid);
        let mut last_status = None;
        for attempt in 0..ATTEMPTS {
            let resp = self
                .client
                .delete(&url)
                .bearer_auth(&self.spec.token)
                .send()
                .await
                .context("vps: DELETE /servers/{id}")?;
            let status = resp.status();
            if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
                registry::remove(id);
                return Ok(());
            }
            last_status = Some(status);
            if !crate::provider::transient_status(status) {
                break;
            }
            if attempt + 1 < ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        Err(anyhow!(
            "vps destroy {id} failed ({})",
            last_status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "no response".into())
        ))
    }

    async fn list(&self) -> Result<Vec<String>> {
        Ok(self
            .list_detailed()
            .await?
            .into_iter()
            .map(|s| s.name)
            .collect())
    }
}

impl VpsProvider {
    /// One-shot exec `(exit_code, combined output)` over ssh — the pipeline's
    /// control-plane primitive (mirrors `SpritesProvider::run_exec`).
    pub async fn run_exec(
        &self,
        id: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<(i32, String)> {
        self.shim(id).await?.run_exec(argv, cwd, env).await
    }
}

impl ProviderFiles for VpsProvider {
    async fn read(&self, id: &str, path: &str) -> Result<Vec<u8>> {
        self.shim(id).await?.read(path).await
    }

    async fn write(&self, id: &str, path: &str, data: &[u8]) -> Result<()> {
        self.shim(id).await?.write(path, data, "0644").await
    }

    async fn write_exec(&self, id: &str, path: &str, data: &[u8]) -> Result<()> {
        self.shim(id).await?.write(path, data, "0755").await
    }

    async fn list_dir(&self, id: &str, path: &str) -> Result<Vec<FileEntry>> {
        self.shim(id).await?.list_dir(path).await
    }

    async fn delete(&self, id: &str, path: &str) -> Result<()> {
        self.shim(id).await?.delete(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_parse_and_defaults() {
        assert_eq!(VpsKind::parse("hetzner"), Some(VpsKind::Hetzner));
        assert_eq!(VpsKind::parse(" hetzner "), Some(VpsKind::Hetzner));
        assert_eq!(VpsKind::parse("digitalocean"), Some(VpsKind::DigitalOcean));
        assert_eq!(VpsKind::parse("sprites"), None);
        assert!(is_vps_provider("hetzner"));
        assert!(is_vps_provider("digitalocean"));
        assert!(!is_vps_provider("daytona"));
        assert_eq!(VpsKind::Hetzner.token_env_default(), "HCLOUD_TOKEN");
        assert_eq!(
            VpsKind::DigitalOcean.token_env_default(),
            "DIGITALOCEAN_TOKEN"
        );
        assert_eq!(VpsKind::DigitalOcean.as_str(), "digitalocean");
    }

    #[test]
    fn digitalocean_spec_defaults_route_through_shaper() {
        let spec = VpsSpec {
            kind: VpsKind::DigitalOcean,
            api_base: String::new(),
            token: "t".into(),
            name: "n".into(),
            region: String::new(),
            size: String::new(),
            image: String::new(),
            max_instances: 0,
            max_lifetime_secs: 0,
            key_path: "/k".into(),
            pubkey: "ssh-ed25519 A".into(),
            skip_ready_wait: true,
        };
        assert_eq!(spec.api_base(), digitalocean::DEFAULT_API_BASE);
        assert_eq!(spec.region(), "nyc3");
        assert_eq!(spec.size(), "s-1vcpu-2gb");
        assert_eq!(spec.image(), ("ubuntu-24-04-x64".to_string(), false));
        // Snapshot template still flips the keys-only cloud-init flag.
        let snap = VpsSpec {
            image: "snapshot:555".into(),
            ..spec
        };
        assert_eq!(snap.image(), ("555".to_string(), true));
    }

    #[test]
    fn same_pubkey_ignores_comment() {
        assert!(same_pubkey(
            "ssh-ed25519 AAAAC3 superzej",
            "ssh-ed25519 AAAAC3 imported-2024"
        ));
        assert!(!same_pubkey("ssh-ed25519 AAAAC3 x", "ssh-ed25519 BBBBB4 x"));
        assert!(!same_pubkey("garbage", "ssh-ed25519 AAAAC3 x"));
    }

    #[test]
    fn spec_defaults_fill_region_size_image_and_cap() {
        let spec = VpsSpec {
            kind: VpsKind::Hetzner,
            api_base: String::new(),
            token: "t".into(),
            name: "n".into(),
            region: String::new(),
            size: String::new(),
            image: String::new(),
            max_instances: 0,
            max_lifetime_secs: 0,
            key_path: "/k".into(),
            pubkey: "ssh-ed25519 A".into(),
            skip_ready_wait: true,
        };
        assert_eq!(spec.api_base(), hetzner::DEFAULT_API_BASE);
        assert_eq!(spec.region(), "fsn1");
        assert_eq!(spec.size(), "cx23");
        assert_eq!(spec.image(), ("ubuntu-24.04".to_string(), false));
        assert_eq!(
            spec.max_instances(),
            5,
            "0 means the default cap, not unlimited"
        );
        // Snapshot template flips the is_snapshot flag (keys-only cloud-init).
        let snap = VpsSpec {
            image: "snapshot:777".into(),
            ..spec
        };
        assert_eq!(snap.image(), ("777".to_string(), true));
    }

    #[test]
    fn host_label_is_stable_and_short() {
        let a = host_label();
        assert_eq!(a, host_label(), "stable per host");
        assert_eq!(a.len(), 10);
    }
}
