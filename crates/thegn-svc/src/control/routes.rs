//! The control API's route table: one row per HTTP route, naming the
//! capability it serves. `http::router` folds this into the axum router, and
//! `routes_cover_catalog` asserts it against
//! [`thegn_core::capability::CATALOG`] — so a new verb without a route, a
//! route without a catalog row, or a stale `SURFACE_GAPS` excuse all fail
//! `just test`.

use axum::routing::{MethodRouter, delete, get, post};

use super::http::ControlState;
use super::{http, http_ci};

/// One path. `caps` lists every capability the method router serves (a
/// `GET`+`POST` path serves two); unauthenticated routes (`/health`, `GET
/// /pair`, `POST /v1/pair`) serve none.
pub struct Route {
    pub path: &'static str,
    pub caps: &'static [&'static str],
    pub build: fn() -> MethodRouter<ControlState>,
}

const fn route(
    path: &'static str,
    caps: &'static [&'static str],
    build: fn() -> MethodRouter<ControlState>,
) -> Route {
    Route { path, caps, build }
}

pub static ROUTES: &[Route] = &[
    route("/health", &[], || get(http::health)),
    // Unauthenticated static redeem page: the pairing code rides in the URL
    // fragment (never sent to the server), so this serves HTML only.
    route("/pair", &[], || get(http::pair_page)),
    route("/v1/pair", &[], || post(http::pair)),
    route("/v1/me", &["me"], || get(http::me)),
    route("/v1/sessions", &["sessions.list", "sessions.open"], || {
        get(http::list_sessions).post(http::open_session)
    }),
    route("/v1/sessions/fork", &["sessions.fork"], || {
        post(http::fork_session)
    }),
    route("/v1/sessions/{s}/snapshot", &["sessions.snapshot"], || {
        get(http::snapshot)
    }),
    route("/v1/sessions/{s}/input", &["sessions.input"], || {
        post(http::send_input)
    }),
    route("/v1/sessions/{s}/resize", &["sessions.resize"], || {
        post(http::resize)
    }),
    route("/v1/sessions/{s}/wait", &["sessions.wait"], || {
        post(http::wait)
    }),
    route("/v1/sessions/{s}/split", &["sessions.split"], || {
        post(http::split)
    }),
    route("/v1/sessions/{s}/record", &["sessions.record"], || {
        post(http::record)
    }),
    route("/v1/sessions/{s}/detach", &["sessions.detach"], || {
        post(http::detach)
    }),
    route("/v1/sessions/{s}/attach", &["sessions.attach"], || {
        get(http::attach_ws)
    }),
    route("/v1/sessions/{s}", &["sessions.kill"], || {
        delete(http::kill)
    }),
    route("/v1/events", &["events.subscribe"], || get(http::events_ws)),
    route("/v1/events/sse", &["events.subscribe"], || {
        get(http::events_sse)
    }),
    route("/v1/leases", &["leases.list"], || get(http::leases)),
    route(
        "/v1/worktrees",
        &["worktrees.list", "worktrees.create"],
        || get(http::list_worktrees).post(http::create_worktree),
    ),
    route("/v1/skills", &["skills.list"], || get(http::list_skills)),
    route("/v1/worktrees/open", &["worktrees.open"], || {
        post(http::open_worktree)
    }),
    route("/v1/editor/open", &["editor.open"], || {
        post(http::open_editor)
    }),
    // --- agent orchestration (THE-57) ---------------------------------------
    route("/v1/issues", &["issues.list"], || get(http::issues_list)),
    route("/v1/issues/{id}", &["issues.get", "issues.update"], || {
        get(http::issue_get).post(http::issue_update)
    }),
    route("/v1/issues/{id}/comment", &["issues.comment"], || {
        post(http::issue_comment)
    }),
    route(
        "/v1/dispatches",
        &["dispatches.list", "dispatches.put"],
        || get(http::dispatches_list).post(http::dispatch_put),
    ),
    route(
        "/v1/dispatches/{id}/status",
        &["dispatches.set_status"],
        || post(http::dispatch_set_status),
    ),
    route("/v1/preview/fetch", &["preview.fetch"], || {
        post(http::preview_fetch)
    }),
    route("/v1/git/status", &["git.status"], || get(http::git_status)),
    route("/v1/git/stage", &["git.stage"], || post(http::git_stage)),
    route("/v1/git/commit", &["git.commit"], || post(http::git_commit)),
    route("/v1/merge/list", &["merge.list"], || get(http::merge_list)),
    route("/v1/merge/add", &["merge.add"], || post(http::merge_add)),
    route("/v1/merge/clear", &["merge.clear"], || {
        post(http::merge_clear)
    }),
    route("/v1/pr/status", &["pr.status"], || get(http::pr_status)),
    route("/v1/ci/runs", &["ci.runs"], || get(http_ci::ci_runs)),
    route("/v1/ci/logs", &["ci.logs"], || get(http_ci::ci_logs)),
    route("/v1/agent/sessions", &["agent.sessions"], || {
        get(http::agent_sessions)
    }),
    route("/v1/notify", &["notify.push"], || post(http::notify_push)),
    route("/v1/automations", &["automations.list"], || {
        get(http::automations_list)
    }),
    route("/v1/automations/test", &["automations.test"], || {
        post(http::automations_test)
    }),
    route("/v1/tools/run", &["tools.run"], || post(http::tools_run)),
    route("/v1/mcp_proxy/status", &["mcp_proxy.status"], || {
        get(http::mcp_proxy_status)
    }),
    route("/v1/mcp_proxy/reload", &["mcp_proxy.reload"], || {
        post(http::mcp_proxy_reload)
    }),
    route("/v1/calendar/events", &["calendar.events"], || {
        get(http::calendar_events)
    }),
    route("/v1/calendar/clocks", &["calendar.clocks"], || {
        get(http::calendar_clocks)
    }),
    route(
        "/v1/calendar/sources/{account}/events",
        &["calendar.ingest"],
        || post(http::calendar_ingest),
    ),
    route("/v1/pairings", &["pairings.list", "pairings.issue"], || {
        get(http::list_pairings).post(http::issue_pairing)
    }),
    route("/v1/pairings/{id}", &["pairings.revoke"], || {
        delete(http::revoke_pairing)
    }),
    route("/v1/pairings/{id}/approve", &["pairings.approve"], || {
        post(http::approve_pairing)
    }),
    route("/v1/daemon/shutdown", &["daemon.shutdown"], || {
        post(http::shutdown)
    }),
];

/// The generic client's spine (`thegn api call`): one `(capability id,
/// HTTP method, path template)` row per capability [`ROUTES`] serves.
/// `{placeholders}` are filled from the call's JSON params (and removed from
/// the body). Streaming capabilities (WebSocket/SSE) carry method `"WS"` and
/// are not callable generically. `api_calls_mirror_routes` pins this table
/// against [`ROUTES`] so a new route without its row fails `just test`.
pub static API_CALLS: &[(&str, &str, &str)] = &[
    ("me", "GET", "/v1/me"),
    ("sessions.list", "GET", "/v1/sessions"),
    ("sessions.open", "POST", "/v1/sessions"),
    ("sessions.fork", "POST", "/v1/sessions/fork"),
    ("sessions.snapshot", "GET", "/v1/sessions/{s}/snapshot"),
    ("sessions.input", "POST", "/v1/sessions/{s}/input"),
    ("sessions.resize", "POST", "/v1/sessions/{s}/resize"),
    ("sessions.wait", "POST", "/v1/sessions/{s}/wait"),
    ("sessions.split", "POST", "/v1/sessions/{s}/split"),
    ("sessions.record", "POST", "/v1/sessions/{s}/record"),
    ("sessions.detach", "POST", "/v1/sessions/{s}/detach"),
    ("sessions.attach", "WS", "/v1/sessions/{s}/attach"),
    ("sessions.kill", "DELETE", "/v1/sessions/{s}"),
    ("events.subscribe", "WS", "/v1/events"),
    ("leases.list", "GET", "/v1/leases"),
    ("worktrees.list", "GET", "/v1/worktrees"),
    ("worktrees.create", "POST", "/v1/worktrees"),
    ("worktrees.open", "POST", "/v1/worktrees/open"),
    ("skills.list", "GET", "/v1/skills"),
    ("editor.open", "POST", "/v1/editor/open"),
    ("issues.list", "GET", "/v1/issues"),
    ("issues.get", "GET", "/v1/issues/{id}"),
    ("issues.update", "POST", "/v1/issues/{id}"),
    ("issues.comment", "POST", "/v1/issues/{id}/comment"),
    ("dispatches.list", "GET", "/v1/dispatches"),
    ("dispatches.put", "POST", "/v1/dispatches"),
    (
        "dispatches.set_status",
        "POST",
        "/v1/dispatches/{id}/status",
    ),
    ("preview.fetch", "POST", "/v1/preview/fetch"),
    ("git.status", "GET", "/v1/git/status"),
    ("git.stage", "POST", "/v1/git/stage"),
    ("git.commit", "POST", "/v1/git/commit"),
    ("merge.list", "GET", "/v1/merge/list"),
    ("merge.add", "POST", "/v1/merge/add"),
    ("merge.clear", "POST", "/v1/merge/clear"),
    ("pr.status", "GET", "/v1/pr/status"),
    ("ci.runs", "GET", "/v1/ci/runs"),
    ("ci.logs", "GET", "/v1/ci/logs"),
    ("agent.sessions", "GET", "/v1/agent/sessions"),
    ("notify.push", "POST", "/v1/notify"),
    ("automations.list", "GET", "/v1/automations"),
    ("automations.test", "POST", "/v1/automations/test"),
    ("tools.run", "POST", "/v1/tools/run"),
    ("mcp_proxy.status", "GET", "/v1/mcp_proxy/status"),
    ("mcp_proxy.reload", "POST", "/v1/mcp_proxy/reload"),
    ("calendar.events", "GET", "/v1/calendar/events"),
    ("calendar.clocks", "GET", "/v1/calendar/clocks"),
    (
        "calendar.ingest",
        "POST",
        "/v1/calendar/sources/{account}/events",
    ),
    ("pairings.list", "GET", "/v1/pairings"),
    ("pairings.issue", "POST", "/v1/pairings"),
    ("pairings.revoke", "DELETE", "/v1/pairings/{id}"),
    ("pairings.approve", "POST", "/v1/pairings/{id}/approve"),
    ("daemon.shutdown", "POST", "/v1/daemon/shutdown"),
];

/// The `(method, path)` for a capability, if it is generically callable.
pub fn api_call_for(cap: &str) -> Option<(&'static str, &'static str)> {
    API_CALLS
        .iter()
        .find(|(c, _, _)| *c == cap)
        .map(|(_, m, p)| (*m, *p))
}

/// Resolve a `(method, path, body)` HTTP call for a capability from JSON params
/// — the shared spine of `thegn api call` (the generic CLI client) and the push
/// command inbox (the daemon's in-process dispatch). `{placeholders}` in the
/// path template are filled from `params` (and removed); remaining params ride
/// the percent-encoded query string on `GET`/`DELETE` and the JSON body on
/// `POST`. Non-string query values, including arrays, retain their existing
/// JSON text representation as one value; an empty query value remains
/// `name=`. `Err` names the problem (unknown/unrouted cap, streaming cap,
/// missing or unsafe placeholder).
///
/// This is the ONE place the catalog id → HTTP call mapping lives, so a new door
/// (the inbox) reuses it rather than growing a second dispatch table.
pub fn build_call(
    cap: &str,
    mut params: serde_json::Map<String, serde_json::Value>,
) -> Result<(&'static str, String, Option<serde_json::Value>), String> {
    if thegn_core::capability::lookup(cap).is_none() {
        return Err(format!("unknown capability {cap} — see `thegn api list`"));
    }
    let Some((method, template)) = api_call_for(cap) else {
        return Err(format!("{cap} has no HTTP route yet"));
    };
    if method == "WS" {
        return Err(format!(
            "{cap} is a streaming capability — not callable generically"
        ));
    }
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
                    format!(
                        "{}={}",
                        encode_query_component(k),
                        encode_query_component(&v)
                    )
                })
                .collect();
            path = format!("{path}?{}", qs.join("&"));
        }
        None
    } else {
        Some(serde_json::Value::Object(params))
    };
    Ok((method, path, body))
}

/// Fill `{placeholders}` in a path template from `params`, removing the used
/// keys. Errors on a placeholder with no matching param.
pub fn fill_path(
    template: &str,
    params: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let close = rest[open..]
            .find('}')
            .map(|i| open + i)
            .ok_or_else(|| "unbalanced path template".to_string())?;
        out.push_str(&rest[..open]);
        let key = &rest[open + 1..close];
        let val = params
            .remove(key)
            .ok_or_else(|| format!("missing path parameter {key:?}"))?;
        let raw = match val {
            serde_json::Value::String(s) => s,
            other => other.to_string().trim_matches('"').to_string(),
        };
        if raw.is_empty() {
            return Err(format!("path parameter {key:?} must not be empty"));
        }
        if raw == "." || raw == ".." {
            return Err(format!("path parameter {key:?} must not be a dot segment"));
        }
        out.push_str(&encode_path_component(&raw));
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Encode one raw path component. Exact `.` and `..` are rejected by
/// [`fill_path`] because URL implementations normalize even percent-encoded
/// dot segments; all other reserved bytes remain data in this segment.
fn encode_path_component(value: &str) -> String {
    percent_encode(value)
}

/// Encode one query name/value. Spaces use `%20` rather than `+` so the
/// generated call round-trips through query decoders without form semantics.
fn encode_query_component(value: &str) -> String {
    percent_encode(value)
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            use std::fmt::Write as _;
            write!(out, "%{byte:02X}").expect("writing percent encoding to String cannot fail");
        }
    }
    out
}

/// Every capability id the HTTP surface implements (duplicates collapsed).
pub fn implemented_caps() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = ROUTES.iter().flat_map(|r| r.caps.iter().copied()).collect();
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::capability::{Surface, coverage_problems};

    #[test]
    fn routes_cover_catalog() {
        let problems = coverage_problems(Surface::Http, &implemented_caps());
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn api_calls_mirror_routes() {
        // Same capability set, and every row's path is a real routed path
        // that lists that capability.
        let mut table: Vec<&str> = API_CALLS.iter().map(|(c, _, _)| *c).collect();
        table.sort_unstable();
        assert_eq!(table, implemented_caps(), "API_CALLS ⇔ ROUTES drifted");
        for (cap, method, path) in API_CALLS {
            let route = ROUTES
                .iter()
                .find(|r| r.path == *path)
                .unwrap_or_else(|| panic!("{cap}: no route at {path}"));
            assert!(route.caps.contains(cap), "{path} does not serve {cap}");
            assert!(
                matches!(*method, "GET" | "POST" | "DELETE" | "WS"),
                "{cap}: bad method {method}"
            );
        }
        // Multi-cap paths map each cap to a distinct method.
        for r in ROUTES.iter().filter(|r| r.caps.len() > 1) {
            let methods: std::collections::HashSet<&str> = API_CALLS
                .iter()
                .filter(|(_, _, p)| *p == r.path)
                .map(|(_, m, _)| *m)
                .collect();
            assert_eq!(methods.len(), r.caps.len(), "{}", r.path);
        }
    }

    #[test]
    fn build_call_fills_path_and_routes_params() {
        // GET: path placeholder consumed, leftover params → query string.
        let params = serde_json::json!({"worktree": "/w"})
            .as_object()
            .cloned()
            .unwrap();
        let (method, path, body) = build_call("git.status", params).unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/v1/git/status?worktree=%2Fw");
        assert!(body.is_none());
        // POST: params become the JSON body.
        let params = serde_json::json!({"worktree": "/w", "message": "hi"})
            .as_object()
            .cloned()
            .unwrap();
        let (method, path, body) = build_call("git.commit", params).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/git/commit");
        assert_eq!(body.unwrap()["message"], "hi");
        // Path placeholder filled + consumed.
        let params = serde_json::json!({"s": "abc", "b64": "AA=="})
            .as_object()
            .cloned()
            .unwrap();
        let (_, path, body) = build_call("sessions.input", params).unwrap();
        assert_eq!(path, "/v1/sessions/abc/input");
        assert_eq!(body.unwrap()["b64"], "AA==");
    }

    #[test]
    fn build_call_encodes_path_components_and_query_names_and_values() {
        let params = serde_json::json!({
            "s": "client/with?delimiters#and%escapes",
        })
        .as_object()
        .cloned()
        .unwrap();
        let (_, path, _) = build_call("sessions.snapshot", params).unwrap();
        assert_eq!(
            path,
            "/v1/sessions/client%2Fwith%3Fdelimiters%23and%25escapes/snapshot"
        );

        let params = serde_json::json!({"s": r"client\with"})
            .as_object()
            .cloned()
            .unwrap();
        let (_, path, _) = build_call("sessions.snapshot", params).unwrap();
        assert_eq!(path, "/v1/sessions/client%5Cwith/snapshot");

        let params = serde_json::json!({
            "work tree&name": "a/b?c#d% e客户",
            "array": ["x", "y"],
        })
        .as_object()
        .cloned()
        .unwrap();
        let (_, path, _) = build_call("git.status", params).unwrap();
        assert_eq!(
            path,
            "/v1/git/status?array=%5B%22x%22%2C%22y%22%5D&work%20tree%26name=a%2Fb%3Fc%23d%25%20e%E5%AE%A2%E6%88%B7"
        );

        let params = serde_json::json!({"optional": ""})
            .as_object()
            .cloned()
            .unwrap();
        let (_, path, _) = build_call("git.status", params).unwrap();
        assert_eq!(path, "/v1/git/status?optional=");
    }

    #[test]
    fn build_call_rejects_empty_and_exact_dot_path_components() {
        for value in ["", ".", ".."] {
            let params = serde_json::json!({"s": value})
                .as_object()
                .cloned()
                .unwrap();
            let error = build_call("sessions.snapshot", params).unwrap_err();
            assert!(error.contains("path parameter"), "{value:?}: {error}");
        }

        let params = serde_json::json!({"s": "../other"})
            .as_object()
            .cloned()
            .unwrap();
        let (_, path, _) = build_call("sessions.snapshot", params).unwrap();
        assert_eq!(path, "/v1/sessions/..%2Fother/snapshot");

        for (value, encoded) in [("%2E", "%252E"), ("%2E%2E", "%252E%252E")] {
            let params = serde_json::json!({"s": value})
                .as_object()
                .cloned()
                .unwrap();
            let (_, path, _) = build_call("sessions.snapshot", params).unwrap();
            assert_eq!(path, format!("/v1/sessions/{encoded}/snapshot"));
        }
    }

    #[test]
    fn build_call_rejects_unknown_streaming_and_missing_placeholder() {
        assert_eq!(
            build_call("browser.drive", Default::default()).unwrap_err(),
            "unknown capability browser.drive — see `thegn api list`"
        );
        assert!(
            build_call("nope.nope", Default::default())
                .unwrap_err()
                .contains("unknown")
        );
        // A streaming (WS) capability is not generically callable.
        assert!(
            build_call("sessions.attach", Default::default())
                .unwrap_err()
                .contains("streaming")
        );
        // Missing path placeholder names the key.
        let err = build_call("sessions.snapshot", Default::default()).unwrap_err();
        assert!(err.contains("path parameter"), "{err}");
    }

    #[test]
    fn paths_are_unique_and_versioned() {
        let mut seen = std::collections::HashSet::new();
        for r in ROUTES {
            assert!(seen.insert(r.path), "duplicate path {}", r.path);
            assert!(
                r.path == "/health" || r.path == "/pair" || r.path.starts_with("/v1/"),
                "{} is not under /v1",
                r.path
            );
        }
        // Unauthenticated routes are exactly the three the http module doc
        // names: the health probe, the static pairing-redeem page, and the
        // pairing-code redeem endpoint (the code IS the credential there).
        let open: Vec<&str> = ROUTES
            .iter()
            .filter(|r| r.caps.is_empty())
            .map(|r| r.path)
            .collect();
        assert_eq!(open, ["/health", "/pair", "/v1/pair"]);
    }
}
