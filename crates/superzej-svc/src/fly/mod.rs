//! Fly.io as a managed-sandbox **provider** — CLI-free (no `flyctl`).
//!
//! A Fly machine is container-native with **no plain public-IP ssh**, so Fly is
//! its own [`crate::provider::Provider`] variant (not a `VpsKind`). But once it
//! is *reachable*, it behaves like a VPS, so this provider deliberately reuses
//! the VPS reachability stack:
//!
//! - **Lifecycle** ([`machines`]) — the Machines REST API drives one app per
//!   sandbox: create app → allocate a dedicated public **IPv4** (via
//!   [`graphql`], the one thing Machines REST lacks) → create a machine whose
//!   [`machines::SSHD_INIT`] brings up sshd with superzej's managed key on a
//!   `tcp/22` service. Destroy deletes the app (cascading the machine + IP).
//! - **Reachability** — plain ssh to the app's IPv4:22, so exec/files reuse the
//!   VPS [`crate::vps::ssh_shim`] and the leak-safety
//!   [`crate::vps::registry`] **verbatim**. No WireGuard, no vendor CLI.
//!
//! A Fly machine is a real Firecracker VM (its own kernel), so the standard
//! provisioning pipeline — nix, direnv, **docker** (with the `vfs` storage driver
//! [`machines::SSHD_INIT`] preconfigures) — runs over ssh exactly as on a VPS.
//! Scale-to-zero is real (a stopped machine bills only for rootfs).

pub mod graphql;
pub mod machines;

use std::collections::BTreeMap;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use base64::Engine;

use crate::provider::{ExecKind, FileEntry, ProviderFiles, RemoteProvider, SandboxHandle};
use crate::vps::{host_label, registry, ssh_shim};
use superzej_core::remote::SshTarget;

/// The ledger `provider` tag for Fly records (kept apart from VPS records that
/// share the same on-disk registry).
const LEDGER_PROVIDER: &str = "fly";
/// The remote user (Fly machines boot as root; sshd admits the injected key).
const FLY_USER: &str = "root";

pub(crate) fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
pub(crate) fn b64_decode(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(Into::into)
}

/// The Fly app that hosts one sandbox's machine: one app **per sandbox** so its
/// dedicated IPv4 unambiguously addresses that machine (and destroy = delete the
/// app, releasing the IP). Globally unique (Fly app names are), host- + name-
/// scoped so two hosts never collide.
fn app_name(sandbox: &str) -> String {
    format!(
        "sz-{}",
        superzej_core::util::short_hash(&format!("{}-{sandbox}", host_label()), 12)
    )
}

/// Everything needed to drive one named Fly machine.
#[derive(Debug, Clone)]
pub struct FlySpec {
    /// Machines API base (empty ⇒ [`machines::DEFAULT_API_BASE`]).
    pub api_base: String,
    /// Fly GraphQL endpoint (empty ⇒ [`graphql::DEFAULT_GRAPHQL_URL`]).
    pub graphql_url: String,
    pub token: String,
    /// Fly organization slug (empty ⇒ `"personal"`).
    pub org_slug: String,
    /// The machine/sandbox name to manage (the resolved sandbox id).
    pub name: String,
    /// Fly region (empty ⇒ kind default).
    pub region: String,
    /// Fly size preset (empty ⇒ kind default; see [`machines::guest_for_size`]).
    pub size: String,
    /// Image ref (empty ⇒ default stock image; `image:<ref>` or a bare ref).
    pub image: String,
    /// Hard cap on concurrently-managed machines (0 ⇒ default 5).
    pub max_instances: u32,
    /// Ceiling on machine lifetime in seconds (0 ⇒ off).
    pub max_lifetime_secs: u64,
    /// Managed private key path + its OpenSSH public line (as for the VPS shim).
    pub key_path: std::path::PathBuf,
    pub pubkey: String,
    /// Optional iroh call-home injection: when `Some`, the created machine gets
    /// the three `SUPERZEJ_*` env vars the baked `sz-agent` reads on boot to dial
    /// home over iroh. `None` ⇒ today's ssh/IPv4-only behavior, unchanged.
    pub iroh: Option<IrohInject>,
    /// Test hook: skip the post-create ssh-readiness wait. Never set outside tests.
    #[doc(hidden)]
    pub skip_ready_wait: bool,
}

/// The three iroh call-home values injected into a Fly machine's environment so
/// the baked `sz-agent` (see `nix/fly-sandbox-image.nix`) can reach the
/// compositor. The env-var *keys* come from `superzej_core::iroh_wire`; these are
/// the per-sandbox *values* the host mints at provision time.
#[derive(Debug, Clone)]
pub struct IrohInject {
    /// The compositor's stable home EndpointId (the agent's dial target).
    pub home_node: String,
    /// This sandbox's minted, short-lived auth token.
    pub sandbox_auth: String,
    /// Which sandbox the agent serves (the home registry key).
    pub sandbox_id: String,
}

impl FlySpec {
    fn api_base(&self) -> String {
        let b = self.api_base.trim().trim_end_matches('/');
        if b.is_empty() {
            machines::DEFAULT_API_BASE.to_string()
        } else {
            b.to_string()
        }
    }

    fn graphql_url(&self) -> String {
        let g = self.graphql_url.trim();
        if g.is_empty() {
            graphql::DEFAULT_GRAPHQL_URL.to_string()
        } else {
            g.to_string()
        }
    }

    fn org_slug(&self) -> &str {
        let o = self.org_slug.trim();
        if o.is_empty() { "personal" } else { o }
    }

    fn region(&self) -> &str {
        let r = self.region.trim();
        if r.is_empty() {
            machines::DEFAULT_REGION
        } else {
            r
        }
    }

    fn size(&self) -> &str {
        let s = self.size.trim();
        if s.is_empty() {
            machines::DEFAULT_SIZE
        } else {
            s
        }
    }

    fn image(&self) -> String {
        machines::image_ref(&self.image)
            .unwrap_or(machines::DEFAULT_IMAGE)
            .to_string()
    }

    fn max_instances(&self) -> usize {
        if self.max_instances == 0 {
            5
        } else {
            self.max_instances as usize
        }
    }

    fn metadata(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert(machines::MANAGED_KEY.into(), machines::MANAGED_VAL.into());
        m.insert(machines::HOST_KEY.into(), host_label());
        m
    }

    /// Whether the template names a **prebaked** superzej image (`image:<ref>`)
    /// that runs its own sshd + ships the toolchain — the fast path, booting
    /// straight into a reachable shell with no per-VM install. A bare/empty
    /// template is a stock distro that gets [`machines::SSHD_INIT`] instead.
    fn is_prebaked(&self) -> bool {
        self.image.trim().starts_with("image:")
    }
}

/// The async driver: Machines REST lifecycle + ssh reachability for one machine.
pub struct FlyProvider {
    spec: FlySpec,
    client: reqwest::Client,
    ip: Mutex<Option<String>>,
}

impl FlyProvider {
    pub fn new(spec: FlySpec) -> Self {
        FlyProvider {
            spec,
            client: crate::provider::provider_http_client(),
            ip: Mutex::new(None),
        }
    }

    pub fn spec(&self) -> &FlySpec {
        &self.spec
    }

    async fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        let resp = self
            .client
            .get(url)
            .bearer_auth(&self.spec.token)
            .send()
            .await
            .with_context(|| format!("fly: GET {url}"))?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            return Err(anyhow!("fly GET {url} failed ({status}): {body}"));
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
            .with_context(|| format!("fly: POST {url}"))?;
        let status = resp.status();
        let out: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            return Err(anyhow!("fly POST {url} failed ({status}): {out}"));
        }
        Ok(out)
    }

    /// A Fly GraphQL call (IP allocation) — bearer auth, error-surfacing.
    async fn graphql(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        self.post_json(&self.spec.graphql_url(), body).await
    }

    /// Idempotently ensure the app exists (create on 404, tolerate a concurrent
    /// 409/422 uniqueness win).
    async fn ensure_app(&self, app: &str) -> Result<()> {
        let base = self.spec.api_base();
        if self.get_json(&machines::app_url(&base, app)).await.is_ok() {
            return Ok(());
        }
        let body = machines::create_app_body(app, self.spec.org_slug());
        match self.post_json(&machines::apps_url(&base), &body).await {
            Ok(_) => Ok(()),
            Err(e)
                if e.to_string().contains("failed (409")
                    || e.to_string().contains("failed (422") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Ensure the app has a dedicated IPv4 (reuse an existing one, else allocate).
    async fn ensure_ipv4(&self, app: &str) -> Result<String> {
        if let Some(ip) =
            graphql::parse_app_ipv4(&self.graphql(&graphql::app_ips_query(app)).await?)
        {
            return Ok(ip);
        }
        graphql::parse_allocated_ipv4(&self.graphql(&graphql::allocate_ipv4(app)).await?)
    }

    /// All superzej-managed Fly sandboxes this host created (ledger view).
    fn ledger_names(&self) -> Vec<String> {
        registry::list()
            .into_iter()
            .filter(|r| r.provider == LEDGER_PROVIDER)
            .map(|r| r.name)
            .collect()
    }

    /// Resolve the sandbox's public IPv4: cache → ledger → the app's IP via API.
    pub async fn resolve_ip(&self, name: &str) -> Result<String> {
        if let Some(ip) = self.ip.lock().unwrap().clone().filter(|i| !i.is_empty()) {
            return Ok(ip);
        }
        if let Some(rec) = registry::read(name).filter(|r| r.state == "ready" && !r.ip.is_empty()) {
            *self.ip.lock().unwrap() = Some(rec.ip.clone());
            return Ok(rec.ip);
        }
        let app = app_name(name);
        let ip = graphql::parse_app_ipv4(&self.graphql(&graphql::app_ips_query(&app)).await?)
            .ok_or_else(|| anyhow!("fly: sandbox {name} has no dedicated IPv4 yet"))?;
        *self.ip.lock().unwrap() = Some(ip.clone());
        Ok(ip)
    }

    async fn shim(&self, name: &str) -> Result<ssh_shim::SshShim> {
        let ip = self.resolve_ip(name).await?;
        Ok(ssh_shim::SshShim {
            name: name.to_string(),
            ip,
            user: FLY_USER.into(),
            key_path: self.spec.key_path.clone(),
        })
    }

    /// Poll a freshly-created machine to `started`. 2s poll, bounded.
    async fn wait_started(&self, app: &str, id: &str) -> Result<()> {
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(120);
        let base = self.spec.api_base();
        let start = std::time::Instant::now();
        loop {
            let body = self
                .get_json(&machines::machine_url(&base, app, id))
                .await?;
            if machines::parse_machine(&body).is_some_and(|m| m.is_started()) {
                return Ok(());
            }
            if start.elapsed() >= BUDGET {
                return Err(anyhow!(
                    "fly: machine {id} not started after {}s",
                    BUDGET.as_secs()
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    /// Wait until sshd answers over the proxy. The Fly proxy accepts :22 before
    /// the backend is up (and [`machines::SSHD_INIT`] apt-installs sshd, ~40s), so
    /// a bare TCP check is insufficient — probe an actual ssh exec. Bounded.
    async fn wait_reachable(&self, name: &str, ip: &str) -> Result<()> {
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(180);
        let shim = ssh_shim::SshShim {
            name: name.to_string(),
            ip: ip.to_string(),
            user: FLY_USER.into(),
            key_path: self.spec.key_path.clone(),
        };
        let probe = vec!["true".to_string()];
        let start = std::time::Instant::now();
        loop {
            // `run_exec` returns Ok((255, "")) on an ssh CONNECT failure (ssh's
            // own exit code), so a bare `is_ok()` would pass before sshd is up —
            // gate on the exit code actually being 0.
            if matches!(shim.run_exec(&probe, None, &[]).await, Ok((0, _))) {
                return Ok(());
            }
            if start.elapsed() >= BUDGET {
                return Err(anyhow!(
                    "fly: {name} ({ip}) sshd not reachable after {}s",
                    BUDGET.as_secs()
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }

    async fn first_machine(&self, app: &str) -> Result<Option<machines::FlyMachine>> {
        let base = self.spec.api_base();
        let body = self.get_json(&machines::machines_url(&base, app)).await?;
        Ok(machines::parse_machine_list(&body).into_iter().next())
    }

    /// Stop the machine and wait until it is really `stopped` (scale-to-zero
    /// park). SIGTERM so sshd-as-PID1 actually exits.
    pub async fn stop(&self, name: &str) -> Result<()> {
        self.machine_action(name, "stop", machines::stop_body(), "stopped")
            .await
    }

    /// Start a stopped machine and wait until `started`.
    pub async fn start(&self, name: &str) -> Result<()> {
        self.machine_action(name, "start", serde_json::json!({}), "started")
            .await
    }

    async fn machine_action(
        &self,
        name: &str,
        action: &str,
        body: serde_json::Value,
        want: &str,
    ) -> Result<()> {
        let app = app_name(name);
        let m = self
            .first_machine(&app)
            .await?
            .ok_or_else(|| anyhow!("fly: machine for {name} not found"))?;
        let base = self.spec.api_base();
        self.post_json(
            &machines::machine_action_url(&base, &app, &m.id, action),
            &body,
        )
        .await?;
        // Transitions are async (a start 412s while still stopping) and the
        // `/wait` long-poll can return before the machine truly settles — poll
        // the real state.
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(90);
        let start = std::time::Instant::now();
        loop {
            let cur = self
                .get_json(&machines::machine_url(&base, &app, &m.id))
                .await
                .ok()
                .and_then(|v| machines::parse_machine(&v))
                .map(|m| m.state)
                .unwrap_or_default();
            if cur == want {
                return Ok(());
            }
            if start.elapsed() >= BUDGET {
                return Err(anyhow!(
                    "fly: machine {name} not {want} after {}s (state {cur})",
                    BUDGET.as_secs()
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }

    /// One-shot exec over ssh — the pipeline's control-plane primitive.
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

impl RemoteProvider for FlyProvider {
    async fn create(&self) -> Result<SandboxHandle> {
        let name = self.spec.name.trim().to_string();
        if name.is_empty() {
            return Err(anyhow!("fly: the sandbox name is empty"));
        }
        // Spend guardrail (ledger-based, covers in-flight creates).
        let managed = self.ledger_names().len();
        if managed >= self.spec.max_instances() {
            return Err(anyhow!(
                "fly: {managed} managed machines already exist (max_instances = {}); \
                 destroy one or raise `[env.<name>.provider] max_instances`",
                self.spec.max_instances()
            ));
        }
        let app = app_name(&name);
        // Intent BEFORE create — the crash-leak window closes here.
        registry::write(&registry::VpsRecord {
            name: name.clone(),
            provider: LEDGER_PROVIDER.into(),
            state: "creating".into(),
            instance_id: String::new(),
            ip: String::new(),
            created_at: superzej_core::util::now(),
        })?;

        let create = async {
            self.ensure_app(&app).await?;
            let ip = self.ensure_ipv4(&app).await?;
            let base = self.spec.api_base();
            let body = machines::create_machine_body(
                &name,
                self.spec.region(),
                &self.spec.image(),
                self.spec.size(),
                &self.spec.pubkey,
                &self.spec.metadata(),
                self.spec.is_prebaked(),
                self.spec.iroh.as_ref(),
            );
            let created = self
                .post_json(&machines::machines_url(&base, &app), &body)
                .await?;
            let machine = machines::parse_machine(&created)
                .ok_or_else(|| anyhow!("fly: no machine in create response: {created}"))?;
            if !self.spec.skip_ready_wait {
                self.wait_started(&app, &machine.id).await?;
                self.wait_reachable(&name, &ip).await?;
            }
            Ok::<_, anyhow::Error>((machine.id, ip))
        }
        .await;

        let (machine_id, ip) = match create {
            Ok(v) => v,
            Err(e) => {
                // A definite API rejection means nothing was created — clear the
                // intent. Transport errors stay for the (future) reaper.
                if e.to_string().contains("failed (4") {
                    registry::remove(&name);
                }
                return Err(e);
            }
        };
        registry::write(&registry::VpsRecord {
            name: name.clone(),
            provider: LEDGER_PROVIDER.into(),
            state: "ready".into(),
            instance_id: machine_id,
            ip: ip.clone(),
            created_at: superzej_core::util::now(),
        })?;
        *self.ip.lock().unwrap() = Some(ip.clone());
        Ok(SandboxHandle {
            id: name,
            exec: ExecKind::Ssh(SshTarget {
                host: ip,
                port: machines::SSH_PORT,
                forward_agent: false,
            }),
        })
    }

    async fn destroy(&self, id: &str) -> Result<()> {
        // Deleting the app cascades the machine + releases the dedicated IPv4.
        let base = self.spec.api_base();
        let url = machines::app_url(&base, &app_name(id));
        const ATTEMPTS: u32 = 3;
        let mut last_status = None;
        for attempt in 0..ATTEMPTS {
            let resp = self
                .client
                .delete(&url)
                .bearer_auth(&self.spec.token)
                .send()
                .await
                .context("fly: DELETE app")?;
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
            "fly destroy {id} failed ({})",
            last_status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "no response".into())
        ))
    }

    async fn list(&self) -> Result<Vec<String>> {
        Ok(self.ledger_names())
    }
}

impl ProviderFiles for FlyProvider {
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

    fn spec() -> FlySpec {
        FlySpec {
            api_base: String::new(),
            graphql_url: String::new(),
            token: "t".into(),
            org_slug: String::new(),
            name: "sz-fly-1".into(),
            region: String::new(),
            size: String::new(),
            image: String::new(),
            max_instances: 0,
            max_lifetime_secs: 0,
            key_path: "/k".into(),
            pubkey: "ssh-ed25519 A".into(),
            iroh: None,
            skip_ready_wait: true,
        }
    }

    #[test]
    fn spec_defaults() {
        let s = spec();
        assert_eq!(s.api_base(), machines::DEFAULT_API_BASE);
        assert_eq!(s.graphql_url(), graphql::DEFAULT_GRAPHQL_URL);
        assert_eq!(s.org_slug(), "personal");
        assert_eq!(s.region(), "iad");
        assert_eq!(s.size(), "shared-cpu-2x");
        assert_eq!(s.image(), "ubuntu:24.04");
        assert_eq!(s.max_instances(), 5);
        let m = s.metadata();
        assert_eq!(m.get("managed-by").map(String::as_str), Some("superzej"));
        assert!(m.contains_key("sz-host"));
    }

    #[test]
    fn app_name_is_per_sandbox_stable_and_valid() {
        let a = app_name("sz-fly-1");
        assert_eq!(a, app_name("sz-fly-1"), "stable for a name");
        assert_ne!(a, app_name("sz-fly-2"), "distinct per sandbox");
        assert!(a.starts_with("sz-"));
        assert!(
            a.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "valid Fly app name: {a}"
        );
    }

    #[test]
    fn image_and_size_overrides() {
        let s = FlySpec {
            image: "image:registry.fly.io/x:deployment-2".into(),
            size: "performance-1x".into(),
            region: "ams".into(),
            org_slug: "acme".into(),
            ..spec()
        };
        assert_eq!(s.image(), "registry.fly.io/x:deployment-2");
        assert_eq!(s.size(), "performance-1x");
        assert_eq!(s.region(), "ams");
        assert_eq!(s.org_slug(), "acme");
    }
}
