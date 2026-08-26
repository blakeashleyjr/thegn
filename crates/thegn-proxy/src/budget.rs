//! Per-agent identity, spend attribution, and budget enforcement (group V).
//!
//! A request authenticates with a virtual attribution key ([`thegn_core::proxy::
//! attribution`]) that is self-describing: the caller-scope chain is baked in at
//! mint time, so the proxy resolves the caller with no DB lookup. Spend is
//! attributed to that scope plus every enclosing scope up to `global`, and a
//! pre-routing check refuses (or downgrades) when a configured cap or the manual
//! kill-switch is hit. Budget *ceilings* come from `[model_proxy.budget]`; the
//! `model_proxy_budget_state` table holds only the rolling accumulators.

use thegn_core::proxy::attribution;
use thegn_core::store::ModelProxyStore;

use crate::model::BudgetSettings;
use crate::shared::SharedDb;

/// The resolved caller behind a request.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    /// Budget scope, e.g. `global`, `agent:<name>`, `worktree:<path>`,
    /// `workspace:<repo_path>`.
    pub scope: String,
    /// The enclosing workspace scope (`workspace:<repo_path>`).
    pub workspace: Option<String>,
    /// The worktree's zone scope (`zone:<name>`), when it belongs to one.
    pub zone: Option<String>,
    /// The virtual key's upstream-account binding: routing prefers this
    /// provider's lanes so a workspace's traffic sticks to its scoped account.
    pub upstream: Option<String>,
}

impl Identity {
    /// The anonymous/global identity used when no virtual key is presented.
    pub fn global() -> Self {
        Self {
            scope: "global".to_string(),
            ..Self::default()
        }
    }

    /// The `agent` label derived from an `agent:<name>` scope, for audit rows.
    pub fn agent(&self) -> Option<String> {
        self.scope.strip_prefix("agent:").map(str::to_string)
    }

    /// The `worktree` label derived from a `worktree:<path>` scope.
    pub fn worktree(&self) -> Option<String> {
        self.scope.strip_prefix("worktree:").map(str::to_string)
    }

    /// The workspace label (repo path) for audit rows.
    pub fn workspace_label(&self) -> Option<String> {
        self.scope
            .strip_prefix("workspace:")
            .map(str::to_string)
            .or_else(|| {
                self.workspace
                    .as_deref()
                    .and_then(|w| w.strip_prefix("workspace:"))
                    .map(str::to_string)
            })
    }

    /// The budget scopes this identity's spend rolls into, most specific first,
    /// deduped: scope → workspace → zone → global.
    pub fn budget_scopes(&self) -> Vec<&str> {
        let mut out: Vec<&str> = vec![self.scope.as_str()];
        for s in [self.workspace.as_deref(), self.zone.as_deref()]
            .into_iter()
            .flatten()
        {
            if !out.contains(&s) {
                out.push(s);
            }
        }
        if !out.contains(&"global") {
            out.push("global");
        }
        out
    }
}

/// Resolves a virtual key (the bearer token presented to the proxy) into an
/// identity. An absent or unparseable key falls back to the global scope.
pub fn resolve_identity(virtual_key: Option<&str>) -> Identity {
    if let Some(key) = virtual_key
        && let Some(a) = attribution::decode(key)
    {
        return Identity {
            scope: if a.scope.is_empty() {
                "global".to_string()
            } else {
                a.scope
            },
            workspace: a.workspace,
            zone: a.zone,
            upstream: a.upstream,
        };
    }
    Identity::global()
}

/// The verdict of a pre-routing budget check.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetVerdict {
    /// Proceed normally.
    Allow,
    /// Refuse the request (cap hit or kill-switch); carries a client-facing reason.
    Refuse(String),
    /// Over the cap — proceed but prefer a cheaper tier where possible.
    Downgrade,
}

/// Reads the current (window-aware) spend for a scope at `now_ms`.
///
/// Three cases all resolve to *zero spend* (i.e. nothing to enforce against):
/// * no state row yet — a scope's first-ever request;
/// * a lapsed rolling window (`now - anchor >= window_len`) — the accumulated
///   spend belongs to the previous period and the anchor advances on the next
///   [`record_spend`], so the new window starts empty;
/// * the boundary itself (`now - anchor == window_len`) — the window is
///   half-open `[anchor, anchor + window_len)`, so the boundary tick is already
///   the next window. `add_model_proxy_spend` re-anchors on the same `>=`.
///
/// `window_len_ms == 0` means "cumulative": the window never lapses.
fn current_spend(
    db: &SharedDb,
    scope: &str,
    window_len_ms: i64,
    now_ms: i64,
) -> Option<(i64, f64, bool)> {
    let row = db
        .lock()
        .ok()?
        .model_proxy_budget_state(scope)
        .ok()
        .flatten()?;
    let lapsed = window_len_ms > 0 && now_ms - row.window_start_ms >= window_len_ms;
    let (tokens, cost) = if lapsed {
        (0, 0.0)
    } else {
        (row.spent_tokens, row.spent_cost)
    };
    Some((tokens, cost, row.killed))
}

/// Checks the kill-switch and configured caps along the identity's rollup chain
/// (scope → workspace → zone → global). A member request is refused by any
/// enclosing cap even when under its own. The kill-switch always refuses; caps
/// refuse or downgrade per `settings.on_breach`.
pub fn check_budget(
    db: &SharedDb,
    settings: &BudgetSettings,
    identity: &Identity,
    now_ms: i64,
) -> BudgetVerdict {
    for scope in identity.budget_scopes() {
        let Some((spent_tokens, spent_cost, killed)) =
            current_spend(db, scope, settings.window_len_ms, now_ms)
        else {
            continue;
        };
        // The manual kill-switch is honored even when enforcement is off.
        if killed {
            return BudgetVerdict::Refuse(format!("budget kill-switch active for scope '{scope}'"));
        }
        if !settings.enabled {
            continue;
        }
        let Some((tok_cap, cost_cap)) = settings.scopes.get(scope) else {
            continue;
        };
        let over_tokens = tok_cap.is_some_and(|lim| spent_tokens >= lim);
        let over_cost = cost_cap.is_some_and(|lim| spent_cost >= lim);
        if over_tokens || over_cost {
            return if settings.refuses() {
                BudgetVerdict::Refuse(format!("budget cap reached for scope '{scope}'"))
            } else if settings.downgrades() {
                BudgetVerdict::Downgrade
            } else {
                // warn: never blocks (the alert is raised by the shell from the
                // audit rows); serve normally.
                BudgetVerdict::Allow
            };
        }
    }
    BudgetVerdict::Allow
}

/// Attributes spend along the identity's rollup chain (scope → workspace → zone
/// → global, deduped), advancing each scope's rolling-window anchor as needed.
///
/// The timestamp is the caller's, not a fresh `now_ms()` read: a request's
/// budget check and its spend record must land in the SAME window, or spend
/// anchored on one timeline gets judged on another (and the window arithmetic
/// stops being testable at all). [`crate::shared::now_ms`] is what production
/// callers pass.
pub fn record_spend(
    db: &SharedDb,
    settings: &BudgetSettings,
    identity: &Identity,
    tokens: i64,
    cost: f64,
    now_ms: i64,
) {
    let guard = match db.lock() {
        Ok(g) => g,
        Err(e) => {
            // fail-open by decision: see the per-scope write below — a spend the
            // proxy cannot record must not take the proxy down with it.
            tracing::warn!(
                target: "thegn::proxy",
                scope = %identity.scope,
                error = %e,
                "budget spend not recorded: db lock poisoned (caps under-count)"
            );
            return;
        }
    };
    for scope in identity.budget_scopes() {
        // fail-open by decision: this write is what ENFORCES the rolling window,
        // so a failure means the spend goes uncounted and the cap trips late (or
        // never). We still do not refuse the request — availability over
        // enforcement, the same MemoryHigh-not-MemoryMax posture the rest of the
        // repo takes — but the miss is never silent.
        if let Err(e) =
            guard.add_model_proxy_spend(scope, tokens, cost, now_ms, settings.window_len_ms)
        {
            tracing::warn!(
                target: "thegn::proxy",
                scope = %scope,
                tokens,
                cost,
                error = %e,
                "budget spend not recorded: caps under-count for this scope"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use thegn_core::config_model_proxy::BudgetBreach;
    use thegn_core::db::Db;

    fn db() -> SharedDb {
        Arc::new(Mutex::new(Db::open_memory().unwrap()))
    }

    fn settings(
        on_breach: BudgetBreach,
        scopes: &[(&str, Option<i64>, Option<f64>)],
    ) -> BudgetSettings {
        BudgetSettings {
            enabled: true,
            on_breach,
            window_len_ms: 0,
            scopes: scopes
                .iter()
                .map(|(s, t, c)| (s.to_string(), (*t, *c)))
                .collect(),
        }
    }

    fn key(scope: &str, workspace: Option<&str>, zone: Option<&str>) -> String {
        attribution::encode(&attribution::AttributionScope {
            scope: scope.into(),
            workspace: workspace.map(String::from),
            zone: zone.map(String::from),
            upstream: None,
            nonce: "n".into(),
        })
    }

    #[test]
    fn unknown_key_is_global() {
        let id = resolve_identity(Some("garbage"));
        assert_eq!(id.scope, "global");
        assert!(resolve_identity(None).scope == "global");
    }

    #[test]
    fn key_resolves_full_chain() {
        let id = resolve_identity(Some(&key(
            "worktree:/repo/wt",
            Some("workspace:/repo"),
            Some("zone:c"),
        )));
        assert_eq!(id.scope, "worktree:/repo/wt");
        assert_eq!(id.workspace.as_deref(), Some("workspace:/repo"));
        assert_eq!(id.zone.as_deref(), Some("zone:c"));
        assert_eq!(
            id.budget_scopes(),
            vec!["worktree:/repo/wt", "workspace:/repo", "zone:c", "global"]
        );
        assert_eq!(id.workspace_label().as_deref(), Some("/repo"));
    }

    #[test]
    fn cap_refuses_or_downgrades() {
        let db = db();
        let id = Identity {
            scope: "agent:x".into(),
            ..Default::default()
        };
        record_spend(&db, &BudgetSettings::default(), &id, 0, 2.0, 0); // $2 spent
        let refuse = settings(BudgetBreach::Refuse, &[("agent:x", None, Some(1.0))]);
        assert!(matches!(
            check_budget(&db, &refuse, &id, 0),
            BudgetVerdict::Refuse(_)
        ));
        let downgrade = settings(BudgetBreach::Downgrade, &[("agent:x", None, Some(1.0))]);
        assert_eq!(
            check_budget(&db, &downgrade, &id, 0),
            BudgetVerdict::Downgrade
        );
        let warn = settings(BudgetBreach::Warn, &[("agent:x", None, Some(1.0))]);
        assert_eq!(check_budget(&db, &warn, &id, 0), BudgetVerdict::Allow);
    }

    #[test]
    fn kill_switch_refuses_even_when_disabled() {
        let db = db();
        db.lock()
            .unwrap()
            .set_model_proxy_kill_switch("global", true)
            .unwrap();
        let off = BudgetSettings::default(); // enforcement off
        assert!(matches!(
            check_budget(&db, &off, &Identity::global(), 0),
            BudgetVerdict::Refuse(_)
        ));
    }

    #[test]
    fn spend_rolls_into_enclosing_scopes() {
        let db = db();
        let id = Identity {
            scope: "worktree:/repo/wt".into(),
            workspace: Some("workspace:/repo".into()),
            zone: Some("zone:c".into()),
            ..Default::default()
        };
        record_spend(&db, &BudgetSettings::default(), &id, 50, 0.25, 0);
        let g = db.lock().unwrap();
        for scope in ["worktree:/repo/wt", "workspace:/repo", "zone:c", "global"] {
            assert_eq!(
                g.model_proxy_budget_state(scope)
                    .unwrap()
                    .unwrap()
                    .spent_tokens,
                50,
                "scope {scope}"
            );
        }
    }

    #[test]
    fn enclosing_cap_refuses_member() {
        let db = db();
        let id = Identity {
            scope: "worktree:/repo/wt".into(),
            workspace: Some("workspace:/repo".into()),
            ..Default::default()
        };
        // Only the workspace has a cap, already spent past it.
        record_spend(&db, &BudgetSettings::default(), &id, 20, 0.0, 0);
        let s = settings(BudgetBreach::Refuse, &[("workspace:/repo", Some(10), None)]);
        assert!(matches!(
            check_budget(&db, &s, &id, 0),
            BudgetVerdict::Refuse(_)
        ));
    }

    #[test]
    fn no_state_row_allows_with_zero_spend() {
        // A scope's first-ever request has no accumulator row: nothing to
        // enforce against, so every cap-bearing scope in the chain allows.
        let db = db();
        let id = Identity {
            scope: "agent:fresh".into(),
            ..Default::default()
        };
        let mut s = settings(
            BudgetBreach::Refuse,
            &[("agent:fresh", Some(1), Some(0.01))],
        );
        s.window_len_ms = 1000;
        assert_eq!(check_budget(&db, &s, &id, 0), BudgetVerdict::Allow);
        assert_eq!(check_budget(&db, &s, &id, 5_000), BudgetVerdict::Allow);
    }

    #[test]
    fn window_boundary_is_the_next_window() {
        // The window is half-open [anchor, anchor + len): the last tick inside
        // it still enforces, the boundary tick itself is already the next
        // window and reads as zero spend.
        let db = db();
        let id = Identity {
            scope: "agent:b".into(),
            ..Default::default()
        };
        let mut s = settings(BudgetBreach::Refuse, &[("agent:b", Some(10), None)]);
        s.window_len_ms = 1000;
        record_spend(&db, &s, &id, 50, 0.0, 100); // anchored at t=100
        assert!(matches!(
            check_budget(&db, &s, &id, 1_099),
            BudgetVerdict::Refuse(_)
        ));
        assert_eq!(check_budget(&db, &s, &id, 1_100), BudgetVerdict::Allow);
        // …and a cumulative budget (window_len_ms = 0) never lapses.
        let mut cumulative = s.clone();
        cumulative.window_len_ms = 0;
        assert!(matches!(
            check_budget(&db, &cumulative, &id, i64::MAX),
            BudgetVerdict::Refuse(_)
        ));
    }

    #[test]
    fn lapsed_window_counts_as_zero() {
        let db = db();
        let id = Identity {
            scope: "agent:x".into(),
            ..Default::default()
        };
        let mut s = settings(BudgetBreach::Refuse, &[("agent:x", None, Some(1.0))]);
        s.window_len_ms = 1000;
        // Spend $2 anchored at t=0.
        record_spend(&db, &s, &id, 0, 2.0, 0);
        assert!(matches!(
            check_budget(&db, &s, &id, 999),
            BudgetVerdict::Refuse(_)
        ));
        // Past the window: old spend no longer counts.
        assert_eq!(check_budget(&db, &s, &id, 1000), BudgetVerdict::Allow);
    }
}
