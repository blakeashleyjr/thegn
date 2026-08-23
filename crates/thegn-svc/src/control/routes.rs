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
