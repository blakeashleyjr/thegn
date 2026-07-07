//! `sz-agent` — the lean in-sandbox dialer.
//!
//! Baked into a superzej workspace image and launched as PID 1. On boot it reads
//! two env vars injected at provision time — `SUPERZEJ_HOME_NODE` (the
//! compositor's stable iroh EndpointId, the dial target) and
//! `SUPERZEJ_SANDBOX_AUTH` (this sandbox's minted, short-lived auth token) — then
//! creates an iroh [`Endpoint`], **dials home** (the container is behind NAT with
//! no public IP, so it always dials *out*; iroh hole-punches or falls back to an
//! n0 relay), authenticates with the token, and serves shells/exec + the reverse
//! tunnel over the single connection.
//!
//! This is the "call-home" inversion of the old dumbpipe-ticket model: superzej
//! is the stable node, every sandbox dials it. The var names deliberately avoid a
//! `*_TOKEN` suffix so they clear superzej's `SUPERZEJ_*` host allowlist and the
//! credential-key drop (`_TOKEN/_KEY/_SECRET/_PASSWORD`).
//!
//! NOTE: this is the Phase-0/1 scaffold — it stands up the endpoint and resolves
//! the home node; the dial + PTY/exec serving land next (the transport is proven
//! in-process against `superzej-svc::iroh` before wiring provisioning).

// This is a standalone PID-1 boot binary, not the compositor: it legitimately
// writes boot/diagnostic lines to stderr (captured by the container init / logs)
// before any structured sink exists. The disallowed-`{e}println!` rule guards the
// compositor's owned terminal, which this process never touches.
#![allow(clippy::disallowed_macros)]

use std::str::FromStr;

use anyhow::{Context, Result};
use superzej_agent::serve;
use superzej_core::iroh_wire::{ALPN, HOME_NODE_ENV, Hello, SANDBOX_AUTH_ENV, SANDBOX_ID_ENV};

#[tokio::main]
async fn main() -> Result<()> {
    let home = std::env::var(HOME_NODE_ENV)
        .with_context(|| format!("{HOME_NODE_ENV} unset — nothing to dial home to"))?;
    let token = std::env::var(SANDBOX_AUTH_ENV)
        .with_context(|| format!("{SANDBOX_AUTH_ENV} unset — cannot authenticate to home"))?;
    // The sandbox id defaults to the token's own short prefix when unset (the
    // compositor can still match on the token alone).
    let sandbox = std::env::var(SANDBOX_ID_ENV).unwrap_or_else(|_| token.clone());

    let home_id = iroh::EndpointId::from_str(home.trim())
        .with_context(|| format!("{HOME_NODE_ENV} is not a valid iroh EndpointId"))?;

    // `presets::N0` = n0's public relays + default discovery (works out of the
    // box; relays only rendezvous, traffic stays E2E-encrypted). Discovery
    // resolves the home node's addresses from just its id.
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .context("bind iroh endpoint")?;

    eprintln!("sz-agent: node={} dialing home {}", endpoint.id(), home_id);
    serve::dial_and_serve(&endpoint, home_id, Hello { token, sandbox }).await
}
