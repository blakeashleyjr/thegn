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
    /// Per-surface coverage ledger (implemented / stub / excused / declared).
    Coverage {
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
        Action::Coverage { json } => coverage(json),
        Action::Schema => {
            outln!("{}", CONTROL_SCHEMA.trim_end());
            Ok(())
        }
        Action::Call { cap, params } => call(cfg, &cap, params.as_deref()),
    }
}

/// Every surface's implemented capability-id table, gathered from the
/// authoritative source each door projects — the same tables the per-surface
/// coverage tests assert. Local introspection; no daemon needed.
pub(crate) fn surface_ledgers() -> Vec<thegn_core::capability::SurfaceLedger> {
    use thegn_core::capability::{Surface, ledger};
    let plugin = thegn_core::plugin_api::plugin_host_call_caps();
    // The plugin event feed is a stream, delivered by the resident-plugin
    // subscribe bridge rather than host.call — implemented, just not a call.
    let mut plugin_impl = plugin.clone();
    if thegn_core::capability::lookup("events.subscribe")
        .is_some_and(|c| c.surfaces.contains(Surface::Plugin))
    {
        plugin_impl.push("events.subscribe");
    }
    vec![
        ledger(
            Surface::Http,
            &thegn_svc::control::routes::implemented_caps(),
        ),
        ledger(Surface::Grpc, thegn_svc::control::grpc::GRPC_CAPS),
        ledger(Surface::Cli, &cli_control_caps()),
        ledger(Surface::Mcp, thegn_core::mcp::state::MCP_STATE_CAPS),
        ledger(Surface::Plugin, &plugin_impl),
    ]
}

/// Every capability id the `thegn` CLI drives through the control API — the
/// non-streaming rows of the `API_CALLS` route table (`thegn api call` reaches
/// them generically) plus `sessions.attach` (the dedicated `thegn attach`
/// verb). Mirrors `cmd::session::cli_control_caps` (that copy is test-only).
fn cli_control_caps() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = thegn_svc::control::routes::API_CALLS
        .iter()
        .filter(|(_, method, _)| *method != "WS")
        .map(|(cap, _, _)| *cap)
        .collect();
    v.push("sessions.attach");
    v.sort_unstable();
    v.dedup();
    v
}

/// The per-surface coverage ledger — what `thegn api coverage` prints.
fn coverage(json: bool) -> Result<()> {
    let ledgers = surface_ledgers();
    if json {
        let rows: Vec<serde_json::Value> = ledgers
            .iter()
            .map(|l| {
                serde_json::json!({
                    "surface": l.surface.as_str(),
                    "implemented": l.implemented,
                    "stub": l.stub,
                    "excused": l.excused,
                    "declared": l.declared,
                    "gaps": l.gaps.iter().map(|(id, why)| {
                        serde_json::json!({ "capability": id, "reason": why })
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        return super::emit_json(&serde_json::json!({ "surfaces": rows }));
    }
    outln!(
        "{:<8} {:>11} {:>4} {:>7} {:>8}",
        "surface",
        "implemented",
        "stub",
        "excused",
        "declared"
    );
    for l in &ledgers {
        outln!(
            "{:<8} {:>11} {:>4} {:>7} {:>8}",
            l.surface.as_str(),
            l.implemented,
            l.stub,
            l.excused,
            l.declared
        );
    }
    // The excused (capability, surface) cells, so the debt is legible.
    let mut any = false;
    for l in &ledgers {
        for (id, why) in &l.gaps {
            if !any {
                outln!("\nexcused gaps (temporary debt):");
                any = true;
            }
            outln!("  {:<8} {:<18} {}", l.surface.as_str(), id, why);
        }
    }
    if !any {
        outln!("\nno excused gaps — the catalog is fully covered");
    }
    Ok(())
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
                    "stub": c.stub,
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
        // Mark routed-but-inert stubs so a reader never mistakes one for a
        // working capability.
        let summary = match c.stub {
            Some(_) => format!("[stub] {}", c.summary),
            None => c.summary.to_string(),
        };
        outln!(
            "{:<20} {:<6} {:<28} {}",
            c.id.as_str(),
            format!("{:?}", scope_of(c)).to_lowercase(),
            surfaces.join(","),
            summary
        );
    }
    Ok(())
}

/// Resolve a generic capability call into `(method, path_with_query, body)`
/// over the control socket, from the shared route table. Path placeholders are
/// filled from `params` (and removed from the body); on `GET`/`DELETE` the
/// remaining params ride the query string, otherwise the JSON body.
///
/// A thin adapter over [`thegn_svc::control::routes::build_call`] — the ONE
/// catalog-id → route spine (also used by `thegn api call` and the push command
/// inbox) — so the plugin `host.call` dispatcher reaches a newly routed verb
/// with no per-verb code and cannot drift from the other doors.
pub(crate) fn resolve_call(
    cap: &str,
    params: serde_json::Value,
) -> Result<(&'static str, String, Option<serde_json::Value>)> {
    let params: serde_json::Map<String, serde_json::Value> = match params {
        serde_json::Value::Null => Default::default(),
        serde_json::Value::Object(m) => m,
        _ => bail!("params must be a JSON object"),
    };
    thegn_svc::control::routes::build_call(cap, params).map_err(|e| anyhow::anyhow!(e))
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
