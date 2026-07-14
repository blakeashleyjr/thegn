//! **machine0** provider — a cloud-VM sandbox driven through machine0's remote
//! **MCP** endpoint (no `machine0` CLI binary), with an ssh **data plane**.
//!
//! Split by plane:
//! - **Control plane → MCP** (`mcp::Mcp0Client`): lifecycle (`vm_create` /
//!   `vm_get_by_name` / `vm_list` / `vm_destroy` / `vm_start` / `vm_suspend`),
//!   snapshots (`image_create` / `image_list`), and ssh-key import
//!   (`ssh_key_create` / `ssh_key_list`).
//! - **Data plane → ssh** (`crate::vps::ssh_shim::SshShim`): the interactive
//!   pane (`ExecKind::Ssh`), one-shot `run_exec`, file sync, and the NixOS
//!   `nixos-rebuild switch --flake` apply. machine0's MCP `ssh_exec` tool is
//!   one-shot with no PTY, so an interactive terminal must ride real ssh — the
//!   same shape as [`crate::vps::VpsProvider`]. We import *thegn's* managed
//!   public key so we hold the private half (managed-key private material is
//!   never returned over MCP).
//!
//! Request/response shaping is split into **pure** functions (`vm_create_args`,
//! `parse_vm`, `parse_vm_list`, `parse_image_list`, …) so the wire mapping is
//! unit-tested without a live endpoint.
//!
//! NOTE: the machines/ssh-exec tool names (`vm_*`, `ssh_exec`) are confirmed
//! from machine0's MCP docs; the keys/images tool names (`ssh_key_create`,
//! `ssh_key_list`, `image_create`, `image_list`) are the documented category
//! verbs and should be validated against a live `tools/list` — they are isolated
//! in the `tool` consts below for a one-line fix if a name differs.

pub mod mcp;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

use crate::provider::{
    CheckpointInfo, ExecKind, FileEntry, ProviderCheckpoints, ProviderFiles, RemoteProvider,
    SandboxHandle,
};
use crate::vps::ssh_shim::SshShim;
use mcp::Mcp0Client;
use thegn_core::remote::SshTarget;

/// MCP tool names (verified against a live machine0 `tools/list`), isolated so a
/// future rename is a one-liner.
mod tool {
    pub const VM_CREATE: &str = "vm_create";
    pub const VM_GET_BY_NAME: &str = "vm_get_by_name";
    pub const VM_LIST: &str = "vm_list";
    pub const VM_DESTROY: &str = "vm_destroy";
    pub const VM_START: &str = "vm_start";
    pub const VM_STOP: &str = "vm_stop";
    pub const VM_SUSPEND: &str = "vm_suspend";
    /// Import an existing public key (we hold the private half). NOT
    /// `ssh_key_create_managed` (whose private key stays server-side).
    pub const SSH_KEY_CREATE_PUBLIC: &str = "ssh_key_create_public";
    pub const SSH_KEY_LIST: &str = "ssh_key_list";
    pub const IMAGE_CREATE: &str = "image_create";
    pub const IMAGE_LIST: &str = "image_list";
    pub const SIZE_LIST: &str = "size_list";
}

/// The remote user provisioning + attach run as (stock machine0 images — incl.
/// the NixOS image — boot with root + the injected key). Overridable via config;
/// the per-VM `defaultSSHUsername` from `vm_get` takes precedence.
pub const DEFAULT_USER: &str = "root";

/// The default machine0 image when `[env.<name>.provider] template` is unset:
/// NixOS 25.11 with a modern shell + dev tools baked in — thegn's preferred
/// substrate (reproducible, flake-provisionable).
pub const DEFAULT_IMAGE: &str = "nixos-25-11-loaded";

/// The default machine0 region when unset (machine0 requires a region and has no
/// server-side default).
pub const DEFAULT_REGION: &str = "us-east";

/// Everything needed to drive one named machine0 VM (resolved host-side from
/// `[env.<name>.provider]` + thegn's managed keypair).
#[derive(Debug, Clone)]
pub struct Machine0Spec {
    /// MCP endpoint (empty ⇒ [`mcp::DEFAULT_ENDPOINT`]).
    pub endpoint: String,
    /// The resolved `x-api-key` (never logged / never on an argv).
    pub api_key: String,
    /// The VM name to manage (the resolved sandbox id).
    pub name: String,
    /// `imageName` at create, e.g. `"nixos-25-11-loaded"` (empty ⇒ machine0's
    /// account default, which machine0 may reject — configure `template`).
    pub image: String,
    /// Explicit vendor size name (e.g. `"large"`). Empty or `"auto"` ⇒ resolve
    /// dynamically from [`size_req`](Self::size_req) against `size_list`.
    pub size: String,
    /// Multidimensional size requirements used when `size` is unset/`auto`:
    /// pick the cheapest machine0 size meeting all of them.
    pub size_req: SizeReq,
    /// Vendor region (empty ⇒ machine0 default).
    pub region: String,
    /// NixOS only: a flake ref applied post-create via `nixos-rebuild switch
    /// --flake <ref>` over ssh (empty ⇒ skip). A local `path#attr` is uploaded
    /// first; a flake URL (`github:…#host`) is used verbatim.
    pub provision_flake: String,
    /// SSH username (empty ⇒ [`DEFAULT_USER`]).
    pub ssh_user: String,
    /// Managed private key path + its OpenSSH public line (imported at create).
    pub key_path: PathBuf,
    pub pubkey: String,
    /// Hard cap on concurrently-managed VMs (0 ⇒ default 5) — the spend
    /// guardrail enforced at create.
    pub max_instances: u32,
    /// Ceiling on a VM's lifetime in seconds (0 ⇒ off). Reserved for the reaper.
    pub max_lifetime_secs: u64,
    /// Test hook: skip the readiness/reachability wait after create (mock
    /// endpoints can't answer ssh). Never set outside tests.
    #[doc(hidden)]
    pub skip_ready_wait: bool,
}

impl Machine0Spec {
    fn max_instances(&self) -> usize {
        if self.max_instances == 0 {
            5
        } else {
            self.max_instances as usize
        }
    }
}

/// The fallback size when `size` is unset/`auto` and no requirements are given.
pub const DEFAULT_SIZE: &str = "large";

/// Multidimensional size requirements — the cheapest machine0 size meeting ALL
/// of these is chosen when no explicit `size` is configured. `0` / `false` =
/// unconstrained on that axis.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SizeReq {
    /// Minimum vCPUs.
    pub min_vcpu: u32,
    /// Minimum RAM (GiB).
    pub min_ram_gb: u32,
    /// Minimum disk (GiB).
    pub min_disk_gb: u32,
    /// Require a GPU size.
    pub gpu: bool,
    /// Require NVMe-backed local disk.
    pub nvme: bool,
}

impl SizeReq {
    pub fn is_empty(&self) -> bool {
        self.min_vcpu == 0
            && self.min_ram_gb == 0
            && self.min_disk_gb == 0
            && !self.gpu
            && !self.nvme
    }
}

/// One machine0 size as returned by `size_list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine0Size {
    pub name: String,
    pub vcpu: u32,
    pub ram_gb: u32,
    pub disk_gb: u32,
    pub gpu: bool,
    pub nvme: bool,
    /// Price per hour in micro-dollars (the ranking key — cheapest wins).
    pub price_micro: u64,
}

/// A machine0 VM as seen by the control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine0Vm {
    pub id: String,
    pub name: String,
    pub status: String,
    pub address: Option<String>,
    pub user: Option<String>,
}

/// The async driver: MCP lifecycle + ssh exec/files for one named VM.
pub struct Machine0Provider {
    mcp: Mcp0Client,
    spec: Machine0Spec,
    /// Resolved `(public address, ssh user)`, cached per provider instance.
    endpoint: Mutex<Option<(String, String)>>,
}

impl Machine0Provider {
    pub fn new(spec: Machine0Spec) -> Self {
        let mcp = Mcp0Client::new(&spec.endpoint, &spec.api_key);
        Machine0Provider {
            mcp,
            spec,
            endpoint: Mutex::new(None),
        }
    }

    pub fn spec(&self) -> &Machine0Spec {
        &self.spec
    }

    // --- control plane (MCP) ------------------------------------------------

    /// Call an MCP tool, surfacing machine0's **business errors** — which arrive
    /// as an `{ "error": …, "message": … }` object *inside* a 200 tool result
    /// (not a JSON-RPC error, not `isError`) — as a hard failure. Without this a
    /// failed `vm_start`/`vm_stop` would look like success.
    async fn call(&self, name: &str, args: Value) -> Result<Value> {
        let v = self.mcp.call_tool(name, args).await?;
        if let Some(err) = v.get("error").and_then(Value::as_str) {
            let msg = v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(err);
            return Err(anyhow!("machine0 {name}: {err}: {msg}"));
        }
        Ok(v)
    }

    /// Look up a VM by name; `Ok(None)` when it does not exist (idempotent
    /// destroy / existence checks). A genuine transport/API failure propagates.
    async fn vm_by_name(&self, name: &str) -> Result<Option<Machine0Vm>> {
        match self.call(tool::VM_GET_BY_NAME, json!({ "name": name })).await {
            Ok(v) => Ok(parse_vm(&v)),
            Err(e) if is_not_found(&e.to_string()) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Import thegn's managed public key (idempotent — reuse a same-material key
    /// already registered), returning its key id for `vm_create.sshKeyId`. Uses
    /// `ssh_key_create_public` (import — we hold the private half) so our own ssh
    /// works; `ssh_key_create_managed` would keep the private key server-side.
    async fn ensure_ssh_key(&self) -> Result<String> {
        let listed = self.call(tool::SSH_KEY_LIST, json!({})).await?;
        if let Some((id, _)) = parse_ssh_key_list(&listed)
            .into_iter()
            .find(|(_, pk)| same_pubkey(pk, &self.spec.pubkey))
        {
            return Ok(id);
        }
        // Name/fileName by material fingerprint so distinct keys never collide on
        // a fixed name (machine0 requires a `fileName` too).
        let fp = thegn_core::util::short_hash(self.spec.pubkey.trim(), 8);
        let created = self
            .call(
                tool::SSH_KEY_CREATE_PUBLIC,
                json!({
                    "name": format!("thegn-managed-{fp}"),
                    "fileName": format!("thegn-managed-{fp}.pub"),
                    "publicKey": self.spec.pubkey.trim(),
                    "isDefault": false,
                }),
            )
            .await?;
        parse_created_id(&created)
            .ok_or_else(|| anyhow!("machine0: no ssh key id in create response: {created}"))
    }

    /// Resolve the vendor size to create at. An explicit `size` (not `auto`)
    /// wins verbatim. Otherwise query `size_list` and pick the **cheapest** size
    /// meeting every `size_req` dimension (cpu/ram/disk/gpu/nvme); with no
    /// requirements, fall back to [`DEFAULT_SIZE`].
    async fn resolve_size(&self) -> Result<String> {
        let explicit = self.spec.size.trim();
        if !explicit.is_empty() && !explicit.eq_ignore_ascii_case("auto") {
            return Ok(explicit.to_string());
        }
        if self.spec.size_req.is_empty() {
            return Ok(DEFAULT_SIZE.to_string());
        }
        let v = self.call(tool::SIZE_LIST, json!({})).await?;
        let sizes = parse_size_list(&v);
        pick_size(&sizes, &self.spec.size_req).ok_or_else(|| {
            anyhow!(
                "machine0: no size satisfies the requirements ({:?}); see `machine0 sizes`",
                self.spec.size_req
            )
        })
    }

    /// Create the VM named `name` from `image`, waiting until it is RUNNING +
    /// reachable. Returns the live VM (with its address + ssh user). Empty
    /// `image` ⇒ [`DEFAULT_IMAGE`]; the size is resolved (dynamic, per
    /// [`Self::resolve_size`]).
    async fn spawn(&self, name: &str, image: &str) -> Result<Machine0Vm> {
        let key_id = self.ensure_ssh_key().await?;
        let image = {
            let i = image.trim();
            if i.is_empty() { DEFAULT_IMAGE } else { i }
        };
        let size = self.resolve_size().await?;
        let region = {
            let r = self.spec.region.trim();
            if r.is_empty() { DEFAULT_REGION } else { r }
        };
        let args = vm_create_args(name, image, &size, region, &key_id);
        self.call(tool::VM_CREATE, args).await?;

        if self.spec.skip_ready_wait {
            return Ok(self.vm_by_name(name).await?.unwrap_or(Machine0Vm {
                id: String::new(),
                name: name.to_string(),
                status: "RUNNING".into(),
                address: None,
                user: None,
            }));
        }
        let vm = self.ensure_running_vm(name).await?;
        let ip = vm
            .address
            .clone()
            .ok_or_else(|| anyhow!("machine0: vm {name} has no address after RUNNING"))?;
        let user = self.ssh_user_for(&vm);
        self.wait_reachable(name, &ip, &user).await?;
        *self.endpoint.lock().unwrap() = Some((ip, user));
        Ok(vm)
    }

    /// The ssh user for a VM: the configured `ssh_user`, else the VM's own
    /// `defaultSSHUsername` (e.g. `ubuntu` for ubuntu images, `root` for NixOS),
    /// else `root`.
    fn ssh_user_for(&self, vm: &Machine0Vm) -> String {
        let u = self.spec.ssh_user.trim();
        if !u.is_empty() {
            return u.to_string();
        }
        vm.user
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .unwrap_or(DEFAULT_USER)
            .to_string()
    }

    /// Poll `vm_get_by_name` until the VM is RUNNING with an address, **starting
    /// it** when it is in a startable state (STOPPED/SUSPENDED/ERRORED) and simply
    /// waiting through transitional states (CREATING/SUSPENDING/STOPPING/…).
    /// machine0 says provisioning takes 1–3 min; bounded. Unifies the
    /// create-wait and the resume-on-claim wait.
    async fn ensure_running_vm(&self, name: &str) -> Result<Machine0Vm> {
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(360);
        let start = std::time::Instant::now();
        let mut started = false;
        let mut last: String;
        loop {
            // A transient control-plane blip (or brief post-create not-found from
            // eventual consistency) must not abort a multi-minute wait — poll
            // through it until the budget, surfacing the last state on timeout.
            match self.vm_by_name(name).await {
                Ok(Some(vm)) => {
                    if status_running(&vm.status) && vm.address.is_some() {
                        return Ok(vm);
                    }
                    last = vm.status.clone();
                    if !started && !vm.id.is_empty() && status_startable(&vm.status) {
                        self.call(tool::VM_START, json!({ "id": vm.id })).await?;
                        started = true;
                    }
                }
                Ok(None) => last = "not-found".into(),
                Err(e) => last = format!("transient: {e}"),
            }
            if start.elapsed() >= BUDGET {
                return Err(anyhow!(
                    "machine0: vm {name} not RUNNING after {}s (last {last})",
                    BUDGET.as_secs(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        }
    }

    /// Poll `vm_get_by_name` until `pred(status)` holds (e.g. STOPPED before an
    /// image). Bounded; `label` names the target state for the timeout error.
    async fn wait_status(
        &self,
        name: &str,
        pred: fn(&str) -> bool,
        label: &str,
    ) -> Result<()> {
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(180);
        let start = std::time::Instant::now();
        let mut last: String;
        loop {
            match self.vm_by_name(name).await {
                Ok(Some(vm)) => {
                    if pred(&vm.status) {
                        return Ok(());
                    }
                    last = vm.status.clone();
                }
                Ok(None) => last = "not-found".into(),
                Err(e) => last = format!("transient: {e}"),
            }
            if start.elapsed() >= BUDGET {
                return Err(anyhow!(
                    "machine0: vm {name} not {label} after {}s (last {last})",
                    BUDGET.as_secs(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        }
    }

    /// Wait until a freshly-created/resumed VM actually accepts an ssh **exec** as
    /// `user` — not merely a TCP :22 connect. A NixOS (cloud-init) image opens the
    /// listening socket before it has injected the authorized key, so a TCP-only
    /// gate races key provisioning and the first `run_exec` fails with ssh's 255.
    /// Two bounded phases: TCP reachability, then an auth+shell probe (`true`).
    async fn wait_reachable(&self, name: &str, ip: &str, user: &str) -> Result<()> {
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
                        "machine0: {name} ({ip}) sshd not reachable after {}s",
                        TCP_BUDGET.as_secs()
                    ));
                }
                _ => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
            }
        }
        // sshd is listening — now wait for key-auth + a working shell.
        const AUTH_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);
        let shim = SshShim {
            name: name.to_string(),
            ip: ip.to_string(),
            user: user.to_string(),
            key_path: self.spec.key_path.clone(),
        };
        let start = std::time::Instant::now();
        loop {
            if let Ok((0, _)) = shim
                .run_exec(&["/bin/sh".into(), "-lc".into(), "true".into()], None, &[])
                .await
            {
                return Ok(());
            }
            if start.elapsed() >= AUTH_BUDGET {
                return Err(anyhow!(
                    "machine0: {name} ({ip}) ssh auth as {user} not ready after {}s",
                    AUTH_BUDGET.as_secs()
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }

    // --- data plane (ssh) ---------------------------------------------------

    /// Resolve a live, awake VM's `(address, ssh user)` — starting it first if it
    /// was suspended/stopped (the warm-pool resume-on-claim path). Cached after
    /// first hit.
    async fn awake_endpoint(&self, name: &str) -> Result<(String, String)> {
        if let Some(ep) = self.endpoint.lock().unwrap().clone() {
            return Ok(ep);
        }
        let vm = self.ensure_running_vm(name).await?;
        let ip = vm
            .address
            .clone()
            .ok_or_else(|| anyhow!("machine0: vm {name} has no address"))?;
        let ep = (ip, self.ssh_user_for(&vm));
        *self.endpoint.lock().unwrap() = Some(ep.clone());
        Ok(ep)
    }

    async fn shim(&self, name: &str) -> Result<SshShim> {
        let (ip, user) = self.awake_endpoint(name).await?;
        Ok(SshShim {
            name: name.to_string(),
            ip,
            user,
            key_path: self.spec.key_path.clone(),
        })
    }

    /// Resolve the VM's `(address, ssh user)` for an **interactive** attach,
    /// starting a suspended/stopped VM first (resume-on-open). The public entry
    /// the `machine0-ssh` pane bridge uses.
    pub async fn resolve_endpoint(&self, name: &str) -> Result<(String, String)> {
        self.awake_endpoint(name).await
    }

    /// Resolve the VM's `(address, ssh user)` **without waking it** — the
    /// control-plane read path (chrome git/fs polls). `Err` when the VM is not
    /// RUNNING/reachable (suspended, stopped, transitional, or gone), so callers
    /// serve cached state instead of resuming a parked VM.
    pub async fn peek_endpoint(&self, name: &str) -> Result<(String, String)> {
        let vm = self
            .vm_by_name(name)
            .await?
            .ok_or_else(|| anyhow!("machine0: vm {name} not found"))?;
        if !status_running(&vm.status) {
            return Err(anyhow!(
                "machine0: vm {name} is not running (status {})",
                vm.status
            ));
        }
        let ip = vm
            .address
            .clone()
            .ok_or_else(|| anyhow!("machine0: vm {name} has no address"))?;
        Ok((ip, self.ssh_user_for(&vm)))
    }

    /// NixOS flake apply over ssh (there is no `provision` MCP tool). A local
    /// `path#attr` ref is uploaded first and rebuilt from the uploaded dir; a
    /// flake URL is applied verbatim. Bounded — a rebuild takes minutes.
    async fn provision_nixos(&self, name: &str) -> Result<()> {
        let flake = self.spec.provision_flake.trim();
        if flake.is_empty() {
            return Ok(());
        }
        let (loc, attr) = match flake.split_once('#') {
            Some((l, a)) => (l, Some(a)),
            None => (flake, None),
        };
        let remote_ref = if Path::new(loc).exists() {
            // Upload the flake dir (parent of a flake.nix file, or the dir itself)
            // and reference it on the VM.
            let p = Path::new(loc);
            let dir = if p.is_file() {
                p.parent().unwrap_or(Path::new("."))
            } else {
                p
            };
            const REMOTE_DIR: &str = "/root/thegn-provision";
            self.upload_dir(name, dir, REMOTE_DIR).await?;
            match attr {
                Some(a) => format!("{REMOTE_DIR}#{a}"),
                None => REMOTE_DIR.to_string(),
            }
        } else {
            flake.to_string()
        };
        let script = format!(
            "nixos-rebuild switch --flake {}",
            thegn_core::util::sh_quote(&remote_ref)
        );
        let argv = vec!["/bin/sh".to_string(), "-lc".to_string(), script];
        let shim = self.shim(name).await?;
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(1800);
        let (code, out) = tokio::time::timeout(BUDGET, shim.run_exec(&argv, None, &[]))
            .await
            .map_err(|_| anyhow!("machine0: nixos-rebuild timed out after {}s", BUDGET.as_secs()))?
            .context("machine0: nixos-rebuild over ssh")?;
        if code != 0 {
            return Err(anyhow!(
                "machine0: nixos-rebuild --flake {remote_ref} failed (exit {code}):\n{out}"
            ));
        }
        Ok(())
    }

    /// One-shot exec `(exit_code, combined output)` over ssh — the pipeline's
    /// control-plane primitive (mirrors [`crate::vps::VpsProvider::run_exec`]).
    pub async fn run_exec(
        &self,
        id: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<(i32, String)> {
        self.shim(id).await?.run_exec(argv, cwd, env).await
    }

    /// Suspend the VM (scale-to-zero park). Concrete — called by the warm-pool
    /// park path, not enum-dispatched.
    pub async fn suspend(&self, name: &str) -> Result<()> {
        let Some(vm) = self.vm_by_name(name).await? else {
            return Ok(());
        };
        if vm.id.is_empty() || !status_running(&vm.status) {
            return Ok(()); // already parked / transitioning
        }
        *self.endpoint.lock().unwrap() = None;
        self.call(tool::VM_SUSPEND, json!({ "id": vm.id }))
            .await
            .map(|_| ())
    }

    /// Resume a suspended VM and wait until it is reachable again (claim path).
    pub async fn resume(&self, name: &str) -> Result<()> {
        *self.endpoint.lock().unwrap() = None;
        let (ip, user) = self.awake_endpoint(name).await?;
        self.wait_reachable(name, &ip, &user).await
    }
}

impl RemoteProvider for Machine0Provider {
    async fn create(&self) -> Result<SandboxHandle> {
        let name = self.spec.name.trim().to_string();
        if name.is_empty() {
            return Err(anyhow!("machine0: the sandbox name is empty"));
        }
        // Spend guardrail: never mint past the cap.
        let managed = self.list().await.map(|v| v.len()).unwrap_or(0);
        if managed >= self.spec.max_instances() {
            return Err(anyhow!(
                "machine0: {managed} VMs already exist (max_instances = {}); destroy one or raise \
                 `[env.<name>.provider] max_instances`",
                self.spec.max_instances()
            ));
        }
        let vm = self.spawn(&name, &self.spec.image).await?;
        // NixOS flake apply (no-op unless `provision_flake` is set).
        self.provision_nixos(&name).await?;
        let host = vm.address.unwrap_or_default();
        Ok(SandboxHandle {
            id: name,
            exec: ExecKind::Ssh(SshTarget {
                host,
                port: 22,
                forward_agent: false,
            }),
        })
    }

    async fn destroy(&self, id: &str) -> Result<()> {
        let Some(vm) = self.vm_by_name(id).await? else {
            return Ok(()); // already gone
        };
        if vm.id.is_empty() {
            return Ok(());
        }
        *self.endpoint.lock().unwrap() = None;
        match self.call(tool::VM_DESTROY, json!({ "id": vm.id })).await {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e.to_string()) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn list(&self) -> Result<Vec<String>> {
        let v = self.call(tool::VM_LIST, json!({})).await?;
        Ok(parse_vm_list(&v))
    }
}

impl ProviderFiles for Machine0Provider {
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

impl ProviderCheckpoints for Machine0Provider {
    async fn checkpoint(&self, id: &str, label: Option<&str>) -> Result<String> {
        let vm = self
            .vm_by_name(id)
            .await?
            .ok_or_else(|| anyhow!("machine0: vm {id} not found"))?;
        // machine0 images a **stopped** machine: stop, wait for STOPPED, then
        // `image_create` by instance name. The VM is left stopped — the next exec
        // resumes it via `ensure_running_vm` (STOPPED is startable).
        let image_name = label
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{id}-{}", thegn_core::util::now()));
        if status_running(&vm.status) && !vm.id.is_empty() {
            *self.endpoint.lock().unwrap() = None;
            self.call(tool::VM_STOP, json!({ "id": vm.id })).await?;
            self.wait_status(id, status_stopped, "STOPPED").await?;
        }
        let created = self
            .call(
                tool::IMAGE_CREATE,
                json!({ "instanceName": id, "imageName": image_name }),
            )
            .await?;
        Ok(parse_created_id(&created).unwrap_or(image_name))
    }

    async fn list_checkpoints(&self, _id: &str) -> Result<Vec<CheckpointInfo>> {
        let v = self.call(tool::IMAGE_LIST, json!({})).await?;
        Ok(parse_image_list(&v))
    }

    async fn restore(&self, id: &str, checkpoint: &str) -> Result<()> {
        // machine0 images have no in-place restore: destroy + recreate from the
        // saved image (recreate semantics — the VM's underlying id changes; the
        // sandbox name/id is stable).
        self.destroy(id).await?;
        *self.endpoint.lock().unwrap() = None;
        self.spawn(id, checkpoint).await?;
        Ok(())
    }
}

// --- pure request/response shaping (unit-tested) ---------------------------

/// The `vm_create` arguments: name always; image/size/region/sshKeyId only when
/// set (empty ⇒ machine0's default). Pure.
pub fn vm_create_args(
    name: &str,
    image: &str,
    size: &str,
    region: &str,
    ssh_key_id: &str,
) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("name".into(), name.into());
    for (k, v) in [
        ("imageName", image),
        ("size", size),
        ("region", region),
        ("sshKeyId", ssh_key_id),
    ] {
        if !v.trim().is_empty() {
            m.insert(k.into(), v.trim().into());
        }
    }
    Value::Object(m)
}

/// Whether a VM status string names the RUNNING state. Pure.
pub fn status_running(status: &str) -> bool {
    status.trim().eq_ignore_ascii_case("running")
}

/// Whether a VM status string names a paused/off state that needs a `vm_start`
/// before exec. Pure.
pub fn status_suspended(status: &str) -> bool {
    let s = status.trim();
    ["suspended", "stopped", "off", "paused", "suspend"]
        .iter()
        .any(|k| s.eq_ignore_ascii_case(k))
}

/// Whether a VM is in a **settled, startable** state (`vm_start` accepts
/// STOPPED / SUSPENDED / ERRORED; transitional CREATING/SUSPENDING/STOPPING are
/// NOT startable — wait through them). Pure.
pub fn status_startable(status: &str) -> bool {
    let s = status.trim();
    ["stopped", "suspended", "errored"]
        .iter()
        .any(|k| s.eq_ignore_ascii_case(k))
}

/// Whether a VM is fully stopped (the pre-image state). Pure.
pub fn status_stopped(status: &str) -> bool {
    status.trim().eq_ignore_ascii_case("stopped")
}

/// Whether an error string names a "does not exist" condition (idempotent
/// destroy / existence checks). Pure.
fn is_not_found(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("not found")
        || m.contains("http 404")
        || m.contains("does not exist")
        || m.contains("no such")
        || m.contains("no machine")
        || m.contains("unknown machine")
}

/// Pull a usable address out of a VM object, tolerating the many field names a
/// provider might use (and one level of nesting). Pure.
fn extract_address(v: &Value) -> Option<String> {
    const KEYS: &[&str] = &[
        "publicIp",
        "public_ip",
        "publicIpv4",
        "ipv4",
        "ip",
        "ipAddress",
        "address",
        "host",
        "hostname",
        "fqdn",
    ];
    for k in KEYS {
        if let Some(s) = v.get(k).and_then(Value::as_str) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    for container in ["network", "networking", "net"] {
        if let Some(inner) = v.get(container)
            && let Some(a) = extract_address(inner)
        {
            return Some(a);
        }
    }
    for arr_key in ["networks", "addresses", "interfaces"] {
        if let Some(arr) = v.get(arr_key).and_then(Value::as_array) {
            for item in arr {
                if let Some(a) = extract_address(item) {
                    return Some(a);
                }
            }
        }
    }
    None
}

/// Parse a single VM object (tolerating a `{machine|vm: {...}}` envelope). Pure.
pub fn parse_vm(v: &Value) -> Option<Machine0Vm> {
    let obj = v.get("machine").or_else(|| v.get("vm")).unwrap_or(v);
    let id = obj
        .get("id")
        .or_else(|| obj.get("uuid"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = obj.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let status = obj
        .get("status")
        .or_else(|| obj.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let address = extract_address(obj);
    let user = obj
        .get("defaultSSHUsername")
        .or_else(|| obj.get("sshUser"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if id.is_empty() && name.is_empty() && address.is_none() {
        return None;
    }
    Some(Machine0Vm {
        id,
        name,
        status,
        address,
        user,
    })
}

/// Parse a `vm_list` response (array, or `{machines|vms|items: [...]}`) into
/// names (falling back to ids). Pure.
pub fn parse_vm_list(v: &Value) -> Vec<String> {
    array_field(v, &["machines", "vms", "items"])
        .iter()
        .filter_map(|e| {
            let obj = e.get("machine").or_else(|| e.get("vm")).unwrap_or(e);
            obj.get("name")
                .or_else(|| obj.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// Parse an `image_list` response into checkpoints. The `id` is the image
/// **name** (what `vm_create.imageName` / restore consumes). Pure.
pub fn parse_image_list(v: &Value) -> Vec<CheckpointInfo> {
    array_field(v, &["images", "items"])
        .iter()
        .filter_map(|e| {
            let obj = e.get("image").unwrap_or(e);
            let id = obj
                .get("name")
                .or_else(|| obj.get("id"))
                .and_then(Value::as_str)?
                .to_string();
            let label = obj
                .get("description")
                .or_else(|| obj.get("label"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Some(CheckpointInfo { id, label })
        })
        .collect()
}

/// Parse a `size_list` response into sizes. A size is NVMe-backed when its name
/// carries the `-nvme` suffix; GPU when the `gpu` field is non-null. Pure.
pub fn parse_size_list(v: &Value) -> Vec<Machine0Size> {
    array_field(v, &["sizes", "items"])
        .iter()
        .filter_map(|e| {
            let name = e.get("size").or_else(|| e.get("name")).and_then(Value::as_str)?;
            Some(Machine0Size {
                name: name.to_string(),
                vcpu: e.get("vcpu").and_then(Value::as_u64).unwrap_or(0) as u32,
                ram_gb: e.get("ramGb").and_then(Value::as_u64).unwrap_or(0) as u32,
                disk_gb: e.get("diskGb").and_then(Value::as_u64).unwrap_or(0) as u32,
                gpu: e.get("gpu").map(|g| !g.is_null()).unwrap_or(false),
                nvme: name.to_ascii_lowercase().contains("nvme"),
                price_micro: e
                    .get("pricePerHourMicro")
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::MAX),
            })
        })
        .collect()
}

/// Pick the **cheapest** size meeting every requirement in `req`. Ties break on
/// the smaller footprint (vcpu then ram) for determinism. `None` if nothing
/// fits. Pure (unit-tested).
pub fn pick_size(sizes: &[Machine0Size], req: &SizeReq) -> Option<String> {
    sizes
        .iter()
        .filter(|s| {
            s.vcpu >= req.min_vcpu
                && s.ram_gb >= req.min_ram_gb
                && s.disk_gb >= req.min_disk_gb
                && (!req.gpu || s.gpu)
                && (!req.nvme || s.nvme)
                // Don't hand a GPU box to a request that didn't ask for one
                // (GPU sizes are pricey + often gpuOnly-imaged).
                && (req.gpu || !s.gpu)
        })
        .min_by(|a, b| {
            a.price_micro
                .cmp(&b.price_micro)
                .then(a.vcpu.cmp(&b.vcpu))
                .then(a.ram_gb.cmp(&b.ram_gb))
        })
        .map(|s| s.name.clone())
}

/// Parse an ssh-key list into `(id, publicKey)` pairs. Pure.
pub fn parse_ssh_key_list(v: &Value) -> Vec<(String, String)> {
    array_field(v, &["sshKeys", "keys", "items"])
        .iter()
        .filter_map(|e| {
            let obj = e.get("sshKey").or_else(|| e.get("key")).unwrap_or(e);
            let id = obj.get("id").and_then(Value::as_str)?.to_string();
            let pk = obj
                .get("publicKey")
                .or_else(|| obj.get("public_key"))
                .or_else(|| obj.get("key"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some((id, pk))
        })
        .collect()
}

/// Extract the created object's id (tolerating `{sshKey|key|image: {...}}`
/// envelopes and `id`/`sshKeyId`/`keyId`/`name` fields). Pure.
pub fn parse_created_id(v: &Value) -> Option<String> {
    let obj = v
        .get("sshKey")
        .or_else(|| v.get("key"))
        .or_else(|| v.get("image"))
        .or_else(|| v.get("machine"))
        .unwrap_or(v);
    obj.get("id")
        .or_else(|| obj.get("sshKeyId"))
        .or_else(|| obj.get("keyId"))
        .or_else(|| obj.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The first array under any of `keys` at the top level, or `v` itself if it is
/// an array. Pure helper for the `parse_*_list` fns.
fn array_field(v: &Value, keys: &[&str]) -> Vec<Value> {
    if let Some(a) = v.as_array() {
        return a.clone();
    }
    for k in keys {
        if let Some(a) = v.get(k).and_then(Value::as_array) {
            return a.clone();
        }
    }
    Vec::new()
}

/// Whether two OpenSSH public keys are the same material (compare `type base64`,
/// ignoring the trailing comment). Pure.
fn same_pubkey(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
    }
    let (a, b) = (a.trim(), b.trim());
    !a.is_empty() && !b.is_empty() && norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_create_args_omits_empty_flags() {
        let full = vm_create_args("dev", "nixos-25-11-loaded", "large", "us-east", "key-1");
        assert_eq!(full["name"], "dev");
        assert_eq!(full["imageName"], "nixos-25-11-loaded");
        assert_eq!(full["size"], "large");
        assert_eq!(full["region"], "us-east");
        assert_eq!(full["sshKeyId"], "key-1");
        // Empties are dropped so machine0 applies its defaults.
        let bare = vm_create_args("dev", "", "", "  ", "");
        assert_eq!(bare["name"], "dev");
        assert!(bare.get("imageName").is_none());
        assert!(bare.get("size").is_none());
        assert!(bare.get("region").is_none());
        assert!(bare.get("sshKeyId").is_none());
    }

    #[test]
    fn status_helpers() {
        assert!(status_running("RUNNING"));
        assert!(status_running("running"));
        assert!(!status_running("SUSPENDED"));
        assert!(status_suspended("SUSPENDED"));
        assert!(status_suspended("stopped"));
        assert!(!status_suspended("RUNNING"));
        // startable = settled off-states; transitional states are NOT startable.
        assert!(status_startable("STOPPED"));
        assert!(status_startable("SUSPENDED"));
        assert!(status_startable("ERRORED"));
        assert!(!status_startable("SUSPENDING"));
        assert!(!status_startable("CREATING"));
        assert!(!status_startable("RUNNING"));
        assert!(status_stopped("STOPPED"));
        assert!(!status_stopped("SUSPENDED"));
    }

    fn sample_sizes() -> Vec<Machine0Size> {
        // Trimmed from a live machine0 `size_list`.
        parse_size_list(&json!([
            {"size":"small","vcpu":1,"ramGb":1,"diskGb":25,"pricePerHourMicro":13000,"gpu":null},
            {"size":"large","vcpu":2,"ramGb":4,"diskGb":80,"pricePerHourMicro":52000,"gpu":null},
            {"size":"large-nvme","vcpu":2,"ramGb":4,"diskGb":80,"pricePerHourMicro":61000,"gpu":null},
            {"size":"xl","vcpu":4,"ramGb":8,"diskGb":160,"pricePerHourMicro":104000,"gpu":null},
            {"size":"gpu-h100","vcpu":8,"ramGb":64,"diskGb":200,"pricePerHourMicro":825000,"gpu":"h100"}
        ]))
    }

    #[test]
    fn parse_size_list_reads_dims_and_flags() {
        let s = sample_sizes();
        assert_eq!(s.len(), 5);
        let nvme = s.iter().find(|z| z.name == "large-nvme").unwrap();
        assert!(nvme.nvme);
        assert_eq!(nvme.ram_gb, 4);
        assert!(s.iter().find(|z| z.name == "gpu-h100").unwrap().gpu);
        assert!(!s.iter().find(|z| z.name == "large").unwrap().gpu);
    }

    #[test]
    fn pick_size_cheapest_meeting_all_dims() {
        let s = sample_sizes();
        // ≥2 cpu / ≥4 GB ⇒ cheapest is "large" (not the pricier nvme/xl).
        assert_eq!(
            pick_size(&s, &SizeReq { min_vcpu: 2, min_ram_gb: 4, ..Default::default() }).as_deref(),
            Some("large")
        );
        // NVMe required ⇒ the nvme variant despite being pricier.
        assert_eq!(
            pick_size(&s, &SizeReq { nvme: true, ..Default::default() }).as_deref(),
            Some("large-nvme")
        );
        // GPU required ⇒ the gpu box (and non-gpu asks never get a gpu box).
        assert_eq!(
            pick_size(&s, &SizeReq { gpu: true, ..Default::default() }).as_deref(),
            Some("gpu-h100")
        );
        // Big disk ⇒ xl (160 GB) is the cheapest that fits ≥120.
        assert_eq!(
            pick_size(&s, &SizeReq { min_disk_gb: 120, ..Default::default() }).as_deref(),
            Some("xl")
        );
        // Unsatisfiable ⇒ None.
        assert_eq!(pick_size(&s, &SizeReq { min_vcpu: 99, ..Default::default() }), None);
        // No constraints, no gpu ⇒ cheapest non-gpu = "small".
        assert_eq!(pick_size(&s, &SizeReq::default()).as_deref(), Some("small"));
    }

    #[test]
    fn parse_vm_extracts_id_status_and_address() {
        let v = json!({
            "id": "uuid-1", "name": "dev", "status": "RUNNING",
            "publicIp": "203.0.113.7", "defaultSSHUsername": "root"
        });
        let vm = parse_vm(&v).unwrap();
        assert_eq!(vm.id, "uuid-1");
        assert_eq!(vm.name, "dev");
        assert!(status_running(&vm.status));
        assert_eq!(vm.address.as_deref(), Some("203.0.113.7"));
        assert_eq!(vm.user.as_deref(), Some("root"));
        // envelope + nested address variants
        let nested = json!({ "machine": { "id": "u2", "state": "running",
            "network": { "ipv4": "198.51.100.9" } } });
        let vm = parse_vm(&nested).unwrap();
        assert_eq!(vm.id, "u2");
        assert_eq!(vm.address.as_deref(), Some("198.51.100.9"));
        // networks array
        let arr = json!({ "id": "u3", "networks": [ { "ip": "192.0.2.5" } ] });
        assert_eq!(parse_vm(&arr).unwrap().address.as_deref(), Some("192.0.2.5"));
        assert!(parse_vm(&json!({})).is_none());
    }

    #[test]
    fn parse_vm_list_names_or_ids() {
        let arr = json!([{ "name": "a" }, { "id": "b" }, { "vm": { "name": "c" } }]);
        assert_eq!(parse_vm_list(&arr), vec!["a", "b", "c"]);
        let env = json!({ "machines": [{ "name": "x" }] });
        assert_eq!(parse_vm_list(&env), vec!["x"]);
        assert!(parse_vm_list(&json!({})).is_empty());
    }

    #[test]
    fn parse_image_list_prefers_name() {
        let v = json!({ "images": [
            { "name": "gm-1", "description": "golden" },
            { "id": "img-2" }
        ]});
        let cps = parse_image_list(&v);
        assert_eq!(cps.len(), 2);
        assert_eq!(cps[0].id, "gm-1");
        assert_eq!(cps[0].label.as_deref(), Some("golden"));
        assert_eq!(cps[1].id, "img-2");
        assert!(cps[1].label.is_none());
    }

    #[test]
    fn parse_ssh_keys_and_created_id() {
        let list = json!({ "sshKeys": [
            { "id": "k1", "publicKey": "ssh-ed25519 AAA me@host" }
        ]});
        let ks = parse_ssh_key_list(&list);
        assert_eq!(ks[0].0, "k1");
        assert!(same_pubkey(&ks[0].1, "ssh-ed25519 AAA other@comment"));
        assert!(!same_pubkey(&ks[0].1, "ssh-ed25519 BBB me@host"));
        assert_eq!(parse_created_id(&json!({ "id": "new-key" })).as_deref(), Some("new-key"));
        assert_eq!(
            parse_created_id(&json!({ "sshKey": { "sshKeyId": "sk-9" } })).as_deref(),
            Some("sk-9")
        );
    }

    #[test]
    fn not_found_detection() {
        assert!(is_not_found("machine0 mcp vm_get_by_name: HTTP 404 Not Found"));
        assert!(is_not_found("machine not found"));
        assert!(!is_not_found("machine0 mcp vm_list: HTTP 500"));
    }
}
