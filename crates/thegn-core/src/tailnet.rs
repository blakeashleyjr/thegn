//! Pure tailnet host-discovery: the `tailscale status --json` parser, the
//! candidate model, filtering, credential-free promotion, and the doctor-probe
//! summary — all substrate-free and unit-tested to the core coverage gate.
//!
//! This is the INBOUND direction of thegn's tailscale story: this machine is
//! already on a tailnet, so use that membership to *find* the user's other
//! machines as remote-host candidates for the existing `[host.<name>]` /
//! `SshTarget` stack. It is unrelated to (and never touches) the per-sandbox
//! egress VPN sidecar (`config_vpn` / `thegn_svc::vpn`) or the `tailscale serve`
//! ingress share — those mint identities; this one stores nothing.
//!
//! The subprocess that runs the local `tailscale` client is the I/O seam in
//! `thegn_svc::host_discovery`; everything here operates on captured bytes.
//!
//! **Trust boundary.** A device appearing in `tailscale status` proves tailnet
//! membership, nothing more — membership is not authorization. Candidates are
//! untrusted inventory: nothing here shell-interpolates a name, promotion is
//! always explicit, and a promoted host carries NO credential (Tailscale SSH /
//! the host's sshd + the tailnet ACLs authorize at connect time). The stable
//! [`HostCandidate::node_id`] is surfaced so a UI can show it beside the
//! MagicDNS name, mitigating a control-plane MagicDNS spoof.

use serde::{Deserialize, Serialize};

use crate::host_config::{HostConfig, HostReach};
use crate::remote::SshTarget;
use crate::seam::Availability;

/// The port Tailscale SSH (and plain sshd over a tailnet) listens on. A promoted
/// candidate is always a port-22 target — there is no per-node port in a tailnet.
pub const TAILNET_SSH_PORT: u16 = 22;

/// Bounded number of peers a single `status` capture may contribute, so a
/// pathological (or hostile) control plane can't make one discovery balloon
/// unboundedly. Far above any real tailnet.
pub const MAX_PEERS: usize = 10_000;

/// Whether a candidate advertises Tailscale SSH. `Unknown` is a first-class
/// answer: the status field that signals it (`sshHostKeys` / an ssh capability)
/// has drifted across client versions, so an older client that exposes none of
/// them is reported honestly as unknown rather than guessed as "no".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshAdvert {
    /// The node advertises Tailscale SSH (tailscaled intercepts `ssh <name>`).
    Advertised,
    /// The node exposes the signal fields but advertises no Tailscale SSH — a
    /// plain sshd over the tailnet may still answer.
    NotAdvertised,
    /// The client version does not expose the advertisement; unknown.
    Unknown,
}

impl SshAdvert {
    pub fn as_str(self) -> &'static str {
        match self {
            SshAdvert::Advertised => "advertised",
            SshAdvert::NotAdvertised => "not-advertised",
            SshAdvert::Unknown => "unknown",
        }
    }
}

/// The tailscaled backend state, as reported by `status`. Only a logged-in
/// state ([`BackendState::logged_in`]) yields usable peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendState {
    /// Up and logged in.
    Running,
    /// Coming up (transiently pre-`Running`); treated as logged in.
    Starting,
    /// Logged out — needs `tailscale up` / an interactive login.
    NeedsLogin,
    /// Stopped (`tailscale down`).
    Stopped,
    /// No state / never configured.
    NoState,
    /// Anything a newer client reports that we don't model.
    Other,
}

impl BackendState {
    fn parse(s: &str) -> Self {
        match s.trim() {
            "Running" => BackendState::Running,
            "Starting" => BackendState::Starting,
            "NeedsLogin" => BackendState::NeedsLogin,
            "Stopped" => BackendState::Stopped,
            "NoState" => BackendState::NoState,
            _ => BackendState::Other,
        }
    }

    /// Whether the client is logged in to a control plane (Tailscale or a
    /// headscale `login_server`) — the precondition for enumerating peers.
    pub fn logged_in(self) -> bool {
        matches!(self, BackendState::Running | BackendState::Starting)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BackendState::Running => "running",
            BackendState::Starting => "starting",
            BackendState::NeedsLogin => "needs-login",
            BackendState::Stopped => "stopped",
            BackendState::NoState => "no-state",
            BackendState::Other => "other",
        }
    }
}

/// One discovered tailnet peer as a remote-host candidate. Untrusted inventory
/// (see the module docs): membership is not authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostCandidate {
    /// Short host name (`nuc`).
    pub name: String,
    /// MagicDNS FQDN with the trailing dot stripped (`nuc.tail1234.ts.net`).
    /// This is what a promoted host's `SshTarget` uses.
    pub fqdn: String,
    /// Reported OS (`linux`, `darwin`, `android`, …); empty when absent.
    pub os: String,
    pub online: bool,
    pub tags: Vec<String>,
    /// The stable tailnet node id (`nXXXX…`). Surfaced at promotion time so a
    /// MagicDNS-name spoof by a hostile control plane is visible.
    pub node_id: String,
    /// Whether the node advertises Tailscale SSH — honestly `Unknown` when the
    /// client version does not expose it.
    pub ssh: SshAdvert,
    /// Tailnet IPs (100.64/10 + IPv6); informational.
    pub tailnet_ips: Vec<String>,
}

impl HostCandidate {
    /// The **credential-free** ssh target a promotion produces: the MagicDNS
    /// FQDN on port 22 with NO identity file, password, token, jump host or
    /// stored secret of any kind. When the node runs Tailscale SSH, tailscaled
    /// intercepts the connection and the tailnet ACLs authorize it; a plain
    /// sshd over the tailnet rides the user's own ssh agent/config through the
    /// identical target. Either way thegn stores no credential.
    pub fn ssh_target(&self) -> SshTarget {
        SshTarget::plain(self.fqdn.clone(), TAILNET_SSH_PORT, false)
    }

    /// The `[host.<name>]`-shaped entry an explicit promotion writes: `reach =
    /// "ssh"`, the ssh sub-table pointing at the MagicDNS FQDN on port 22, and
    /// — by construction — no identity/secret and the default (`ask`) install
    /// consent. Returns `(slug, config)`; the caller may override the name.
    pub fn to_host_config(&self) -> (String, HostConfig) {
        // Slug the FQDN (unique across tailnets), matching `parse_host_target`'s
        // convention so `--promote nuc…` and `host add nuc.tail….ts.net` agree;
        // fall back to the short name only for a degenerate empty FQDN.
        let base = if self.fqdn.trim().is_empty() {
            self.name.as_str()
        } else {
            self.fqdn.as_str()
        };
        let name = crate::util::slugify(base);
        let hc = HostConfig {
            reach: HostReach::Ssh,
            ssh: crate::config::EnvSshConfig {
                host: self.fqdn.clone(),
                port: TAILNET_SSH_PORT,
                ..crate::config::EnvSshConfig::default()
            },
            ..HostConfig::default()
        };
        (name, hc)
    }

    /// Whether `sel` names this candidate (its FQDN or short name, case-folded).
    pub fn matches(&self, sel: &str) -> bool {
        let s = sel.trim();
        self.fqdn.eq_ignore_ascii_case(s) || self.name.eq_ignore_ascii_case(s)
    }
}

/// A parsed `tailscale status --json` capture — control-plane-agnostic (a
/// headscale `login_server` produces the identical schema).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TailnetStatus {
    pub backend_state: BackendState,
    /// The `tailscale` client version string (empty when absent).
    pub version: String,
    /// The tailnet name (`CurrentTailnet.Name`) — e.g. `you@github` or a
    /// headscale org — when present.
    pub tailnet_name: Option<String>,
    /// The MagicDNS suffix (`tail1234.ts.net`, or a headscale domain).
    pub magic_dns_suffix: Option<String>,
    /// The control URL, **if** this client version surfaces it in `status`
    /// (most do not — the probe reads `tailscale debug prefs` for it instead).
    pub control_url: Option<String>,
    /// This machine (never a discovery candidate; excluded from `peers`).
    pub self_node: Option<HostCandidate>,
    /// The peer devices — the host candidates. Sorted by FQDN for determinism.
    pub peers: Vec<HostCandidate>,
}

impl TailnetStatus {
    /// How many peers advertise Tailscale SSH, how many explicitly do not, and
    /// how many are unknown (old client). `(advertised, not_advertised, unknown)`.
    pub fn ssh_counts(&self) -> (usize, usize, usize) {
        let mut adv = 0;
        let mut no = 0;
        let mut unk = 0;
        for p in &self.peers {
            match p.ssh {
                SshAdvert::Advertised => adv += 1,
                SshAdvert::NotAdvertised => no += 1,
                SshAdvert::Unknown => unk += 1,
            }
        }
        (adv, no, unk)
    }
}

/// What went wrong turning `status` bytes into a [`TailnetStatus`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailnetParseError {
    /// The bytes were not valid JSON / not the expected shape.
    Json(String),
}

impl std::fmt::Display for TailnetParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TailnetParseError::Json(e) => {
                write!(f, "could not parse `tailscale status --json`: {e}")
            }
        }
    }
}

impl std::error::Error for TailnetParseError {}

// ── raw wire structs (private) ───────────────────────────────────────────────

#[derive(Deserialize)]
struct RawStatus {
    #[serde(rename = "Version")]
    version: Option<String>,
    #[serde(rename = "BackendState")]
    backend_state: Option<String>,
    #[serde(rename = "MagicDNSSuffix")]
    magic_dns_suffix: Option<String>,
    #[serde(rename = "CurrentTailnet")]
    current_tailnet: Option<RawTailnet>,
    /// Defensive: only some client versions emit a top-level `ControlURL`. The
    /// probe's authoritative source is `tailscale debug prefs` (see
    /// [`parse_prefs_control_url`]).
    #[serde(rename = "ControlURL")]
    control_url: Option<String>,
    #[serde(rename = "Self")]
    self_node: Option<RawPeer>,
    #[serde(rename = "Peer")]
    peer: Option<std::collections::BTreeMap<String, RawPeer>>,
}

#[derive(Deserialize)]
struct RawTailnet {
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "MagicDNSSuffix")]
    magic_dns_suffix: Option<String>,
    #[serde(rename = "ControlURL")]
    control_url: Option<String>,
}

#[derive(Deserialize)]
struct RawPeer {
    #[serde(rename = "ID")]
    id: Option<String>,
    #[serde(rename = "PublicKey")]
    public_key: Option<String>,
    #[serde(rename = "HostName")]
    host_name: Option<String>,
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
    #[serde(rename = "OS")]
    os: Option<String>,
    #[serde(rename = "Online")]
    online: Option<bool>,
    #[serde(rename = "Tags")]
    tags: Option<Vec<String>>,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
    /// Newer clients: the node's advertised SSH host keys (present ⇒ the field
    /// exists; non-empty ⇒ Tailscale SSH advertised).
    #[serde(rename = "sshHostKeys")]
    ssh_host_keys: Option<Vec<String>>,
    /// Older clients: a flat capability list.
    #[serde(rename = "Capabilities")]
    capabilities: Option<Vec<String>>,
    /// Newer clients: a capability map keyed by cap name.
    #[serde(rename = "CapMap")]
    cap_map: Option<std::collections::BTreeMap<String, serde_json::Value>>,
}

/// Does any capability name look like the Tailscale-SSH capability? The cap has
/// been spelled `tailscale.com/cap/ssh` and `https://tailscale.com/cap/ssh`
/// across versions, so match on the stable `cap/ssh` tail.
fn cap_is_ssh(cap: &str) -> bool {
    cap.contains("cap/ssh")
}

impl RawPeer {
    fn ssh_advert(&self) -> SshAdvert {
        // Any positive signal wins.
        let host_keys_positive = self.ssh_host_keys.as_ref().is_some_and(|k| !k.is_empty());
        let cap_map_positive = self
            .cap_map
            .as_ref()
            .is_some_and(|m| m.keys().any(|k| cap_is_ssh(k)));
        let caps_positive = self
            .capabilities
            .as_ref()
            .is_some_and(|c| c.iter().any(|cap| cap_is_ssh(cap)));
        if host_keys_positive || cap_map_positive || caps_positive {
            return SshAdvert::Advertised;
        }
        // No positive signal, but a signal field is present ⇒ genuinely off.
        if self.ssh_host_keys.is_some() || self.cap_map.is_some() || self.capabilities.is_some() {
            return SshAdvert::NotAdvertised;
        }
        // The client exposes nothing about SSH — unknown, not "no".
        SshAdvert::Unknown
    }

    fn into_candidate(self) -> HostCandidate {
        let fqdn = self
            .dns_name
            .as_deref()
            .map(|d| d.trim().trim_end_matches('.').to_string())
            .filter(|d| !d.is_empty())
            .or_else(|| {
                self.host_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let name = self
            .host_name
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| fqdn.split('.').next().unwrap_or_default().to_string());
        let ssh = self.ssh_advert();
        let node_id = self
            .id
            .filter(|s| !s.trim().is_empty())
            .or(self.public_key)
            .unwrap_or_default();
        HostCandidate {
            name,
            fqdn,
            os: self.os.unwrap_or_default(),
            online: self.online.unwrap_or(false),
            tags: self.tags.unwrap_or_default(),
            node_id,
            ssh,
            tailnet_ips: self.tailscale_ips.unwrap_or_default(),
        }
    }
}

/// Parse a `tailscale status --json` capture into a [`TailnetStatus`]. Pure:
/// operates only on the bytes. Peers are returned sorted by FQDN so the output
/// is deterministic (the wire `Peer` map is unordered).
pub fn parse_status_json(bytes: &str) -> Result<TailnetStatus, TailnetParseError> {
    let raw: RawStatus =
        serde_json::from_str(bytes).map_err(|e| TailnetParseError::Json(e.to_string()))?;
    let backend_state = raw
        .backend_state
        .as_deref()
        .map(BackendState::parse)
        .unwrap_or(BackendState::Other);
    let self_node = raw.self_node.map(RawPeer::into_candidate);
    let mut peers: Vec<HostCandidate> = raw
        .peer
        .unwrap_or_default()
        .into_values()
        .take(MAX_PEERS)
        .map(RawPeer::into_candidate)
        .collect();
    peers.sort_by(|a, b| a.fqdn.cmp(&b.fqdn).then_with(|| a.node_id.cmp(&b.node_id)));
    let tailnet = raw.current_tailnet;
    let opt = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let tailnet_name = opt(tailnet.as_ref().and_then(|t| t.name.clone()));
    let magic_dns_suffix = opt(raw
        .magic_dns_suffix
        .or_else(|| tailnet.as_ref().and_then(|t| t.magic_dns_suffix.clone())));
    let control_url = opt(raw
        .control_url
        .or_else(|| tailnet.and_then(|t| t.control_url)));
    Ok(TailnetStatus {
        backend_state,
        version: raw.version.unwrap_or_default(),
        tailnet_name,
        magic_dns_suffix,
        control_url,
        self_node,
        peers,
    })
}

/// Pull the `ControlURL` out of a `tailscale debug prefs` capture — the
/// authoritative control-plane URL (a headscale `login_server` shows up here
/// verbatim). `None` when absent/empty or the bytes don't parse.
pub fn parse_prefs_control_url(bytes: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct RawPrefs {
        #[serde(rename = "ControlURL")]
        control_url: Option<String>,
        #[serde(rename = "Config")]
        config: Option<Box<RawPrefs>>,
    }
    let prefs: RawPrefs = serde_json::from_str(bytes).ok()?;
    prefs
        .control_url
        .or_else(|| prefs.config.and_then(|c| c.control_url))
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
}

/// Filter candidates for display/promotion: drop offline peers when
/// `online_only`, and keep only those with a tag matching `tag_filter` (a glob
/// with `*`; empty ⇒ no tag filter). A candidate matches if ANY of its tags
/// matches. Never mutates its input.
pub fn filter_candidates(
    candidates: &[HostCandidate],
    online_only: bool,
    tag_filter: &str,
) -> Vec<HostCandidate> {
    let pat = tag_filter.trim();
    candidates
        .iter()
        .filter(|c| !online_only || c.online)
        .filter(|c| pat.is_empty() || c.tags.iter().any(|t| crate::grants::glob_match(pat, t)))
        .cloned()
        .collect()
}

/// The doctor-probe summary for a parsed status, derived purely so the seam's
/// `Probe` impl is a thin subprocess wrapper over this. `control_url` is the
/// authoritative value the probe fetched from `tailscale debug prefs` (falling
/// back to any URL the status itself carried).
///
/// - `Ready` — logged in and at least one peer advertises Tailscale SSH (or SSH
///   advertisement is unknown on this client version).
/// - `Degraded` — logged in and reachable, but every peer that reports its SSH
///   state advertises none: plain sshd over the tailnet is the named fallback.
/// - `Unavailable` — logged out / stopped, with the reason.
pub fn probe_summary(
    status: &TailnetStatus,
    control_url: Option<&str>,
) -> (Availability, Vec<String>) {
    if !status.backend_state.logged_in() {
        let reason = match status.backend_state {
            BackendState::NeedsLogin => "tailscale is logged out (run `tailscale up`)".to_string(),
            BackendState::Stopped => "tailscale is stopped (run `tailscale up`)".to_string(),
            other => format!(
                "tailscale is not logged in (backend state: {})",
                other.as_str()
            ),
        };
        return (Availability::Unavailable(reason), Vec::new());
    }

    let (adv, no, unk) = status.ssh_counts();
    let mut notes = Vec::new();
    if !status.version.is_empty() {
        notes.push(format!("client {}", status.version));
    }
    if let Some(name) = &status.tailnet_name {
        notes.push(format!("tailnet: {name}"));
    }
    // The control URL surfaces headscale deployments; honest when unknown.
    match control_url.map(str::trim).filter(|u| !u.is_empty()) {
        Some(url) => notes.push(format!("control URL: {url}")),
        None => notes.push(
            "control URL: unknown (not exposed by `status`; see `tailscale debug prefs`)".into(),
        ),
    }
    notes.push(format!("{} peer(s)", status.peers.len()));
    notes.push(format!(
        "Tailscale SSH: {adv} advertised, {no} plain-sshd only, {unk} unknown"
    ));

    // Degraded only when SOMETHING is known and none of it advertises SSH; if
    // every peer is unknown (old client) or advertising is present, stay Ready.
    let availability = if adv == 0 && no > 0 {
        Availability::Degraded(
            "no peer advertises Tailscale SSH — plain sshd over the tailnet still works".into(),
        )
    } else {
        Availability::Ready
    };
    (availability, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A logged-in Tailscale (SaaS control plane) capture: an SSH-advertising
    // server, an offline phone that explicitly advertises none, and an old-client
    // node that exposes no SSH signal at all. No top-level ControlURL (the common
    // case — the probe reads prefs for it).
    const TAILSCALE_STATUS: &str = r#"{
      "Version": "1.78.1-tabcdef",
      "BackendState": "Running",
      "TUN": true,
      "MagicDNSSuffix": "tail9a1b2.ts.net",
      "CurrentTailnet": {
        "Name": "blake@github",
        "MagicDNSSuffix": "tail9a1b2.ts.net",
        "MagicDNSEnabled": true
      },
      "Self": {
        "ID": "nSelf1",
        "PublicKey": "nodekey:aaa",
        "HostName": "studio",
        "DNSName": "studio.tail9a1b2.ts.net.",
        "OS": "linux",
        "Online": true,
        "TailscaleIPs": ["100.64.0.1"],
        "Tags": ["tag:workstation"]
      },
      "Peer": {
        "nodekey:bbb": {
          "ID": "nNuc1",
          "PublicKey": "nodekey:bbb",
          "HostName": "nuc",
          "DNSName": "nuc.tail9a1b2.ts.net.",
          "OS": "linux",
          "Online": true,
          "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"],
          "Tags": ["tag:server", "tag:prod"],
          "sshHostKeys": ["ssh-ed25519 AAAAC3Nz root@nuc"]
        },
        "nodekey:ccc": {
          "ID": "nPhone1",
          "PublicKey": "nodekey:ccc",
          "HostName": "pixel",
          "DNSName": "pixel.tail9a1b2.ts.net.",
          "OS": "android",
          "Online": false,
          "TailscaleIPs": ["100.64.0.3"],
          "sshHostKeys": []
        },
        "nodekey:ddd": {
          "ID": "nOld1",
          "PublicKey": "nodekey:ddd",
          "HostName": "legacy",
          "DNSName": "legacy.tail9a1b2.ts.net.",
          "OS": "linux",
          "Online": true,
          "TailscaleIPs": ["100.64.0.4"]
        }
      }
    }"#;

    // A logged-in headscale capture: same schema, a self-hosted control URL, and
    // a peer that advertises SSH via the newer CapMap rather than sshHostKeys.
    const HEADSCALE_STATUS: &str = r#"{
      "Version": "1.76.0",
      "BackendState": "Running",
      "ControlURL": "https://headscale.example.com",
      "MagicDNSSuffix": "example.com",
      "CurrentTailnet": {
        "Name": "example.com",
        "MagicDNSSuffix": "example.com",
        "MagicDNSEnabled": true
      },
      "Self": {
        "ID": "nHs1",
        "HostName": "laptop",
        "DNSName": "laptop.example.com.",
        "OS": "linux",
        "Online": true
      },
      "Peer": {
        "nodekey:hs2": {
          "ID": "nHs2",
          "HostName": "buildbox",
          "DNSName": "buildbox.example.com.",
          "OS": "linux",
          "Online": true,
          "Tags": ["tag:ci"],
          "CapMap": { "https://tailscale.com/cap/ssh": [] }
        }
      }
    }"#;

    const LOGGED_OUT_STATUS: &str = r#"{
      "Version": "1.78.1",
      "BackendState": "NeedsLogin",
      "Peer": null
    }"#;

    const STOPPED_STATUS: &str = r#"{ "BackendState": "Stopped" }"#;

    const PREFS: &str = r#"{
      "ControlURL": "https://controlplane.tailscale.com",
      "RouteAll": false,
      "WantRunning": true
    }"#;

    fn parse(s: &str) -> TailnetStatus {
        parse_status_json(s).expect("valid status json")
    }

    #[test]
    fn parses_tailscale_capture_with_mixed_ssh_signals() {
        let st = parse(TAILSCALE_STATUS);
        assert_eq!(st.backend_state, BackendState::Running);
        assert!(st.backend_state.logged_in());
        assert_eq!(st.version, "1.78.1-tabcdef");
        assert_eq!(st.tailnet_name.as_deref(), Some("blake@github"));
        assert_eq!(st.magic_dns_suffix.as_deref(), Some("tail9a1b2.ts.net"));
        assert_eq!(st.control_url, None, "no ControlURL in this capture");
        // Self is parsed but not a candidate.
        assert_eq!(st.self_node.as_ref().unwrap().name, "studio");
        // Three peers, deterministically sorted by FQDN.
        let fqdns: Vec<&str> = st.peers.iter().map(|p| p.fqdn.as_str()).collect();
        assert_eq!(
            fqdns,
            [
                "legacy.tail9a1b2.ts.net",
                "nuc.tail9a1b2.ts.net",
                "pixel.tail9a1b2.ts.net"
            ]
        );
        let nuc = st.peers.iter().find(|p| p.name == "nuc").unwrap();
        assert_eq!(nuc.fqdn, "nuc.tail9a1b2.ts.net", "trailing dot stripped");
        assert!(nuc.online);
        assert_eq!(nuc.os, "linux");
        assert_eq!(nuc.node_id, "nNuc1");
        assert_eq!(nuc.tags, ["tag:server", "tag:prod"]);
        assert_eq!(nuc.ssh, SshAdvert::Advertised, "non-empty sshHostKeys");
        assert_eq!(nuc.tailnet_ips.len(), 2);
        let pixel = st.peers.iter().find(|p| p.name == "pixel").unwrap();
        assert!(!pixel.online);
        assert_eq!(
            pixel.ssh,
            SshAdvert::NotAdvertised,
            "sshHostKeys present-but-empty"
        );
        let legacy = st.peers.iter().find(|p| p.name == "legacy").unwrap();
        assert_eq!(
            legacy.ssh,
            SshAdvert::Unknown,
            "no ssh signal fields at all"
        );
        assert_eq!(st.ssh_counts(), (1, 1, 1));
    }

    #[test]
    fn parses_headscale_capture_same_seam() {
        let st = parse(HEADSCALE_STATUS);
        assert_eq!(st.backend_state, BackendState::Running);
        assert_eq!(st.tailnet_name.as_deref(), Some("example.com"));
        // Control URL is surfaced verbatim (this is how headscale shows up).
        assert_eq!(
            st.control_url.as_deref(),
            Some("https://headscale.example.com")
        );
        assert_eq!(st.peers.len(), 1);
        let bb = &st.peers[0];
        assert_eq!(bb.fqdn, "buildbox.example.com");
        assert_eq!(bb.ssh, SshAdvert::Advertised, "CapMap ssh cap");
        assert_eq!(bb.tags, ["tag:ci"]);
    }

    #[test]
    fn logged_out_and_stopped_are_not_logged_in() {
        let out = parse(LOGGED_OUT_STATUS);
        assert_eq!(out.backend_state, BackendState::NeedsLogin);
        assert!(!out.backend_state.logged_in());
        assert!(out.peers.is_empty(), "null Peer map ⇒ no candidates");
        let stopped = parse(STOPPED_STATUS);
        assert_eq!(stopped.backend_state, BackendState::Stopped);
        assert!(!stopped.backend_state.logged_in());
    }

    #[test]
    fn unknown_backend_and_missing_fields_degrade_gracefully() {
        // An unmodeled backend state + a peer that is nothing but a hostname.
        let st =
            parse(r#"{ "BackendState": "Frobnicating", "Peer": { "k": { "HostName": "bare" } } }"#);
        assert_eq!(st.backend_state, BackendState::Other);
        assert_eq!(st.version, "");
        assert_eq!(st.tailnet_name, None);
        let p = &st.peers[0];
        assert_eq!(p.name, "bare");
        assert_eq!(p.fqdn, "bare", "no DNSName ⇒ fall back to HostName");
        assert!(!p.online);
        assert_eq!(p.node_id, "", "no ID/PublicKey ⇒ empty (never panics)");
        assert_eq!(p.ssh, SshAdvert::Unknown);
    }

    #[test]
    fn public_key_is_the_node_id_fallback() {
        let st = parse(
            r#"{ "BackendState": "Running", "Peer": { "k": {
            "PublicKey": "nodekey:zzz", "HostName": "h", "DNSName": "h.ts.net."
        } } }"#,
        );
        assert_eq!(st.peers[0].node_id, "nodekey:zzz");
    }

    #[test]
    fn old_client_capability_list_advertises_ssh() {
        let st = parse(
            r#"{ "BackendState": "Running", "Peer": { "k": {
            "HostName": "srv", "DNSName": "srv.ts.net.",
            "Capabilities": ["https://tailscale.com/cap/ssh", "funnel"]
        } } }"#,
        );
        assert_eq!(st.peers[0].ssh, SshAdvert::Advertised);
        // Present capability list without the ssh cap ⇒ explicitly not advertised.
        let st2 = parse(
            r#"{ "BackendState": "Running", "Peer": { "k": {
            "HostName": "srv", "DNSName": "srv.ts.net.", "Capabilities": ["funnel"]
        } } }"#,
        );
        assert_eq!(st2.peers[0].ssh, SshAdvert::NotAdvertised);
    }

    #[test]
    fn malformed_json_is_a_typed_error() {
        let err = parse_status_json("not json at all").unwrap_err();
        assert!(matches!(err, TailnetParseError::Json(_)));
        assert!(err.to_string().contains("tailscale status"));
        // A JSON value of the wrong shape (array, not object) also errors.
        assert!(parse_status_json("[1,2,3]").is_err());
    }

    #[test]
    fn empty_object_is_valid_but_logged_out() {
        let st = parse_status_json("{}").unwrap();
        assert_eq!(st.backend_state, BackendState::Other);
        assert!(st.peers.is_empty());
        assert!(!st.backend_state.logged_in());
    }

    #[test]
    fn filter_online_only_and_tag_glob() {
        let st = parse(TAILSCALE_STATUS);
        // Default: online only, no tag filter ⇒ drops the offline phone.
        let online = filter_candidates(&st.peers, true, "");
        let names: Vec<&str> = online.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["legacy", "nuc"]);
        // Include offline.
        assert_eq!(filter_candidates(&st.peers, false, "").len(), 3);
        // Tag glob: only nuc carries tag:server / tag:prod.
        let servers = filter_candidates(&st.peers, false, "tag:server");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "nuc");
        let prod = filter_candidates(&st.peers, false, "tag:prod-*");
        assert!(prod.is_empty(), "prod-* does not match tag:prod");
        let any_tag = filter_candidates(&st.peers, false, "tag:*");
        assert_eq!(any_tag.len(), 1, "only nuc has any tag");
        // Whitespace-only filter is treated as no filter.
        assert_eq!(filter_candidates(&st.peers, false, "   ").len(), 3);
    }

    #[test]
    fn promotion_is_credential_free() {
        let st = parse(TAILSCALE_STATUS);
        let nuc = st.peers.iter().find(|p| p.name == "nuc").unwrap();
        // The ssh target: MagicDNS FQDN, port 22, and NOTHING else.
        let t = nuc.ssh_target();
        assert_eq!(t.host, "nuc.tail9a1b2.ts.net");
        assert_eq!(t.port, TAILNET_SSH_PORT);
        assert!(!t.forward_agent);
        assert_eq!(t.identity, None, "no identity file");
        assert_eq!(t.ssh_config, None);
        assert_eq!(t.jump_host, None);
        assert!(t.extra_args.is_empty());
        // The [host.<name>]-shaped def: ssh reach, fqdn host, no secret, default
        // (ask) consent.
        let (name, hc) = nuc.to_host_config();
        assert_eq!(name, "nuc-tail9a1b2-ts-net");
        assert_eq!(hc.reach, HostReach::Ssh);
        assert_eq!(hc.ssh.host, "nuc.tail9a1b2.ts.net");
        assert_eq!(hc.ssh.port, 22);
        assert!(hc.ssh.identity.is_empty(), "no stored identity");
        assert!(hc.ssh.jump_host.is_empty());
        assert!(hc.ssh.ssh_config.is_empty());
        assert_eq!(
            hc.install_runtime,
            crate::host_config::InstallConsent::Ask,
            "consent stays at the default — promotion never flips it"
        );
        assert!(hc.image.is_empty(), "no image override minted");
    }

    #[test]
    fn candidate_matches_by_name_or_fqdn_case_insensitively() {
        let st = parse(TAILSCALE_STATUS);
        let nuc = st.peers.iter().find(|p| p.name == "nuc").unwrap();
        assert!(nuc.matches("nuc"));
        assert!(nuc.matches("NUC"));
        assert!(nuc.matches("nuc.tail9a1b2.ts.net"));
        assert!(nuc.matches("  nuc.tail9a1b2.ts.net  "));
        assert!(!nuc.matches("pixel"));
    }

    #[test]
    fn parse_prefs_extracts_control_url() {
        assert_eq!(
            parse_prefs_control_url(PREFS).as_deref(),
            Some("https://controlplane.tailscale.com")
        );
        // Headscale login server.
        assert_eq!(
            parse_prefs_control_url(r#"{"ControlURL":"https://hs.example.org "}"#).as_deref(),
            Some("https://hs.example.org")
        );
        // Nested under Config (an alternate prefs shape).
        assert_eq!(
            parse_prefs_control_url(r#"{"Config":{"ControlURL":"https://nested.example"}}"#)
                .as_deref(),
            Some("https://nested.example")
        );
        // Missing / empty / unparseable ⇒ None.
        assert_eq!(parse_prefs_control_url(r#"{"WantRunning":true}"#), None);
        assert_eq!(parse_prefs_control_url(r#"{"ControlURL":""}"#), None);
        assert_eq!(parse_prefs_control_url("nope"), None);
    }

    #[test]
    fn probe_summary_ready_when_ssh_advertised() {
        let st = parse(TAILSCALE_STATUS);
        let (avail, notes) = probe_summary(&st, Some("https://controlplane.tailscale.com"));
        assert!(avail.is_ready(), "one peer advertises SSH ⇒ Ready");
        assert!(notes.iter().any(|n| n.contains("tailnet: blake@github")));
        assert!(
            notes
                .iter()
                .any(|n| n.contains("control URL: https://controlplane.tailscale.com"))
        );
        assert!(notes.iter().any(|n| n.contains("3 peer(s)")));
        assert!(notes.iter().any(|n| n.contains("1 advertised")));
    }

    #[test]
    fn probe_summary_degraded_when_nothing_advertises_ssh() {
        // Logged in, one peer that explicitly advertises no SSH, none unknown.
        let st = parse(
            r#"{ "BackendState": "Running", "CurrentTailnet": {"Name":"t"},
            "Peer": { "k": { "HostName": "x", "DNSName": "x.ts.net.", "sshHostKeys": [] } } }"#,
        );
        let (avail, notes) = probe_summary(&st, None);
        match avail {
            Availability::Degraded(why) => assert!(why.contains("plain sshd")),
            other => panic!("{other:?}"),
        }
        // Control URL unknown is reported honestly.
        assert!(notes.iter().any(|n| n.contains("control URL: unknown")));
    }

    #[test]
    fn probe_summary_ready_when_all_unknown() {
        // Old client: everything unknown ⇒ not Degraded (we don't guess "no").
        let st = parse(
            r#"{ "BackendState": "Running",
            "Peer": { "k": { "HostName": "x", "DNSName": "x.ts.net." } } }"#,
        );
        let (avail, _) = probe_summary(&st, None);
        assert!(avail.is_ready());
    }

    #[test]
    fn probe_summary_unavailable_when_logged_out_or_stopped() {
        let (avail, notes) = probe_summary(&parse(LOGGED_OUT_STATUS), None);
        match avail {
            Availability::Unavailable(why) => assert!(why.contains("logged out")),
            other => panic!("{other:?}"),
        }
        assert!(notes.is_empty());
        let (avail, _) = probe_summary(&parse(STOPPED_STATUS), None);
        match avail {
            Availability::Unavailable(why) => assert!(why.contains("stopped")),
            other => panic!("{other:?}"),
        }
        // An unmodeled non-logged-in state names itself.
        let (avail, _) = probe_summary(&parse(r#"{"BackendState":"NoState"}"#), None);
        assert!(avail.is_unavailable());
    }

    #[test]
    fn ssh_advert_and_backend_state_as_str() {
        assert_eq!(SshAdvert::Advertised.as_str(), "advertised");
        assert_eq!(SshAdvert::NotAdvertised.as_str(), "not-advertised");
        assert_eq!(SshAdvert::Unknown.as_str(), "unknown");
        assert_eq!(BackendState::Running.as_str(), "running");
        assert_eq!(BackendState::Starting.as_str(), "starting");
        assert!(BackendState::Starting.logged_in());
        assert_eq!(BackendState::NeedsLogin.as_str(), "needs-login");
        assert_eq!(BackendState::NoState.as_str(), "no-state");
        assert_eq!(BackendState::Other.as_str(), "other");
    }

    #[test]
    fn peer_count_is_bounded() {
        // Build a status with more than MAX_PEERS entries; parsing caps it.
        let mut peers = String::new();
        for i in 0..(MAX_PEERS + 50) {
            if i > 0 {
                peers.push(',');
            }
            peers.push_str(&format!(
                r#""k{i}":{{"HostName":"h{i}","DNSName":"h{i}.ts.net."}}"#
            ));
        }
        let json = format!(r#"{{"BackendState":"Running","Peer":{{{peers}}}}}"#);
        let st = parse_status_json(&json).unwrap();
        assert_eq!(st.peers.len(), MAX_PEERS);
    }
}
