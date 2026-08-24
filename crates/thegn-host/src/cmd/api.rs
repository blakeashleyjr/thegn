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

use anyhow::{Context, Result, bail};
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

/// Fill `{placeholders}` from `params`, removing the used keys. Errors on a
/// placeholder with no matching param.
fn fill_path(
    template: &str,
    params: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let close = rest[open..]
            .find('}')
            .map(|i| open + i)
            .context("unbalanced path template")?;
        out.push_str(&rest[..open]);
        let key = &rest[open + 1..close];
        let val = params
            .remove(key)
            .with_context(|| format!("missing path parameter {key:?} (pass it in --params)"))?;
        match val {
            serde_json::Value::String(s) => out.push_str(&s),
            other => out.push_str(other.to_string().trim_matches('"')),
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn call(cfg: &Config, cap: &str, params: Option<&str>) -> Result<()> {
    use thegn_core::capability::CATALOG;
    if !CATALOG.iter().any(|c| c.id.as_str() == cap) {
        bail!("unknown capability {cap} — see `thegn api list`");
    }
    let Some((method, template)) = api_call_for(cap) else {
        bail!("{cap} has no HTTP route yet — see SURFACE_GAPS in the catalog");
    };
    if method == "WS" {
        bail!("{cap} is a streaming capability — use `thegn attach` / the events endpoints");
    }
    let mut params: serde_json::Map<String, serde_json::Value> = match params {
        None => Default::default(),
        Some(p) => serde_json::from_str::<serde_json::Value>(p)
            .context("--params must be a JSON object")?
            .as_object()
            .cloned()
            .context("--params must be a JSON object")?,
    };
    let mut path = fill_path(template, &mut params)?;
    let body = if method == "GET" || method == "DELETE" {
        if !params.is_empty() {
            let qs: Vec<String> = params
                .iter()
                .map(|(k, v)| {
                    let v = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    format!("{k}={v}")
                })
                .collect();
            path = format!("{path}?{}", qs.join("&"));
        }
        None
    } else {
        Some(serde_json::Value::Object(params))
    };
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
    fn fill_path_substitutes_and_consumes_params() {
        let mut p = serde_json::json!({"s": "abc", "extra": 1})
            .as_object()
            .cloned()
            .unwrap();
        let path = fill_path("/v1/sessions/{s}/input", &mut p).unwrap();
        assert_eq!(path, "/v1/sessions/abc/input");
        assert!(p.contains_key("extra") && !p.contains_key("s"));
        // A missing placeholder errors, naming the key.
        let mut empty = serde_json::Map::new();
        let err = fill_path("/v1/pairings/{id}", &mut empty).unwrap_err();
        assert!(err.to_string().contains("id"), "{err}");
    }

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
