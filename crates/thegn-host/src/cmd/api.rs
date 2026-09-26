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

/// The machine-readable coverage document is a local diagnostic contract,
/// separate from the control wire schema. Increment it when its JSON shape
/// changes incompatibly.
pub(crate) const COVERAGE_SCHEMA_VERSION: u32 = 1;

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
        Action::Schema => print_schema(),
        Action::Call { cap, params } => call(cfg, &cap, params.as_deref()),
    }
}

/// Print only the embedded schema; no configuration or controller is needed.
pub(crate) fn print_schema() -> Result<()> {
    outln!("{}", CONTROL_SCHEMA.trim_end());
    Ok(())
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
        ledger(Surface::Cli, &super::session::cli_control_caps()),
        ledger(Surface::Mcp, thegn_core::mcp::state::MCP_STATE_CAPS),
        ledger(Surface::Plugin, &plugin_impl),
    ]
}

/// One generated coverage snapshot shared by the API and doctor reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoverageReport {
    pub(crate) revision: String,
    pub(crate) schema_version: u32,
    pub(crate) surfaces: Vec<thegn_core::capability::SurfaceLedger>,
}

pub(crate) fn coverage_report() -> CoverageReport {
    CoverageReport {
        revision: crate::diag::build_string().unwrap_or_else(|| "unavailable".to_string()),
        schema_version: COVERAGE_SCHEMA_VERSION,
        surfaces: surface_ledgers(),
    }
}

fn surface_json(ledger: &thegn_core::capability::SurfaceLedger) -> serde_json::Value {
    serde_json::json!({
        "surface": ledger.surface.as_str(),
        "implemented": ledger.implemented,
        "stub": ledger.stub,
        "excused": ledger.excused,
        "declared": ledger.declared,
        "gaps": ledger.gaps.iter().map(|(id, why)| {
            serde_json::json!({ "capability": id, "reason": why })
        }).collect::<Vec<_>>(),
    })
}

pub(crate) fn coverage_json(report: &CoverageReport) -> serde_json::Value {
    serde_json::json!({
        "revision": &report.revision,
        "schema_version": report.schema_version,
        "surfaces": report.surfaces.iter().map(surface_json).collect::<Vec<_>>(),
    })
}

/// Render the human document from the same snapshot used by [`coverage_json`].
pub(crate) fn coverage_text(report: &CoverageReport) -> String {
    use std::fmt::Write as _;

    let mut text = String::new();
    writeln!(text, "revision: {}", report.revision).expect("writing a String cannot fail");
    writeln!(text, "schema_version: {}", report.schema_version)
        .expect("writing a String cannot fail");
    writeln!(
        text,
        "{:<8} {:>11} {:>4} {:>7} {:>8}",
        "surface", "implemented", "stub", "excused", "declared"
    )
    .expect("writing a String cannot fail");
    for ledger in &report.surfaces {
        writeln!(
            text,
            "{:<8} {:>11} {:>4} {:>7} {:>8}",
            ledger.surface.as_str(),
            ledger.implemented,
            ledger.stub,
            ledger.excused,
            ledger.declared,
        )
        .expect("writing a String cannot fail");
    }
    let mut any = false;
    for ledger in &report.surfaces {
        for (id, why) in &ledger.gaps {
            if !any {
                text.push_str("\nexcused gaps (temporary debt):\n");
                any = true;
            }
            writeln!(text, "  {:<8} {:<18} {}", ledger.surface.as_str(), id, why)
                .expect("writing a String cannot fail");
        }
    }
    if !any {
        text.push_str("\nno excused gaps — the catalog is fully covered\n");
    }
    text
}

/// The per-surface coverage ledger — what `thegn api coverage` prints.
pub(crate) fn coverage(json: bool) -> Result<()> {
    let report = coverage_report();
    if json {
        return super::emit_json(&coverage_json(&report));
    }
    outln!("{}", coverage_text(&report).trim_end());
    Ok(())
}

pub(crate) fn list(json: bool) -> Result<()> {
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
        _ => anyhow::bail!("params must be a JSON object"),
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

    #[test]
    fn cli_ledger_includes_the_event_tail_projection() {
        assert!(super::super::session::cli_control_caps().contains(&"events.subscribe"));
    }

    #[test]
    fn coverage_report_uses_the_session_registry_and_one_document_shape() {
        let report = coverage_report();
        let expected = thegn_core::capability::ledger(
            thegn_core::capability::Surface::Cli,
            &super::super::session::cli_control_caps(),
        );
        let actual = report
            .surfaces
            .iter()
            .find(|ledger| ledger.surface == thegn_core::capability::Surface::Cli)
            .expect("coverage report includes the CLI surface");
        assert_eq!(actual, &expected);

        let json = coverage_json(&report);
        assert!(
            json["revision"]
                .as_str()
                .is_some_and(|revision| !revision.is_empty())
        );
        assert_eq!(
            json["schema_version"].as_u64(),
            Some(COVERAGE_SCHEMA_VERSION as u64)
        );
        assert!(coverage_text(&report).starts_with("revision: "));
        assert!(
            coverage_text(&report)
                .contains(&format!("schema_version: {}", COVERAGE_SCHEMA_VERSION))
        );
    }

    #[test]
    fn every_advertised_plugin_host_call_has_a_generic_control_route() {
        for cap in thegn_core::plugin_api::plugin_host_call_caps() {
            let (method, _) = thegn_svc::control::routes::api_call_for(cap)
                .unwrap_or_else(|| panic!("plugin host.call {cap} has no generic control route"));
            assert_ne!(method, "WS", "streaming cap {cap} cannot be a host.call");
        }
        assert!(
            !thegn_core::plugin_api::plugin_host_call_caps().contains(&"launch.preset"),
            "CLI-first launch.preset stays excluded until its control route lands"
        );
    }
}
