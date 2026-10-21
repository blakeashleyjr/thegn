//! `[host.<name>]` — container-capable machines as first-class config, and the
//! resolution from an `[env.<name>]` to the [`HostBinding`] its sandboxes land
//! on. Hosts are **global config only**: the repo overlay (`.thegn.toml`)
//! structurally cannot define them (its schema has no `host` table), mirroring
//! how envs are select-only there — a repo can pick a host-backed env, never
//! smuggle machine definitions or credentials in.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{
    Config, EnvConfig, EnvSshConfig, PlacementMode, RemoteTransport, config_enum, config_warn,
    expand_env_ref,
};
use crate::host::{CloudReach, DeliveryCap, HostId, IrohReach, Reach, VolumeSpec};
use crate::image::ImageRef;
use crate::placement::{SshPlacement, TransportKind};
use crate::store::HostStore;

/// Default probe TTL: a `Ready` host older than this re-verifies (one cheap
/// batched exec on a warm ControlMaster) before the fast path trusts it.
pub const DEFAULT_PROBE_TTL_SECS: i64 = 900;

/// Maximum encoded length for a persisted host alias.
pub const HOST_ALIAS_MAX_BYTES: usize = 128;

config_enum! {
    /// `[host.<n>] reach` — how thegn reaches this host.
    pub enum HostReach: "host reach" {
        Ssh = "ssh", Iroh = "iroh", Cloud = "cloud", Local = "local",
    } default = Ssh;
}

config_enum! {
    /// `[host.<n>] install_runtime` — consent for bootstrapping a container
    /// runtime on the machine. Installing software on someone's box is a
    /// capability the user grants per-host; it is NEVER implied.
    pub enum InstallConsent: "install consent" {
        Never = "never", Ask = "ask", Auto = "auto" | "always",
    } default = Ask;
}

/// `[host.<name>]` — one machine. Reach-specific knobs live in the matching
/// sub-table (`.ssh` reuses the env table verbatim).
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct HostConfig {
    pub reach: HostReach,
    /// Base image override (`name[:tag][@sha256:…]`); empty ⇒ the LOCAL sandbox
    /// base (`[sandbox] image`, else the built-in digest-pinned `debian:stable`),
    /// transferred to the host — see `Config::default_host_image`.
    pub image: String,
    pub install_runtime: InstallConsent,
    /// Delivery preference order (names per [`DeliveryCap::parse`]); empty ⇒
    /// auto-ranked (registry-less transfer first). Unknown names warn.
    pub delivery: Vec<String>,
    /// Warm volumes to seed (`"nix-store"`, `"cargo"`). Absent ⇒ both;
    /// explicit `[]` ⇒ none. Unknown names warn.
    pub volumes: Option<Vec<String>>,
    /// Probe TTL in seconds (0 ⇒ [`DEFAULT_PROBE_TTL_SECS`]).
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
    pub probe_ttl_secs: u64,
    /// ATTESTATION (taken on faith, never verified): the owner asserts this
    /// machine enforces the egress/config posture a thegn-built image
    /// guarantees, restoring the one-notch trust-class drop every unattested
    /// user-owned host gets for packing. See `thegn_core::trust_class`.
    pub trust_egress_enforced: bool,
    /// Declared machine size for the placement engine's capacity index
    /// (`capacity = { cpu = "8", memory = "16g" }`). Only `cpu`/`memory` are
    /// consulted; empty ⇒ unknown size ⇒ the host serves dedicated placements
    /// but is never packed (an overcommit ceiling over an unknown base is
    /// meaningless). Engine-created hosts get an authoritative spec from
    /// their create template instead.
    #[serde(skip_serializing_if = "crate::config_placement::ResourcesDecl::is_empty")]
    pub capacity: crate::config_placement::ResourcesDecl,
    /// `[host.<n>.ssh]` (reach = ssh) — same knobs as `[env.<n>.ssh]`.
    #[serde(skip_serializing_if = "EnvSshConfig::is_default")]
    pub ssh: EnvSshConfig,
    /// `[host.<n>.iroh]` (reach = iroh).
    #[serde(skip_serializing_if = "HostIrohConfig::is_default")]
    pub iroh: HostIrohConfig,
    /// `[host.<n>.cloud]` (reach = cloud).
    #[serde(skip_serializing_if = "HostCloudConfig::is_default")]
    pub cloud: HostCloudConfig,
}

/// Compare the connection authority behind two host definitions. Display and
/// provisioning preferences intentionally do not participate: re-adding the
/// same machine may update its image or install consent without rebinding the
/// host name to a different authority.
pub fn same_host_authority(a: &HostConfig, b: &HostConfig) -> bool {
    if a.reach != b.reach {
        return false;
    }
    match a.reach {
        HostReach::Local => true,
        HostReach::Ssh => {
            a.ssh.host == b.ssh.host
                && a.ssh.port == b.ssh.port
                && a.ssh.transport == b.ssh.transport
                && a.ssh.ssh_config == b.ssh.ssh_config
                && a.ssh.jump_host == b.ssh.jump_host
                && a.ssh.identity == b.ssh.identity
                && a.ssh.extra_args == b.ssh.extra_args
        }
        HostReach::Iroh => {
            a.iroh.ticket == b.iroh.ticket
                && a.iroh.user == b.iroh.user
                && a.iroh.ssh_port == b.iroh.ssh_port
        }
        HostReach::Cloud => {
            a.cloud.provider == b.cloud.provider
                && a.cloud.api_base == b.cloud.api_base
                && a.cloud.template == b.cloud.template
                && a.cloud.api_key_env == b.cloud.api_key_env
        }
    }
}

/// Validate the single-segment name used to identify a persisted host.
pub fn validate_host_alias(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > HOST_ALIAS_MAX_BYTES {
        return Err(format!(
            "host alias {name:?} must be 1..={HOST_ALIAS_MAX_BYTES} bytes"
        ));
    }
    if name == "local" {
        return Err("host alias \"local\" is reserved for the local host".into());
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name.as_bytes()[name.len() - 1].is_ascii_alphanumeric()
    {
        return Err(format!(
            "host alias {name:?} must contain only lowercase ASCII letters, digits, or hyphens and begin and end with a letter or digit"
        ));
    }
    Ok(())
}

/// Stable SHA-256 digest of exactly the connection authority compared by
/// [`same_host_authority`]. Provisioning preferences and display settings are
/// deliberately excluded.
pub fn host_authority_digest(hc: &HostConfig) -> String {
    let encoded = match hc.reach {
        HostReach::Ssh => serde_json::to_vec(&(
            hc.reach.as_str(),
            &hc.ssh.host,
            hc.ssh.port,
            &hc.ssh.transport,
            &hc.ssh.ssh_config,
            &hc.ssh.jump_host,
            &hc.ssh.identity,
            &hc.ssh.extra_args,
        )),
        HostReach::Iroh => serde_json::to_vec(&(
            hc.reach.as_str(),
            &hc.iroh.ticket,
            &hc.iroh.user,
            hc.iroh.ssh_port,
        )),
        HostReach::Cloud => serde_json::to_vec(&(
            hc.reach.as_str(),
            &hc.cloud.provider,
            &hc.cloud.api_base,
            &hc.cloud.template,
            &hc.cloud.api_key_env,
        )),
        HostReach::Local => serde_json::to_vec(&(hc.reach.as_str(),)),
    }
    .expect("host authority serializes");
    let mut digest = Sha256::new();
    digest.update(b"thegn-host-authority-v1\0");
    digest.update(encoded);
    format!("{:x}", digest.finalize())
}

/// Select a stable, collision-resistant alias for a host authority.
pub fn default_host_alias(
    derived: &str,
    hc: &HostConfig,
    db_defs: &[(String, HostConfig)],
    config_only: &dyn Fn(&str) -> bool,
) -> Result<String, String> {
    if let Some((alias, _)) = db_defs.iter().find(|(_, def)| same_host_authority(def, hc)) {
        validate_host_alias(alias)
            .map_err(|error| format!("{error}; pass --name to choose a valid alias"))?;
        return Ok(alias.clone());
    }
    let db_taken = |name: &str| db_defs.iter().any(|(stored, _)| stored == name);
    let alias = if !db_taken(derived) && !config_only(derived) {
        derived.to_string()
    } else {
        let digest = host_authority_digest(hc);
        format!("{derived}-{}", &digest[..12])
    };
    validate_host_alias(&alias)
        .map_err(|error| format!("{error}; pass --name to choose a valid alias"))?;
    if db_taken(&alias) || config_only(&alias) {
        return Err(format!(
            "host alias {alias:?} is already taken; pass --name to choose another alias"
        ));
    }
    Ok(alias)
}

/// `[host.<n>.iroh]` — a NAT'd host behind a dumbpipe listener.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct HostIrohConfig {
    /// dumbpipe node ticket; supports `env:VAR` / `file:PATH` secret refs.
    pub ticket: String,
    /// The sshd port the remote listener fronts.
    pub ssh_port: u16,
    /// SSH user for the forwarded session.
    pub user: String,
}

impl Default for HostIrohConfig {
    fn default() -> Self {
        HostIrohConfig {
            ticket: String::new(),
            ssh_port: 22,
            user: String::new(),
        }
    }
}

impl HostIrohConfig {
    fn is_default(&self) -> bool {
        self.ticket.is_empty() && self.user.is_empty() && self.ssh_port == 22
    }
}

/// `[host.<n>.cloud]` — a provider-managed host.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct HostCloudConfig {
    /// Provider id (`"sprites"`, `"daytona"`).
    pub provider: String,
    /// API base URL (empty ⇒ the provider's default).
    pub api_base: String,
    /// Env var holding the API token.
    pub api_key_env: String,
    /// Template / snapshot scope the base image registers under.
    pub template: String,
}

impl HostCloudConfig {
    fn is_default(&self) -> bool {
        self.provider.is_empty()
            && self.api_base.is_empty()
            && self.api_key_env.is_empty()
            && self.template.is_empty()
    }
}

/// The resolved binding of an env (or a named host) to a machine: everything
/// `ensure_ready` needs to drive the state machine.
#[derive(Debug, Clone)]
pub struct HostBinding {
    pub id: HostId,
    pub reach: Reach,
    pub consent: InstallConsent,
    pub image: ImageRef,
    pub volumes: Vec<VolumeSpec>,
    pub delivery_prefs: Vec<DeliveryCap>,
    pub probe_ttl_secs: i64,
    /// Declared machine size (`[host.<n>] capacity`) for the placement
    /// engine's capacity index; `None` = unknown (never packed).
    pub declared_spec: Option<crate::capacity::HostSpec>,
}

/// Default warm-volume set when `[host.<n>] volumes` is absent.
fn default_volumes() -> Vec<VolumeSpec> {
    VolumeSpec::from_names(&["nix-store".into(), "cargo".into()])
}

fn parse_image(name: &str, raw: &str, default: &ImageRef) -> ImageRef {
    if raw.trim().is_empty() {
        return default.clone();
    }
    match ImageRef::parse(raw) {
        Ok(r) => r,
        Err(e) => {
            config_warn(&format!("[host.{name}] image: {e}; using the default base"));
            default.clone()
        }
    }
}

fn parse_delivery(name: &str, prefs: &[String]) -> Vec<DeliveryCap> {
    let mut out = Vec::new();
    for p in prefs {
        match DeliveryCap::parse(p) {
            Some(c) if !out.contains(&c) => out.push(c),
            Some(_) => {}
            None => config_warn(&format!(
                "[host.{name}] delivery: unknown strategy {p:?} (expected e.g. \
                 \"ssh-stream\", \"rsync\", \"registry\", \"skopeo\", \"build\")"
            )),
        }
    }
    out
}

/// Lower a `[host.<n>] capacity` declaration to a spec; partial/unparseable
/// declarations warn and read as unknown (never a half-spec).
fn parse_capacity(
    name: &str,
    decl: &crate::config_placement::ResourcesDecl,
) -> Option<crate::capacity::HostSpec> {
    if decl.is_empty() {
        return None;
    }
    let cpu = crate::capacity::parse_cpu_milli(&decl.cpu);
    let mem = crate::capacity::parse_mem_mb(&decl.memory);
    match (cpu, mem) {
        (Some(c), Some(m)) => Some(crate::capacity::HostSpec {
            cpu_milli: c,
            mem_mb: m,
        }),
        _ => {
            config_warn(&format!(
                "[host.{name}] capacity: needs parseable cpu + memory (e.g. \
                 cpu = \"8\", memory = \"16g\"); treating the size as unknown"
            ));
            None
        }
    }
}

fn parse_volumes(name: &str, volumes: &Option<Vec<String>>) -> Vec<VolumeSpec> {
    match volumes {
        None => default_volumes(),
        Some(names) => {
            for n in names {
                if VolumeSpec::by_name(n).is_none() {
                    config_warn(&format!("[host.{name}] volumes: unknown volume {n:?}"));
                }
            }
            VolumeSpec::from_names(names)
        }
    }
}

/// Build an [`SshPlacement`] from the shared ssh table. `None` when no target
/// host is set.
fn ssh_placement(ssh: &EnvSshConfig) -> Option<SshPlacement> {
    let host = ssh.host.trim();
    if host.is_empty() {
        return None;
    }
    let opt = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    Some(SshPlacement {
        host: host.to_string(),
        port: if ssh.port == 0 { 22 } else { ssh.port },
        forward_agent: ssh.forward_agent,
        // `kind` only steers the INTERACTIVE pane (mosh vs ssh -t); the control
        // plane always wraps with batch ssh regardless (see `control_argv`). So
        // honour the host's `transport` here — a host-pinned env's pane reaches
        // the box exactly as configured (e.g. `transport = "ssh"` for a box with
        // no mosh), instead of silently defaulting to mosh.
        kind: match ssh.transport {
            RemoteTransport::Ssh => TransportKind::Ssh,
            RemoteTransport::Mosh => TransportKind::Mosh,
        },
        ssh_config: opt(&ssh.ssh_config),
        jump_host: opt(&ssh.jump_host),
        identity: opt(&ssh.identity),
        extra_args: ssh.extra_args.clone(),
    })
}

/// Merge USER-ADDED host definitions (the DB rows written by the in-TUI /
/// CLI "add host" flow) into the loaded config catalog: each def becomes a
/// `[host.<name>]` entry, and — for reaches with a pane transport — a matching
/// `[env.<name>]` (placement + ssh table + `host = name`) so DB hosts flow
/// through the wizard/palette/resolution with zero call-site changes.
/// Declarative config SHADOWS DB defs of the same name (config is the
/// catalog; the DB is the user's runtime additions).
pub fn merge_host_defs(cfg: &mut Config, defs: &[(String, HostConfig)]) {
    for (name, hc) in defs {
        let effective = cfg.host.entry(name.clone()).or_insert_with(|| hc.clone());
        if cfg.env.contains_key(name) {
            continue;
        }
        // Synthesize the selectable env. iroh hosts get no pane transport yet
        // (provisioning works; interactive panes over iroh are a follow-up),
        // so they surface in the Hosts panel/CLI but not the env list.
        // The selectable environment must use the same winning definition as
        // the host catalog, including its connection settings.
        let placement = match effective.reach {
            HostReach::Ssh => PlacementMode::Ssh,
            HostReach::Local => PlacementMode::Local,
            HostReach::Iroh | HostReach::Cloud => continue,
        };
        cfg.env.insert(
            name.clone(),
            EnvConfig {
                placement,
                host: name.clone(),
                ssh: effective.ssh.clone(),
                ..EnvConfig::default()
            },
        );
    }
}

/// [`merge_host_defs`] fed from the state DB — best-effort (a missing/locked
/// DB just means no user-added hosts this load). Call after `load_layered`.
pub fn merge_db_hosts(cfg: &mut Config) {
    if let Ok(db) = crate::db::Db::open()
        && let Ok(defs) = db.host_defs()
    {
        merge_host_defs(cfg, &defs);
    }
}

/// Parse an "add host" target string into a [`HostConfig`]:
/// `user@host[:port]` ⇒ ssh reach; `dumbpipe:<ticket>` (+ `user`) ⇒ iroh.
/// The name defaults to a slug of the hostname when the caller passes none.
pub fn parse_host_target(
    target: &str,
    iroh_user: Option<&str>,
) -> Result<(String, HostConfig), String> {
    let t = target.trim();
    if t.is_empty() {
        return Err("empty host target".into());
    }
    if let Some(ticket) = t.strip_prefix("dumbpipe:") {
        let user = iroh_user
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .ok_or("iroh host needs a user (user@ prefix is not part of a ticket)")?;
        let hc = HostConfig {
            reach: HostReach::Iroh,
            iroh: HostIrohConfig {
                ticket: ticket.trim().to_string(),
                user: user.to_string(),
                ..HostIrohConfig::default()
            },
            ..HostConfig::default()
        };
        let name = format!("iroh-{}", &host_authority_digest(&hc)[..32]);
        return Ok((name, hc));
    }
    // user@host[:port]
    let (rest, port) = match t.rsplit_once(':') {
        Some((r, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (r, p.parse::<u16>().map_err(|_| format!("bad port {p:?}"))?)
        }
        _ => (t, 22),
    };
    if rest.is_empty() || rest.ends_with('@') {
        return Err(format!("bad ssh target {t:?} (want user@host[:port])"));
    }
    let hostname = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    let name = crate::util::slugify(hostname);
    if name.is_empty() {
        return Err(format!("bad ssh target {t:?}"));
    }
    let hc = HostConfig {
        reach: HostReach::Ssh,
        ssh: crate::config::EnvSshConfig {
            host: rest.to_string(),
            port,
            ..crate::config::EnvSshConfig::default()
        },
        ..HostConfig::default()
    };
    Ok((name, hc))
}

impl Config {
    /// The base image a host delivers when `[host.<n>] image` is unset (or
    /// unparseable): the **local** sandbox base — `[sandbox] image` if set,
    /// else the built-in digest-pinned `debian:stable` — so a plain `[host.*]`
    /// mirrors your
    /// local environment onto the remote (the delivery step transfers it) and
    /// works out of the box, rather than depending on a separately-published
    /// registry image.
    fn default_host_image(&self) -> ImageRef {
        let raw = self.sandbox.image.trim();
        let src = if raw.is_empty() {
            crate::sandbox::DEFAULT_OCI_IMAGE
        } else {
            raw
        };
        // `[sandbox] image` is already validated at load; fall back defensively.
        ImageRef::parse(src).unwrap_or_else(|_| ImageRef::default_base())
    }

    /// The binding for a named `[host.<name>]` entry. `None` when undefined or
    /// (with a warning) misconfigured for its reach.
    pub fn host_binding(&self, name: &str) -> Option<HostBinding> {
        let hc = self.host.get(name)?;
        let reach = match hc.reach {
            HostReach::Local => Reach::Local,
            HostReach::Ssh => match ssh_placement(&hc.ssh) {
                Some(p) => Reach::Ssh(p),
                None => {
                    config_warn(&format!(
                        "[host.{name}] reach = \"ssh\" needs [host.{name}.ssh] host"
                    ));
                    return None;
                }
            },
            HostReach::Iroh => {
                let Some(ticket) = expand_env_ref(&hc.iroh.ticket) else {
                    config_warn(&format!(
                        "[host.{name}] reach = \"iroh\" needs [host.{name}.iroh] ticket"
                    ));
                    return None;
                };
                if hc.iroh.user.trim().is_empty() {
                    config_warn(&format!(
                        "[host.{name}] reach = \"iroh\" needs [host.{name}.iroh] user"
                    ));
                    return None;
                }
                Reach::Iroh(IrohReach {
                    ticket,
                    ssh_port: if hc.iroh.ssh_port == 0 {
                        22
                    } else {
                        hc.iroh.ssh_port
                    },
                    user: hc.iroh.user.trim().to_string(),
                })
            }
            HostReach::Cloud => {
                if hc.cloud.provider.trim().is_empty() {
                    config_warn(&format!(
                        "[host.{name}] reach = \"cloud\" needs [host.{name}.cloud] provider"
                    ));
                    return None;
                }
                Reach::Cloud(CloudReach {
                    provider: hc.cloud.provider.trim().to_string(),
                    api_base: hc.cloud.api_base.trim().to_string(),
                    api_key_env: hc.cloud.api_key_env.trim().to_string(),
                    template: hc.cloud.template.trim().to_string(),
                })
            }
        };
        let id = match &reach {
            Reach::Cloud(c) => HostId::cloud(&c.provider, &c.template),
            _ => HostId::named(name),
        };
        Some(HostBinding {
            id,
            reach,
            consent: hc.install_runtime,
            image: parse_image(name, &hc.image, &self.default_host_image()),
            volumes: parse_volumes(name, &hc.volumes),
            delivery_prefs: parse_delivery(name, &hc.delivery),
            probe_ttl_secs: if hc.probe_ttl_secs == 0 {
                DEFAULT_PROBE_TTL_SECS
            } else {
                crate::time_policy::saturating_i64(u128::from(hc.probe_ttl_secs))
            },
            declared_spec: parse_capacity(name, &hc.capacity),
        })
    }

    /// Resolve the host an env lands on:
    ///
    /// 1. `[env.<n>] host = "name"` with `[host.name]` defined ⇒ that host.
    /// 2. `host` set but undefined ⇒ warn + fall through (never a hard fail).
    /// 3. No ref, `placement = "ssh"` with an inline target ⇒ an IMPLICIT
    ///    ANONYMOUS host: same id for the same `user@host:port`, so two envs on
    ///    one box share the setup. Consent is `never` — bootstrapping a machine
    ///    requires an explicit `[host.*]` opt-in — which preserves today's
    ///    behavior for inline-ssh envs (plus digest pinning + inventory).
    /// 4. `placement = "provider"` ⇒ a cloud host from the provider table.
    /// 5. `placement = "local"` / `"k8s"` ⇒ `None` (no host lifecycle).
    pub fn resolve_host_binding(&self, env_name: &str, envc: &EnvConfig) -> Option<HostBinding> {
        let host_ref = envc.host.trim();
        if !host_ref.is_empty() {
            if let Some(b) = self.host_binding(host_ref) {
                return Some(b);
            }
            if !self.host.contains_key(host_ref) {
                config_warn(&format!(
                    "[env.{env_name}] host = {host_ref:?} is not a defined [host.*] \
                     (hosts are global config only); falling back to the env's inline \
                     placement"
                ));
            }
        }
        match envc.placement {
            PlacementMode::Ssh => {
                let p = ssh_placement(&envc.ssh)?;
                Some(HostBinding {
                    id: HostId::anon_ssh(&p.host, p.port),
                    reach: Reach::Ssh(p),
                    consent: InstallConsent::Never,
                    image: self.default_host_image(),
                    volumes: default_volumes(),
                    delivery_prefs: Vec::new(),
                    probe_ttl_secs: DEFAULT_PROBE_TTL_SECS,
                    declared_spec: None,
                })
            }
            PlacementMode::Provider => {
                let pc = &envc.provider;
                if pc.provider.trim().is_empty() {
                    return None;
                }
                let template = if pc.template.trim().is_empty() {
                    "default"
                } else {
                    pc.template.trim()
                };
                Some(HostBinding {
                    id: HostId::cloud(pc.provider.trim(), template),
                    reach: Reach::Cloud(CloudReach {
                        provider: pc.provider.trim().to_string(),
                        api_base: pc.api_base.trim().to_string(),
                        api_key_env: pc.api_key_env.trim().to_string(),
                        template: template.to_string(),
                    }),
                    consent: InstallConsent::Never,
                    image: ImageRef::default_base(),
                    volumes: Vec::new(), // cloud folds volumes into the checkpoint
                    delivery_prefs: Vec::new(),
                    probe_ttl_secs: DEFAULT_PROBE_TTL_SECS,
                    declared_spec: None,
                })
            }
            PlacementMode::Local | PlacementMode::K8s => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn named_ssh_host_binds() {
        let cfg = cfg_from(
            r#"
            [host.gpu-box]
            reach = "ssh"
            install_runtime = "auto"
            delivery = ["registry", "ssh-stream", "warp-drive"]
            probe_ttl_secs = 60
            [host.gpu-box.ssh]
            host = "blake@gpu.internal"
            identity = "~/.ssh/id_ed25519"
            "#,
        );
        let b = cfg.host_binding("gpu-box").unwrap();
        assert_eq!(b.id, HostId::named("gpu-box"));
        let Reach::Ssh(p) = &b.reach else { panic!() };
        assert_eq!(p.host, "blake@gpu.internal");
        assert_eq!(p.port, 22);
        assert_eq!(p.identity.as_deref(), Some("~/.ssh/id_ed25519"));
        assert_eq!(b.consent, InstallConsent::Auto);
        assert_eq!(
            b.delivery_prefs,
            vec![DeliveryCap::RegistryPull, DeliveryCap::SshStream],
            "unknown pref warned + dropped"
        );
        assert_eq!(b.probe_ttl_secs, 60);
        assert_eq!(b.volumes.len(), 2, "absent volumes ⇒ default set");
        // Unset `[host.*] image` ⇒ the local sandbox base (here the built-in
        // debian, since no `[sandbox] image` is set), transferred to the host.
        // Compare the whole reference, not `name_tag()`: that drops the digest,
        // so it would still pass if a remote host silently delivered a
        // different build of the same tag than the local sandbox runs.
        assert_eq!(
            b.image,
            crate::image::ImageRef::parse(crate::sandbox::DEFAULT_OCI_IMAGE).unwrap()
        );
    }

    #[test]
    fn ssh_host_without_target_is_rejected() {
        let cfg = cfg_from("[host.broken]\nreach = \"ssh\"\n");
        assert!(cfg.host_binding("broken").is_none());
        assert!(cfg.host_binding("undefined").is_none());
    }

    #[test]
    fn iroh_host_binds_and_validates() {
        let cfg = cfg_from(
            r#"
            [host.laptop]
            reach = "iroh"
            volumes = []
            [host.laptop.iroh]
            ticket = "nodeabc123"
            user = "blake"
            "#,
        );
        let b = cfg.host_binding("laptop").unwrap();
        assert_eq!(b.id, HostId::named("laptop"));
        let Reach::Iroh(i) = &b.reach else { panic!() };
        assert_eq!(
            (i.ticket.as_str(), i.ssh_port, i.user.as_str()),
            ("nodeabc123", 22, "blake")
        );
        assert!(b.volumes.is_empty(), "explicit [] ⇒ no volumes");

        let missing_user =
            cfg_from("[host.l2]\nreach = \"iroh\"\n[host.l2.iroh]\nticket = \"t\"\n");
        assert!(missing_user.host_binding("l2").is_none());
        let missing_ticket =
            cfg_from("[host.l3]\nreach = \"iroh\"\n[host.l3.iroh]\nuser = \"u\"\n");
        assert!(missing_ticket.host_binding("l3").is_none());
    }

    #[test]
    fn cloud_host_binds_with_cloud_id() {
        let cfg = cfg_from(
            r#"
            [host.sprites-us]
            reach = "cloud"
            [host.sprites-us.cloud]
            provider = "sprites"
            api_key_env = "SPRITES_TOKEN"
            template = "thegn-base"
            "#,
        );
        let b = cfg.host_binding("sprites-us").unwrap();
        assert_eq!(b.id, HostId::cloud("sprites", "thegn-base"));
        let Reach::Cloud(c) = &b.reach else { panic!() };
        assert_eq!(c.api_key_env, "SPRITES_TOKEN");

        let no_provider = cfg_from("[host.c]\nreach = \"cloud\"\n");
        assert!(no_provider.host_binding("c").is_none());
    }

    #[test]
    fn local_reach_binds() {
        let cfg = cfg_from("[host.here]\nreach = \"local\"\n");
        let b = cfg.host_binding("here").unwrap();
        assert!(matches!(b.reach, Reach::Local));
        assert_eq!(b.id, HostId::named("here"));
    }

    #[test]
    fn env_host_ref_resolves_and_shares() {
        let cfg = cfg_from(
            r#"
            [host.box]
            reach = "ssh"
            install_runtime = "ask"
            [host.box.ssh]
            host = "blake@box"

            [env.gpu]
            placement = "ssh"
            host = "box"
            [env.gpu-heavy]
            placement = "ssh"
            host = "box"
            "#,
        );
        let a = cfg
            .resolve_host_binding("gpu", cfg.env.get("gpu").unwrap())
            .unwrap();
        let b = cfg
            .resolve_host_binding("gpu-heavy", cfg.env.get("gpu-heavy").unwrap())
            .unwrap();
        assert_eq!(a.id, b.id, "two envs share one host record");
        assert_eq!(a.consent, InstallConsent::Ask);
    }

    #[test]
    fn undefined_host_ref_warns_and_falls_back_to_inline() {
        let cfg = cfg_from(
            r#"
            [env.gpu]
            placement = "ssh"
            host = "nope"
            [env.gpu.ssh]
            host = "blake@box"
            port = 2222
            "#,
        );
        let b = cfg
            .resolve_host_binding("gpu", cfg.env.get("gpu").unwrap())
            .unwrap();
        assert_eq!(b.id, HostId::anon_ssh("blake@box", 2222));
        assert_eq!(
            b.consent,
            InstallConsent::Never,
            "anonymous ⇒ never install"
        );
    }

    #[test]
    fn inline_ssh_env_gets_stable_anonymous_host() {
        let cfg = cfg_from(
            r#"
            [env.a]
            placement = "ssh"
            [env.a.ssh]
            host = "blake@box"
            [env.b]
            placement = "ssh"
            [env.b.ssh]
            host = "blake@box"
            [env.other]
            placement = "ssh"
            [env.other.ssh]
            host = "blake@elsewhere"
            "#,
        );
        let a = cfg
            .resolve_host_binding("a", cfg.env.get("a").unwrap())
            .unwrap();
        let b = cfg
            .resolve_host_binding("b", cfg.env.get("b").unwrap())
            .unwrap();
        let o = cfg
            .resolve_host_binding("other", cfg.env.get("other").unwrap())
            .unwrap();
        assert_eq!(a.id, b.id, "same target ⇒ same anonymous host");
        assert_ne!(a.id, o.id);
    }

    #[test]
    fn provider_env_lowers_to_cloud_host() {
        let cfg = cfg_from(
            r#"
            [env.sprite]
            placement = "provider"
            [env.sprite.provider]
            provider = "sprites"
            api_key_env = "SPRITES_TOKEN"
            "#,
        );
        let b = cfg
            .resolve_host_binding("sprite", cfg.env.get("sprite").unwrap())
            .unwrap();
        assert_eq!(b.id, HostId::cloud("sprites", "default"));
        assert!(b.volumes.is_empty(), "cloud folds volumes into checkpoints");
    }

    #[test]
    fn local_and_k8s_envs_have_no_host() {
        let cfg = cfg_from(
            r#"
            [env.here]
            placement = "local"
            [env.pods]
            placement = "k8s"
            [env.pods.k8s]
            pod = "dev"
            "#,
        );
        assert!(
            cfg.resolve_host_binding("here", cfg.env.get("here").unwrap())
                .is_none()
        );
        assert!(
            cfg.resolve_host_binding("pods", cfg.env.get("pods").unwrap())
                .is_none()
        );
        // ssh env with no target anywhere: nothing to bind.
        let empty = cfg_from("[env.s]\nplacement = \"ssh\"\n");
        assert!(
            empty
                .resolve_host_binding("s", empty.env.get("s").unwrap())
                .is_none()
        );
    }

    #[test]
    fn host_image_override_parses_and_bad_ref_falls_back() {
        let cfg = cfg_from(
            "[host.a]\nreach = \"local\"\nimage = \"ghcr.io/me/base:v2\"\n\
             [host.b]\nreach = \"local\"\nimage = \"@bad\"\n",
        );
        assert_eq!(
            cfg.host_binding("a").unwrap().image.name_tag(),
            "ghcr.io/me/base:v2"
        );
        assert_eq!(
            cfg.host_binding("b").unwrap().image,
            crate::image::ImageRef::parse(crate::sandbox::DEFAULT_OCI_IMAGE).unwrap(),
            "unparseable override warns + uses the local sandbox base"
        );
    }

    #[test]
    fn unset_host_image_follows_the_local_sandbox_base() {
        // "Transfer the local system": an unset `[host.*] image` follows the
        // local `[sandbox] image` (which delivery then transfers to the remote),
        // not a separately-published registry ref.
        let cfg = cfg_from(
            "[sandbox]\nimage = \"ghcr.io/me/dev-base:v9\"\n\
             [host.x]\nreach = \"ssh\"\n[host.x.ssh]\nhost = \"me@box\"\n",
        );
        assert_eq!(
            cfg.host_binding("x").unwrap().image.name_tag(),
            "ghcr.io/me/dev-base:v9"
        );
        // No `[sandbox] image` ⇒ the built-in debian base (pullable, works OOTB),
        // and the same for an inline-ssh env with no `[host.*]`.
        let plain = cfg_from("[host.y]\nreach = \"ssh\"\n[host.y.ssh]\nhost = \"me@box\"\n");
        assert_eq!(
            plain.host_binding("y").unwrap().image,
            crate::image::ImageRef::parse(crate::sandbox::DEFAULT_OCI_IMAGE).unwrap()
        );
    }

    #[test]
    fn consent_enum_parses_aliases() {
        assert_eq!(
            InstallConsent::from_str_validated("always").unwrap(),
            InstallConsent::Auto
        );
        assert_eq!(
            InstallConsent::from_str_validated("ask").unwrap(),
            InstallConsent::Ask
        );
        assert!(InstallConsent::from_str_validated("maybe").is_err());
        assert_eq!(InstallConsent::default(), InstallConsent::Ask);
        assert_eq!(HostReach::default(), HostReach::Ssh);
    }

    #[test]
    fn merge_host_defs_synthesizes_envs_and_config_shadows() {
        let mut cfg = cfg_from("[host.box]\nreach = \"local\"\nimage = \"cfg:wins\"\n");
        let ssh_def = HostConfig {
            reach: HostReach::Ssh,
            ssh: crate::config::EnvSshConfig {
                host: "blake@gpu".into(),
                ..crate::config::EnvSshConfig::default()
            },
            ..HostConfig::default()
        };
        let shadowed = HostConfig {
            reach: HostReach::Local,
            image: "db:loses".into(),
            ..HostConfig::default()
        };
        let iroh_def = HostConfig {
            reach: HostReach::Iroh,
            iroh: HostIrohConfig {
                ticket: "t".into(),
                user: "u".into(),
                ..HostIrohConfig::default()
            },
            ..HostConfig::default()
        };
        merge_host_defs(
            &mut cfg,
            &[
                ("gpu".into(), ssh_def),
                ("box".into(), shadowed),
                ("nat".into(), iroh_def),
            ],
        );
        // config [host.box] shadows the DB def of the same name.
        assert_eq!(cfg.host["box"].image, "cfg:wins");
        // ssh def lands with a synthesized selectable env carrying host=.
        assert_eq!(cfg.host["gpu"].ssh.host, "blake@gpu");
        let env = cfg.env.get("gpu").expect("env synthesized");
        assert_eq!(env.host, "gpu");
        assert_eq!(env.placement, PlacementMode::Ssh);
        assert_eq!(env.ssh.host, "blake@gpu");
        // iroh def: host present (panel/CLI) but no pane env yet.
        assert!(cfg.host.contains_key("nat"));
        assert!(!cfg.env.contains_key("nat"));
        // idempotent
        let before = cfg.env.len();
        merge_host_defs(&mut cfg.clone(), &[]);
        assert_eq!(cfg.env.len(), before);
    }

    #[test]
    fn merge_host_defs_never_clobbers_existing_envs() {
        let mut cfg = cfg_from("[env.gpu]\nplacement = \"local\"\n");
        let d = HostConfig {
            reach: HostReach::Ssh,
            ssh: crate::config::EnvSshConfig {
                host: "blake@gpu".into(),
                ..crate::config::EnvSshConfig::default()
            },
            ..HostConfig::default()
        };
        merge_host_defs(&mut cfg, &[("gpu".into(), d)]);
        assert_eq!(
            cfg.env["gpu"].placement,
            PlacementMode::Local,
            "user's env wins"
        );
        assert!(cfg.host.contains_key("gpu"), "host still added");
    }

    #[test]
    fn parse_host_target_forms() {
        let (name, hc) = parse_host_target("blake@gpu.internal", None).unwrap();
        assert_eq!(name, "gpu-internal");
        assert_eq!(hc.reach, HostReach::Ssh);
        assert_eq!(hc.ssh.host, "blake@gpu.internal");
        assert_eq!(hc.ssh.port, 22);

        let (_, hc) = parse_host_target("blake@box:2222", None).unwrap();
        assert_eq!(hc.ssh.port, 2222);

        let (name, hc) = parse_host_target("box.example.com", None).unwrap();
        assert_eq!(
            name, "box-example-com",
            "bare hostname works (ssh config user)"
        );
        assert_eq!(hc.ssh.host, "box.example.com");

        let (name, hc) = parse_host_target("dumbpipe:nodeabc", Some("blake")).unwrap();
        assert!(name.starts_with("iroh-"));
        assert_eq!(hc.reach, HostReach::Iroh);
        assert_eq!(hc.iroh.ticket, "nodeabc");
        assert_eq!(hc.iroh.user, "blake");

        assert!(
            parse_host_target("dumbpipe:x", None).is_err(),
            "iroh needs user"
        );
        assert!(parse_host_target("", None).is_err());
        assert!(parse_host_target("blake@", None).is_err());
        assert!(
            parse_host_target("blake@box:notaport", None)
                .unwrap()
                .1
                .ssh
                .port
                == 22,
            "non-numeric suffix is part of the hostname, not a port"
        );
    }

    #[test]
    fn host_alias_validator_rejects_malformed() {
        for invalid in ["", "-a", "a-", "A", "a_b", "a.b", "local"] {
            assert!(
                validate_host_alias(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        assert!(validate_host_alias(&"a".repeat(HOST_ALIAS_MAX_BYTES + 1)).is_err());
        assert!(validate_host_alias(&"a".repeat(HOST_ALIAS_MAX_BYTES)).is_ok());
        for valid in ["a", "gpu-internal", "a-1b2c3d4e5f6a"] {
            assert!(validate_host_alias(valid).is_ok(), "rejected {valid:?}");
        }
    }

    #[test]
    fn host_alias_default_suffixes_an_occupied_slug() {
        let (first_name, first) = parse_host_target("me@a.b", None).unwrap();
        let (second_name, second) = parse_host_target("me@a-b", None).unwrap();
        assert_eq!(first_name, "a-b");
        assert_eq!(second_name, "a-b");
        let defs = vec![(first_name.clone(), first.clone())];
        assert_eq!(
            default_host_alias(&first_name, &first, &defs, &|_| false).unwrap(),
            "a-b"
        );
        let resolved = default_host_alias(&second_name, &second, &defs, &|_| false).unwrap();
        assert!(resolved.starts_with("a-b-"));
        assert_eq!(
            resolved,
            format!("a-b-{}", &host_authority_digest(&second)[..12])
        );
        assert_eq!(resolved.len(), "a-b-".len() + 12);
        assert!(
            resolved["a-b-".len()..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        let both_defs = vec![(first_name, first), (resolved.clone(), second.clone())];
        assert_eq!(
            default_host_alias(&second_name, &second, &both_defs, &|_| false).unwrap(),
            resolved
        );
    }

    #[test]
    fn host_alias_default_reuses_an_existing_authority() {
        let (_, hc) = parse_host_target("me@gpu.internal", None).unwrap();
        let defs = vec![("gpu".to_string(), hc.clone())];
        assert_eq!(
            default_host_alias("gpu-internal", &hc, &defs, &|_| false).unwrap(),
            "gpu"
        );
        let malformed_legacy = vec![("Bad_Name".to_string(), hc.clone())];
        assert!(
            default_host_alias("gpu-internal", &hc, &malformed_legacy, &|_| false)
                .is_err_and(|error| error.contains("--name"))
        );
    }

    #[test]
    fn host_alias_default_uses_a_free_slug() {
        let (_, hc) = parse_host_target("me@free.example", None).unwrap();
        assert_eq!(
            default_host_alias("free-example", &hc, &[], &|_| false).unwrap(),
            "free-example"
        );
    }

    #[test]
    fn host_alias_config_only_name_gets_suffix() {
        let (_, hc) = parse_host_target("me@example.test", None).unwrap();
        let resolved =
            default_host_alias("example-test", &hc, &[], &|name| name == "example-test").unwrap();
        assert!(resolved.starts_with("example-test-"));
        assert_eq!(resolved.len(), "example-test-".len() + 12);
    }

    #[test]
    fn host_alias_suffix_collision_and_overlong_default_refuse() {
        let (_, hc) = parse_host_target("me@example.test", None).unwrap();
        let suffix = format!("example-test-{}", &host_authority_digest(&hc)[..12]);
        let defs = vec![(suffix.clone(), HostConfig::default())];
        assert!(
            default_host_alias("example-test", &hc, &defs, &|_| false)
                .is_err_and(|error| error.contains("--name"))
        );
        assert!(
            default_host_alias("example-test", &hc, &[], &|name| name == suffix.as_str())
                .is_err_and(|error| error.contains("--name"))
        );
        assert!(
            default_host_alias(&"a".repeat(HOST_ALIAS_MAX_BYTES), &hc, &[], &|_| false)
                .is_err_and(|error| error.contains("--name"))
        );
    }

    #[test]
    fn host_alias_iroh_digest_is_full_strength() {
        let (name, first) = parse_host_target("dumbpipe:ticket-one", Some("me")).unwrap();
        let (other_name, other) = parse_host_target("dumbpipe:ticket-two", Some("me")).unwrap();
        assert!(name.starts_with("iroh-"));
        assert_eq!(name.len(), "iroh-".len() + 32);
        assert_eq!(
            name,
            format!("iroh-{}", &host_authority_digest(&first)[..32])
        );
        assert!(
            name["iroh-".len()..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_ne!(name, other_name);
        assert_eq!(host_authority_digest(&first).len(), 64);
        let mut same_authority_with_different_preferences = first.clone();
        same_authority_with_different_preferences.image = "updated-image".into();
        assert!(same_host_authority(
            &first,
            &same_authority_with_different_preferences
        ));
        assert_eq!(
            host_authority_digest(&first),
            host_authority_digest(&same_authority_with_different_preferences)
        );
        let changed_port = HostConfig {
            iroh: HostIrohConfig {
                ssh_port: first.iroh.ssh_port + 1,
                ..first.iroh.clone()
            },
            ..first.clone()
        };
        assert!(!same_host_authority(&first, &changed_port));
        assert_ne!(
            host_authority_digest(&first),
            host_authority_digest(&changed_port)
        );
        let (_, ssh) = parse_host_target("me@digest.example", None).unwrap();
        let changed_ssh_port = HostConfig {
            ssh: crate::config::EnvSshConfig {
                port: ssh.ssh.port + 1,
                ..ssh.ssh.clone()
            },
            ..ssh.clone()
        };
        assert!(!same_host_authority(&ssh, &changed_ssh_port));
        assert_ne!(
            host_authority_digest(&ssh),
            host_authority_digest(&changed_ssh_port)
        );
        let mut ssh_same_authority = ssh.clone();
        ssh_same_authority.image = "different-image".into();
        assert!(same_host_authority(&ssh, &ssh_same_authority));
        assert_eq!(
            host_authority_digest(&ssh),
            host_authority_digest(&ssh_same_authority)
        );
    }

    #[test]
    fn host_authority_comparison_covers_each_reach() {
        use crate::config::RemoteTransport;

        let ssh = HostConfig {
            reach: HostReach::Ssh,
            ssh: crate::config::EnvSshConfig {
                host: "me@box".into(),
                ..crate::config::EnvSshConfig::default()
            },
            ..HostConfig::default()
        };
        assert!(same_host_authority(&ssh, &ssh));
        let mut ssh_same_authority = ssh.clone();
        ssh_same_authority.image = "other-image".into();
        assert_eq!(
            host_authority_digest(&ssh),
            host_authority_digest(&ssh_same_authority)
        );
        for changed in [
            HostConfig {
                ssh: crate::config::EnvSshConfig {
                    host: "you@box".into(),
                    ..ssh.ssh.clone()
                },
                ..ssh.clone()
            },
            HostConfig {
                ssh: crate::config::EnvSshConfig {
                    port: 2200,
                    ..ssh.ssh.clone()
                },
                ..ssh.clone()
            },
            HostConfig {
                ssh: crate::config::EnvSshConfig {
                    transport: RemoteTransport::Ssh,
                    ..ssh.ssh.clone()
                },
                ..ssh.clone()
            },
            HostConfig {
                ssh: crate::config::EnvSshConfig {
                    ssh_config: "config".into(),
                    ..ssh.ssh.clone()
                },
                ..ssh.clone()
            },
            HostConfig {
                ssh: crate::config::EnvSshConfig {
                    jump_host: "jump".into(),
                    ..ssh.ssh.clone()
                },
                ..ssh.clone()
            },
            HostConfig {
                ssh: crate::config::EnvSshConfig {
                    identity: "identity".into(),
                    ..ssh.ssh.clone()
                },
                ..ssh.clone()
            },
            HostConfig {
                ssh: crate::config::EnvSshConfig {
                    extra_args: vec!["-x".into()],
                    ..ssh.ssh.clone()
                },
                ..ssh.clone()
            },
        ] {
            assert!(!same_host_authority(&ssh, &changed));
        }
        assert!(!same_host_authority(
            &ssh,
            &HostConfig {
                reach: HostReach::Local,
                ..HostConfig::default()
            }
        ));

        let iroh = HostConfig {
            reach: HostReach::Iroh,
            iroh: HostIrohConfig {
                ticket: "ticket".into(),
                user: "me".into(),
                ssh_port: 2222,
            },
            ..HostConfig::default()
        };
        assert!(same_host_authority(&iroh, &iroh));
        let mut iroh_same_authority = iroh.clone();
        iroh_same_authority.image = "other-image".into();
        assert_eq!(
            host_authority_digest(&iroh),
            host_authority_digest(&iroh_same_authority)
        );
        for changed in [
            HostConfig {
                iroh: HostIrohConfig {
                    ticket: "other".into(),
                    ..iroh.iroh.clone()
                },
                ..iroh.clone()
            },
            HostConfig {
                iroh: HostIrohConfig {
                    user: "other".into(),
                    ..iroh.iroh.clone()
                },
                ..iroh.clone()
            },
            HostConfig {
                iroh: HostIrohConfig {
                    ssh_port: 2200,
                    ..iroh.iroh.clone()
                },
                ..iroh.clone()
            },
        ] {
            assert!(!same_host_authority(&iroh, &changed));
        }

        let cloud = HostConfig {
            reach: HostReach::Cloud,
            cloud: HostCloudConfig {
                provider: "p".into(),
                api_base: "https://api.invalid".into(),
                template: "t".into(),
                api_key_env: "TOKEN".into(),
            },
            ..HostConfig::default()
        };
        assert!(same_host_authority(&cloud, &cloud));
        let mut cloud_same_authority = cloud.clone();
        cloud_same_authority.image = "other-image".into();
        assert_eq!(
            host_authority_digest(&cloud),
            host_authority_digest(&cloud_same_authority)
        );
        for changed in [
            HostConfig {
                cloud: HostCloudConfig {
                    provider: "q".into(),
                    ..cloud.cloud.clone()
                },
                ..cloud.clone()
            },
            HostConfig {
                cloud: HostCloudConfig {
                    api_base: "https://other.invalid".into(),
                    ..cloud.cloud.clone()
                },
                ..cloud.clone()
            },
            HostConfig {
                cloud: HostCloudConfig {
                    template: "other".into(),
                    ..cloud.cloud.clone()
                },
                ..cloud.clone()
            },
            HostConfig {
                cloud: HostCloudConfig {
                    api_key_env: "OTHER".into(),
                    ..cloud.cloud.clone()
                },
                ..cloud.clone()
            },
        ] {
            assert!(!same_host_authority(&cloud, &changed));
        }

        let local = HostConfig {
            reach: HostReach::Local,
            ..HostConfig::default()
        };
        assert!(same_host_authority(&local, &local));
        let mut local_same_authority = local.clone();
        local_same_authority.image = "other-image".into();
        assert_eq!(
            host_authority_digest(&local),
            host_authority_digest(&local_same_authority)
        );
        assert!(same_host_authority(
            &local,
            &HostConfig {
                image: "changed:image".into(),
                install_runtime: InstallConsent::Auto,
                ..local.clone()
            }
        ));
        assert!(same_host_authority(
            &ssh,
            &HostConfig {
                image: "changed:image".into(),
                install_runtime: InstallConsent::Auto,
                ..ssh.clone()
            }
        ));
    }

    #[test]
    fn repo_overlay_cannot_define_hosts() {
        // The repo overlay schema (RepoConfigFile) has no `host` table, so a
        // `[host.*]` smuggled into .thegn.toml can never reach Config.host:
        // resolution consults ONLY the global config's host map. An env whose
        // ref isn't in the global map falls back to inline placement — even if
        // a repo overlay wished otherwise.
        let cfg = cfg_from(
            "[env.e]\nplacement = \"ssh\"\nhost = \"evil\"\n[env.e.ssh]\nhost = \"blake@ok\"\n",
        );
        assert!(cfg.host.is_empty());
        let b = cfg
            .resolve_host_binding("e", cfg.env.get("e").unwrap())
            .unwrap();
        assert_eq!(b.id, HostId::anon_ssh("blake@ok", 22));
        assert_eq!(b.consent, InstallConsent::Never);
    }
}

#[cfg(test)]
#[path = "host_config_merge_tests.rs"]
mod merge_tests;
