//! `thegn pair …` — pairing-credential management for thin clients. Pure DB
//! operations (the `pairings` table): they work with or without a running
//! daemon, so issuing/revoking access is never blocked on one.

use anyhow::{Context, Result};
#[cfg(feature = "iroh")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "iroh")]
use std::io::Read as _;
use thegn_core::config::Config;
use thegn_core::control::{PairingUrl, ScopeSet, TokenKind};
use thegn_core::db::Db;
use thegn_core::outln;
use thegn_core::store::ControlStore;
use thegn_svc::control::auth;

#[derive(clap::Subcommand, Clone)]
pub enum PairAction {
    /// Mint a single-use pairing code and print its pairing URL. The code is
    /// shown ONCE (only its hash is stored).
    New {
        /// Scopes the redeemed token will hold (csv of read,write,git,merge_add,exec,admin).
        #[arg(long, default_value = "read")]
        scope: String,
        /// Human label shown in `pair list` and approval prompts.
        #[arg(long, default_value = "")]
        label: String,
        /// Code lifetime in minutes.
        #[arg(long, default_value_t = 15)]
        ttl_mins: i64,
        /// Host to embed in the printed pairing URL (defaults to
        /// `[serve] advertise_host` or the concrete bind IP). Safe direct mode
        /// accepts only a loopback IP or localhost name.
        #[arg(long)]
        host: Option<String>,
        /// Port to embed in the printed pairing URL (the resolved public port
        /// by default: 443 for TLS termination, otherwise the backend port).
        #[arg(long)]
        port: Option<u16>,
    },
    /// List pairings (codes and tokens) with their state.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Revoke a pairing by id — live streams for it drop on the next event.
    Revoke { id: String },
    /// Approve a parked token (the `[serve] require_approval` flow).
    Approve { id: String },
    /// Direct-only Iroh control pairing and the reconnectable scoped profile.
    #[cfg(feature = "iroh")]
    Direct {
        #[command(subcommand)]
        action: DirectAction,
    },
}

#[cfg(feature = "iroh")]
#[derive(clap::Subcommand, Clone)]
pub enum DirectAction {
    /// Mint a scoped one-time code and save a protected direct invitation file.
    New {
        /// The stable direct control peer ID printed by `thegn serve`.
        #[arg(long)]
        endpoint_id: String,
        /// Concrete IP socket address printed by `thegn serve` (repeatable).
        #[arg(long = "address", required = true)]
        addresses: Vec<String>,
        /// Scopes inherited by the redeemed control token.
        #[arg(long, default_value = "read")]
        scope: String,
        /// Human label shown in `pair list` and approval prompts.
        #[arg(long, default_value = "")]
        label: String,
        /// Code lifetime in minutes.
        #[arg(long, default_value_t = 15)]
        ttl_mins: i64,
        /// New owner-only invitation file; must not already exist.
        #[arg(long)]
        out: std::path::PathBuf,
    },
    /// Redeem an invitation from a file or stdin and save a reconnectable profile.
    Redeem {
        /// Local name for the authenticated peer profile.
        name: String,
        /// JSON invitation file; stdin is read when omitted.
        #[arg(long)]
        invitation_file: Option<std::path::PathBuf>,
    },
    /// Make an authenticated read-scoped control request to the saved peer.
    Me { name: String },
}

#[cfg(feature = "iroh")]
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DirectInvitation {
    endpoint_id: String,
    addresses: Vec<String>,
    code: String,
    #[serde(default)]
    label: String,
}

#[cfg(feature = "iroh")]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectProfile {
    endpoint: iroh::EndpointAddr,
    token_ref: String,
    pairing_id: String,
    scopes: String,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| thegn_core::time_policy::saturating_i64(d.as_millis()))
        .unwrap_or(0)
}

pub fn run(cfg: &Config, action: PairAction) -> Result<()> {
    #[cfg(feature = "iroh")]
    let action = match action {
        PairAction::Direct { action } => return run_direct(action),
        action => action,
    };
    let db = Db::open()?;
    match action {
        PairAction::New {
            scope,
            label,
            ttl_mins,
            host,
            port,
        } => {
            // Treat URL overrides as inputs to the same validated policy as
            // startup. In particular, `--host` must not bypass host-only URL
            // validation and accidentally place a path/query beside a secret.
            let mut effective_serve = cfg.serve.clone();
            if let Some(host) = host {
                effective_serve.advertise_host = host;
            }
            if let Some(port) = port {
                effective_serve.advertise_port = port;
            }
            let transport = effective_serve
                .resolve_transport(None, false)
                .map_err(anyhow::Error::msg)?;
            let scopes = ScopeSet::parse(&scope);
            if scopes.is_empty() {
                anyhow::bail!(
                    "no valid scopes in {scope:?} (use csv of read,write,git,merge_add,exec,admin)"
                );
            }
            let now = now_ms();
            let delay_ms = u64::try_from(ttl_mins.max(1))
                .ok()
                .and_then(|minutes| minutes.checked_mul(60_000));
            let expires = delay_ms
                .and_then(|delay| thegn_core::time_policy::deadline_millis(now, delay))
                .context("pairing lifetime exceeds the supported duration or epoch range")?;
            let minted = auth::mint(
                TokenKind::PairingCode,
                scopes,
                &label,
                None,
                Some(expires),
                now,
            );
            db.put_pairing(&minted.row)?;
            let url = PairingUrl {
                host: transport.advertise_host.clone(),
                port: transport.advertise_port(transport.bind.port()),
                code: minted.token,
                secure: transport.http_scheme() == "https",
                fp: None,
            };
            outln!("pairing id : {}", minted.row.pairing_id);
            outln!("scopes     : {}", minted.row.scope);
            outln!("expires    : in {} min", ttl_mins.max(1));
            outln!("url        : {}", url.encode());
            outln!("web        : {}", url.web_form());
            outln!("(single-use; only its hash is stored — this is the last time it is shown)");
        }
        PairAction::List { json } => {
            let rows = db.pairings()?;
            if json {
                let out: Vec<_> = rows
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "pairing_id": p.pairing_id,
                            "kind": p.kind,
                            "scopes": p.scope,
                            "label": p.label,
                            "parent_id": p.parent_id,
                            "created_at": p.created_at,
                            "expires_at": p.expires_at,
                            "redeemed_at": p.redeemed_at,
                            "approved_at": p.approved_at,
                            "revoked_at": p.revoked_at,
                        })
                    })
                    .collect();
                outln!("{}", serde_json::to_string_pretty(&out)?);
            } else if rows.is_empty() {
                outln!("no pairings");
            } else {
                let now = now_ms();
                for p in rows {
                    let state = if p.revoked_at.is_some() {
                        "revoked"
                    } else if p.kind == "code" && p.redeemed_at.is_some() {
                        "redeemed"
                    } else if p.expires_at.is_some_and(|e| e <= now) {
                        "expired"
                    } else if p.kind == "token" && p.approved_at.is_none() {
                        "pending approval"
                    } else {
                        "live"
                    };
                    outln!(
                        "{}  {:5}  [{}]  {}  {}",
                        p.pairing_id,
                        p.kind,
                        p.scope,
                        state,
                        p.label
                    );
                }
            }
        }
        PairAction::Revoke { id } => {
            // Look up first so a nonexistent / already-revoked id fails loudly
            // instead of claiming success: the exit code is what scripts/CI
            // branch on, and the DB UPDATE is a no-op (0 rows) in those cases.
            let row = db
                .pairings()?
                .into_iter()
                .find(|p| p.pairing_id == id)
                .ok_or_else(|| anyhow::anyhow!("no pairing with id {id}"))?;
            if row.revoked_at.is_some() {
                anyhow::bail!("pairing {id} is already revoked");
            }
            db.revoke_pairing(&id, now_ms())?;
            outln!("revoked {id}");
        }
        PairAction::Approve { id } => {
            // Same reasoning as Revoke: don't report success for a missing id
            // or one already approved/revoked (the UPDATE matches 0 rows) —
            // the non-zero exit is what scripting/CI relies on.
            let row = db
                .pairings()?
                .into_iter()
                .find(|p| p.pairing_id == id)
                .ok_or_else(|| anyhow::anyhow!("no pairing with id {id}"))?;
            if row.revoked_at.is_some() {
                anyhow::bail!("pairing {id} is revoked; cannot approve");
            }
            if row.approved_at.is_some() {
                anyhow::bail!("pairing {id} is already approved");
            }
            db.approve_pairing(&id, now_ms())?;
            outln!("approved {id}");
        }
        #[cfg(feature = "iroh")]
        PairAction::Direct { .. } => unreachable!("direct pairing returned above"),
    }
    Ok(())
}

#[cfg(feature = "iroh")]
fn profile_name(name: &str) -> Result<&str> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "profile name must be 1–64 ASCII letters, digits, '-' or '_'"
    );
    Ok(name)
}

#[cfg(feature = "iroh")]
fn profile_dir() -> Result<thegn_core::fsperm::PrivateDir> {
    let path = thegn_core::util::xdg_state_home().join("thegn/direct-peers");
    thegn_core::fsperm::ensure_private_dir(&path)
        .context("prepare private direct-peer profile directory")?;
    thegn_core::fsperm::open_private_dir(&path)
        .context("open private direct-peer profile directory")
}

#[cfg(feature = "iroh")]
fn read_invitation(file: Option<std::path::PathBuf>) -> Result<DirectInvitation> {
    match file {
        Some(path) => {
            let text = thegn_core::fsperm::read_owner_only_secret_file(&path, 16 * 1024)
                .map_err(|error| anyhow::anyhow!("invitation file refused: {error:?}"))?;
            parse_invitation(text.as_bytes())
        }
        None => read_invitation_from(std::io::stdin()),
    }
}

#[cfg(feature = "iroh")]
fn read_invitation_from(reader: impl std::io::Read) -> Result<DirectInvitation> {
    let mut bytes = Vec::new();
    reader.take(16 * 1024).read_to_end(&mut bytes)?;
    parse_invitation(&bytes)
}

#[cfg(feature = "iroh")]
fn parse_invitation(bytes: &[u8]) -> Result<DirectInvitation> {
    anyhow::ensure!(bytes.len() < 16 * 1024, "direct invitation exceeds 16 KiB");
    serde_json::from_slice(bytes).context("parse direct Iroh invitation JSON")
}

#[cfg(feature = "iroh")]
fn run_direct(action: DirectAction) -> Result<()> {
    use std::str::FromStr as _;
    use thegn_svc::control::client::{ControlAddr, ControlClient};

    match action {
        DirectAction::New {
            endpoint_id,
            addresses,
            scope,
            label,
            ttl_mins,
            out,
        } => {
            use std::str::FromStr as _;
            let id = iroh::EndpointId::from_str(&endpoint_id)
                .context("invalid direct control endpoint ID")?;
            anyhow::ensure!(!addresses.is_empty(), "at least one --address is required");
            let mut addrs = Vec::with_capacity(addresses.len());
            for address in addresses {
                let socket = std::net::SocketAddr::from_str(&address)
                    .with_context(|| format!("invalid direct socket address {address:?}"))?;
                anyhow::ensure!(
                    !socket.ip().is_unspecified() && socket.port() != 0,
                    "direct invitation addresses need a concrete IP and port"
                );
                addrs.push(iroh::TransportAddr::Ip(socket));
            }
            let scopes = ScopeSet::parse(&scope);
            anyhow::ensure!(!scopes.is_empty(), "no valid scopes in {scope:?}");
            let now = now_ms();
            let delay_ms = u64::try_from(ttl_mins.max(1))
                .ok()
                .and_then(|minutes| minutes.checked_mul(60_000));
            let expires = delay_ms
                .and_then(|delay| thegn_core::time_policy::deadline_millis(now, delay))
                .context("pairing lifetime exceeds the supported duration or epoch range")?;
            let minted = auth::mint(
                TokenKind::PairingCode,
                scopes,
                &label,
                None,
                Some(expires),
                now,
            );
            Db::open()?.put_pairing(&minted.row)?;
            let invitation = DirectInvitation {
                endpoint_id: id.to_string(),
                addresses: addrs
                    .into_iter()
                    .filter_map(|addr| match addr {
                        iroh::TransportAddr::Ip(socket) => Some(socket.to_string()),
                        _ => None,
                    })
                    .collect(),
                code: minted.token,
                label,
            };
            thegn_core::fsperm::write_owner_only_new(&out, &serde_json::to_vec(&invitation)?)
                .with_context(|| {
                    format!("create owner-only direct invitation {}", out.display())
                })?;
            outln!(
                "created direct invitation {} · peer {} · scopes [{}] · expires in {} min",
                out.display(),
                id,
                minted.row.scope,
                ttl_mins.max(1)
            );
        }
        DirectAction::Redeem {
            name,
            invitation_file,
        } => {
            let name = profile_name(&name)?;
            let invite = read_invitation(invitation_file)?;
            let id = iroh::EndpointId::from_str(&invite.endpoint_id)
                .context("invalid invitation endpoint ID")?;
            anyhow::ensure!(
                !invite.addresses.is_empty(),
                "invitation has no direct IP addresses"
            );
            let mut addrs = Vec::with_capacity(invite.addresses.len());
            for address in invite.addresses {
                let socket = std::net::SocketAddr::from_str(&address)
                    .with_context(|| format!("invalid direct socket address {address:?}"))?;
                anyhow::ensure!(
                    !socket.ip().is_unspecified() && socket.port() != 0,
                    "invitation address must have a concrete IP and port"
                );
                addrs.push(iroh::TransportAddr::Ip(socket));
            }
            let endpoint = iroh::EndpointAddr::from_parts(id, addrs);
            let client = ControlClient::new(ControlAddr::Iroh {
                endpoint: endpoint.clone(),
                token: String::new(),
            });
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(client.pair(&invite.code, &invite.label))?;
            let token = result
                .get("token")
                .and_then(serde_json::Value::as_str)
                .context("pair response omitted token")?;
            let pairing_id = result
                .get("pairing_id")
                .and_then(serde_json::Value::as_str)
                .context("pair response omitted pairing id")?;
            let scopes = result
                .get("scopes")
                .and_then(serde_json::Value::as_str)
                .context("pair response omitted scopes")?;
            let secret_name = format!("thegn-iroh-{pairing_id}");
            let token_ref = crate::secret::store(&secret_name, token)
                .context("store direct peer token in the OS keyring or owner-only secret file")?;
            let profile = DirectProfile {
                endpoint,
                token_ref,
                pairing_id: pairing_id.to_owned(),
                scopes: scopes.to_owned(),
            };
            let dir = profile_dir()?;
            dir.write_owner_only_atomic(&format!("{name}.json"), &serde_json::to_vec(&profile)?)?;
            outln!(
                "paired {name} · peer {} · scopes [{}]",
                profile.endpoint.id,
                profile.scopes
            );
        }
        DirectAction::Me { name } => {
            let name = profile_name(&name)?;
            let dir = profile_dir()?;
            let bytes = dir
                .read_regular(&format!("{name}.json"), 16 * 1024)?
                .with_context(|| format!("no direct peer profile named {name}"))?;
            let profile: DirectProfile =
                serde_json::from_slice(&bytes).context("parse saved direct peer profile")?;
            anyhow::ensure!(
                profile.token_ref.starts_with("keyring:") || profile.token_ref.starts_with("file:"),
                "saved direct peer token reference must use the keyring or protected file store"
            );
            let token =
                crate::secret::resolve(&profile.token_ref).context("resolve direct peer token")?;
            let client = ControlClient::new(ControlAddr::Iroh {
                endpoint: profile.endpoint.clone(),
                token,
            });
            let me = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(client.me())?;
            outln!(
                "peer {} · pairing {} · scopes [{}]",
                profile.endpoint.id,
                profile.pairing_id,
                profile.scopes
            );
            outln!("{}", serde_json::to_string(&me)?);
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "iroh"))]
mod direct_tests {
    use super::*;

    const INVITATION: &str = r#"{"endpoint_id":"peer-id","addresses":["192.0.2.7:4567"],"code":"tgc1_single-use-code","label":"laptop"}"#;

    #[test]
    fn direct_redeem_reads_json_from_stdin_without_putting_code_in_argv() {
        let invite = read_invitation_from(std::io::Cursor::new(INVITATION)).unwrap();
        assert_eq!(invite.endpoint_id, "peer-id");
        assert_eq!(invite.addresses, ["192.0.2.7:4567"]);
        assert_eq!(invite.code, "tgc1_single-use-code");
        assert_eq!(invite.label, "laptop");
    }

    #[test]
    fn direct_redeem_accepts_owner_only_file_and_refuses_world_readable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invite.json");
        thegn_core::fsperm::write_owner_only_new(&path, INVITATION.as_bytes()).unwrap();
        assert_eq!(
            read_invitation(Some(path.clone())).unwrap().code,
            "tgc1_single-use-code"
        );

        if crate::platform::make_world_readable_for_test(&path).unwrap() {
            let error = read_invitation(Some(path)).err().unwrap().to_string();
            assert!(error.contains("invitation file refused"));
            assert!(!error.contains("tgc1_single-use-code"));
        }
    }

    #[test]
    fn direct_redeem_bounds_and_rejects_unknown_invitation_fields() {
        let oversized = vec![b' '; 16 * 1024];
        assert!(parse_invitation(&oversized).is_err());
        assert!(
            parse_invitation(
                br#"{"endpoint_id":"peer","addresses":[],"code":"x","unexpected":true}"#
            )
            .is_err()
        );
    }
}
