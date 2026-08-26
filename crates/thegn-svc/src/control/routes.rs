//! The control API's route table: one row per HTTP route, naming the
//! capability it serves. `http::router` folds this into the axum router, and
//! `routes_cover_catalog` asserts it against
//! [`thegn_core::capability::CATALOG`] — so a new verb without a route, a
//! route without a catalog row, or a stale `SURFACE_GAPS` excuse all fail
//! `just test`.

use axum::routing::{MethodRouter, delete, get, post};

use super::http::{self, ControlState};

/// One path. `caps` lists every capability the method router serves (a
/// `GET`+`POST` path serves two); unauthenticated routes (`/health`,
/// `POST /v1/pair`) serve none.
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
    route("/v1/pair", &[], || post(http::pair)),
    route("/v1/me", &["me"], || get(http::me)),
    route("/v1/sessions", &["sessions.list", "sessions.open"], || {
        get(http::list_sessions).post(http::open_session)
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
    route("/v1/worktrees/open", &["worktrees.open"], || {
        post(http::open_worktree)
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
    route("/v1/browser", &["browser.drive"], || post(http::browser)),
    route("/v1/git/status", &["git.status"], || get(http::git_status)),
    route("/v1/git/stage", &["git.stage"], || post(http::git_stage)),
    route("/v1/git/commit", &["git.commit"], || post(http::git_commit)),
    route("/v1/merge/list", &["merge.list"], || get(http::merge_list)),
    route("/v1/merge/add", &["merge.add"], || post(http::merge_add)),
    route("/v1/merge/clear", &["merge.clear"], || {
        post(http::merge_clear)
    }),
    route("/v1/pr/status", &["pr.status"], || get(http::pr_status)),
    route("/v1/agent/sessions", &["agent.sessions"], || {
        get(http::agent_sessions)
    }),
    route("/v1/notify", &["notify.push"], || post(http::notify_push)),
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
    ("browser.drive", "POST", "/v1/browser"),
    ("git.status", "GET", "/v1/git/status"),
    ("git.stage", "POST", "/v1/git/stage"),
    ("git.commit", "POST", "/v1/git/commit"),
    ("merge.list", "GET", "/v1/merge/list"),
    ("merge.add", "POST", "/v1/merge/add"),
    ("merge.clear", "POST", "/v1/merge/clear"),
    ("pr.status", "GET", "/v1/pr/status"),
    ("agent.sessions", "GET", "/v1/agent/sessions"),
    ("notify.push", "POST", "/v1/notify"),
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
/// the query string on `GET`/`DELETE` and the JSON body on `POST`. `Err` names
/// the problem (unknown/unrouted cap, streaming cap, missing placeholder).
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
                    format!("{k}={v}")
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
        match val {
            serde_json::Value::String(s) => out.push_str(&s),
            other => out.push_str(other.to_string().trim_matches('"')),
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
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
        assert_eq!(path, "/v1/git/status?worktree=/w");
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
    fn build_call_rejects_unknown_streaming_and_missing_placeholder() {
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
                r.path == "/health" || r.path.starts_with("/v1/"),
                "{} is not under /v1",
                r.path
            );
        }
        // Unauthenticated routes are exactly the two the http module doc names.
        let open: Vec<&str> = ROUTES
            .iter()
            .filter(|r| r.caps.is_empty())
            .map(|r| r.path)
            .collect();
        assert_eq!(open, ["/health", "/v1/pair"]);
    }
}
