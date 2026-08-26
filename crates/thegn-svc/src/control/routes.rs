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
    route("/v1/worktrees", &["worktrees.list"], || {
        get(http::list_worktrees)
    }),
    route("/v1/worktrees/open", &["worktrees.open"], || {
        post(http::open_worktree)
    }),
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
    ("sessions.detach", "POST", "/v1/sessions/{s}/detach"),
    ("sessions.attach", "WS", "/v1/sessions/{s}/attach"),
    ("sessions.kill", "DELETE", "/v1/sessions/{s}"),
    ("events.subscribe", "WS", "/v1/events"),
    ("leases.list", "GET", "/v1/leases"),
    ("worktrees.list", "GET", "/v1/worktrees"),
    ("worktrees.open", "POST", "/v1/worktrees/open"),
    ("browser.drive", "POST", "/v1/browser"),
    ("git.status", "GET", "/v1/git/status"),
    ("git.stage", "POST", "/v1/git/stage"),
    ("git.commit", "POST", "/v1/git/commit"),
    ("merge.list", "GET", "/v1/merge/list"),
    ("merge.add", "POST", "/v1/merge/add"),
    ("merge.clear", "POST", "/v1/merge/clear"),
    ("pr.status", "GET", "/v1/pr/status"),
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
