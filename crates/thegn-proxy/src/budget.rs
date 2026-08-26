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
use crate::shared::{SharedDb, now_ms};

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

/// Reads the current (window-aware) spend for a scope: a lapsed rolling window
/// counts as zero, since accumulated spend belongs to the previous period.
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
pub fn record_spend(
    db: &SharedDb,
    settings: &BudgetSettings,
    identity: &Identity,
    tokens: i64,
    cost: f64,
) {
    let ts = now_ms();
    if let Ok(guard) = db.lock() {
        for scope in identity.budget_scopes() {
            let _ = guard.add_model_proxy_spend(scope, tokens, cost, ts, settings.window_len_ms);
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
        record_spend(&db, &BudgetSettings::default(), &id, 0, 2.0); // $2 spent
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
        record_spend(&db, &BudgetSettings::default(), &id, 50, 0.25);
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
        record_spend(&db, &BudgetSettings::default(), &id, 20, 0.0);
        let s = settings(BudgetBreach::Refuse, &[("workspace:/repo", Some(10), None)]);
        assert!(matches!(
            check_budget(&db, &s, &id, 0),
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
        record_spend(&db, &s, &id, 0, 2.0);
        assert!(matches!(
            check_budget(&db, &s, &id, 999),
            BudgetVerdict::Refuse(_)
        ));
        // Past the window: old spend no longer counts.
        assert_eq!(check_budget(&db, &s, &id, 1000), BudgetVerdict::Allow);
    }
}
