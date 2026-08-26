//! `thegn api` — the capability catalog as a generic client.
//!
//! - `list` — every catalog row: id, required scope, surfaces, summary. What
//!   you see is what every door (HTTP, gRPC, CLI, MCP, plugin `host.call`)
//!   projects, because they all read `thegn_core::capability::CATALOG`.
//! - `schema` — the committed control wire schema (`docs/api/control-v1.json`,
//!   pinned by `thegn-svc`'s snapshot test): types + `(cap, method, path)`
//!   routes.
//! - `call <cap> [--params '<json>']` — resolve the capability's HTTP route
//!   from the `API_CALLS` table and perform it over the control socket, JSON
//!   in/out. No per-verb client code: a newly routed verb is callable the
//!   moment its route lands (the route-coverage tests force that moment).
//!
//! `{placeholders}` in the path template are filled from params (and removed
//! from the body); remaining params ride the query string on `GET`/`DELETE`
//! and the JSON body on `POST`. Streaming caps (`WS`) are not callable here —
//! use `thegn attach` / the events endpoints.

use anyhow::{Context, Result};
use clap::Subcommand;
use thegn_core::config::Config;
use thegn_core::outln;
use thegn_svc::control::routes::api_call_for;

/// The committed wire schema — embedded so `schema` needs no checkout.
const CONTROL_SCHEMA: &str = include_str!("../../../../docs/api/control-v1.json");

#[derive(Subcommand, Clone)]
pub enum Action {
    /// List the capability catalog (id, scope, surfaces, summary).
    List {
        /// Emit machine-readable JSON instead of the text table.
        #[arg(long)]
        json: bool,
    },
    /// Print the control wire schema (docs/api/control-v1.json).
    Schema,
    /// Call a capability by catalog id over the control socket.
    Call {
        /// The capability id, e.g. `worktrees.list`, `notify.push`.
        cap: String,
        /// JSON object of parameters (path placeholders + body/query).
        #[arg(long)]
        params: Option<String>,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List { json } => list(json),
        Action::Schema => {
            outln!("{}", CONTROL_SCHEMA.trim_end());
            Ok(())
        }
        Action::Call { cap, params } => call(cfg, &cap, params.as_deref()),
    }
}

fn list(json: bool) -> Result<()> {
    use thegn_core::capability::{CATALOG, Surface, scope_of};
    if json {
        let rows: Vec<serde_json::Value> = CATALOG
            .iter()
            .map(|c| {
                let surfaces: Vec<&str> = Surface::ALL
                    .iter()
                    .filter(|s| c.surfaces.contains(**s))
                    .map(|s| s.as_str())
                    .collect();
                serde_json::json!({
                    "id": c.id.as_str(),
                    "scope": format!("{:?}", scope_of(c)).to_lowercase(),
                    "surfaces": surfaces,
                    "summary": c.summary,
                    "since": c.since,
                    "deprecated": c.deprecated,
                    "callable": api_call_for(c.id.as_str())
                        .map(|(m, p)| serde_json::json!({"method": m, "path": p})),
                })
            })
            .collect();
        return super::emit_json(&rows);
    }
    for c in CATALOG {
        let surfaces: Vec<&str> = Surface::ALL
            .iter()
            .filter(|s| c.surfaces.contains(**s))
            .map(|s| s.as_str())
            .collect();
        outln!(
            "{:<20} {:<6} {:<28} {}",
            c.id.as_str(),
            format!("{:?}", scope_of(c)).to_lowercase(),
            surfaces.join(","),
            c.summary
        );
    }
    Ok(())
}

fn call(cfg: &Config, cap: &str, params: Option<&str>) -> Result<()> {
    let params: serde_json::Map<String, serde_json::Value> = match params {
        None => Default::default(),
        Some(p) => serde_json::from_str::<serde_json::Value>(p)
            .context("--params must be a JSON object")?
            .as_object()
            .cloned()
            .context("--params must be a JSON object")?,
    };
    // The catalog id → (method, path, body) mapping is shared with the push
    // command inbox — one dispatch spine, never two.
    let (method, path, body) =
        thegn_svc::control::routes::build_call(cap, params).map_err(|e| anyhow::anyhow!(e))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let out = rt.block_on(async {
        let client = super::session::connect(cfg).await?;
        client.call_raw(method, &path, body).await
    })?;
    super::emit_json(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_routed_cap_is_callable_or_streaming() {
        // The catalog + route tables agree: `call` can resolve every cap the
        // HTTP surface implements (WS rows excepted by the streaming error).
        for c in thegn_core::capability::CATALOG {
            if let Some((method, path)) = api_call_for(c.id.as_str()) {
                assert!(path.starts_with("/v1/"), "{}", c.id.as_str());
                assert!(matches!(method, "GET" | "POST" | "DELETE" | "WS"));
            }
        }
    }
}
