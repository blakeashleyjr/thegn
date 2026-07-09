//! `szhost pair …` — pairing-credential management for thin clients. Pure DB
//! operations (the `pairings` table): they work with or without a running
//! daemon, so issuing/revoking access is never blocked on one.

use anyhow::Result;
use superzej_core::config::Config;
use superzej_core::control::{PairingUrl, ScopeSet, TokenKind};
use superzej_core::db::Db;
use superzej_core::outln;
use superzej_core::store::ControlStore;
use superzej_svc::control::auth;

#[derive(clap::Subcommand, Clone)]
pub enum PairAction {
    /// Mint a single-use pairing code and print its pairing URL. The code is
    /// shown ONCE (only its hash is stored).
    New {
        /// Scopes the redeemed token will hold (csv of read,write,git,admin).
        #[arg(long, default_value = "read")]
        scope: String,
        /// Human label shown in `pair list` and approval prompts.
        #[arg(long, default_value = "")]
        label: String,
        /// Code lifetime in minutes.
        #[arg(long, default_value_t = 15)]
        ttl_mins: i64,
        /// Host to embed in the printed pairing URL (defaults to this
        /// machine's hostname — override with the address clients reach).
        #[arg(long)]
        host: Option<String>,
        /// Port to embed in the printed pairing URL (`[serve] bind`'s port
        /// by default).
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
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

pub fn run(cfg: &Config, action: PairAction) -> Result<()> {
    let db = Db::open()?;
    match action {
        PairAction::New {
            scope,
            label,
            ttl_mins,
            host,
            port,
        } => {
            let scopes = ScopeSet::parse(&scope);
            if scopes.is_empty() {
                anyhow::bail!("no valid scopes in {scope:?} (use csv of read,write,git,admin)");
            }
            let now = now_ms();
            let minted = auth::mint(
                TokenKind::PairingCode,
                scopes,
                &label,
                None,
                Some(now + ttl_mins.max(1) * 60_000),
                now,
            );
            db.put_pairing(&minted.row)?;
            let port = port.unwrap_or_else(|| {
                cfg.serve
                    .bind
                    .rsplit(':')
                    .next()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(5380)
            });
            let url = PairingUrl {
                host: host.unwrap_or_else(hostname),
                port,
                code: minted.token,
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
            db.revoke_pairing(&id, now_ms())?;
            outln!("revoked {id}");
        }
        PairAction::Approve { id } => {
            db.approve_pairing(&id, now_ms())?;
            outln!("approved {id}");
        }
    }
    Ok(())
}
