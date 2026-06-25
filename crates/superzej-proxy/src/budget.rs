//! Per-agent identity, spend attribution, and budget enforcement (group V).
//!
//! A request authenticates with a virtual key (V 287) that resolves to a caller
//! scope; spend is attributed to that scope plus the `global` scope (V 290), and
//! a pre-routing check refuses (or downgrades) when a cap or the kill-switch is
//! hit (V 292/293/296). This is net-new versus the Go proxy.

use superzej_core::db::Db;

use crate::shared::{SharedDb, now_ms};

/// The resolved caller behind a request.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub virtual_key: Option<String>,
    /// Budget scope, e.g. `global`, `agent:<name>`, `worktree:<path>`.
    pub scope: String,
}

impl Identity {
    /// The anonymous/global identity used when no virtual key is presented.
    pub fn global() -> Self {
        Self {
            virtual_key: None,
            scope: "global".to_string(),
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
}

/// Resolves a virtual key (the bearer token presented to the proxy) into an
/// identity. An unknown/absent key falls back to the global scope.
pub fn resolve_identity(db: &SharedDb, virtual_key: Option<&str>) -> Identity {
    if let Some(key) = virtual_key
        && let Ok(guard) = db.lock()
        && let Ok(Some((scope, _upstream))) = guard.proxy_virtual_key(key)
    {
        return Identity {
            virtual_key: Some(key.to_string()),
            scope,
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
    /// Over the soft cap — proceed but prefer a cheaper tier where possible.
    Downgrade,
}

/// Checks the kill-switch and caps for the request's scope and the global scope.
/// `refuse_on_breach` selects refuse (true) vs. downgrade (false) when a cap is
/// exceeded; the kill-switch always refuses.
pub fn check_budget(db: &SharedDb, identity: &Identity, refuse_on_breach: bool) -> BudgetVerdict {
    for scope in [identity.scope.as_str(), "global"] {
        let row = match db.lock() {
            Ok(g) => g.proxy_budget(scope).ok().flatten(),
            Err(_) => None,
        };
        let Some(b) = row else { continue };
        if b.killed {
            return BudgetVerdict::Refuse(format!("budget kill-switch active for scope '{scope}'"));
        }
        let over_tokens = b.limit_tokens.is_some_and(|lim| b.spent_tokens >= lim);
        let over_cost = b.limit_cost.is_some_and(|lim| b.spent_cost >= lim);
        if over_tokens || over_cost {
            return if refuse_on_breach {
                BudgetVerdict::Refuse(format!("budget cap reached for scope '{scope}'"))
            } else {
                BudgetVerdict::Downgrade
            };
        }
    }
    BudgetVerdict::Allow
}

/// Attributes spend to the request's scope and the global scope. Returns the
/// post-update `killed` flag for the request's scope (so a breach mid-flight can
/// be surfaced). Mirrors the V 290 attribution rollup.
pub fn record_spend(db: &SharedDb, identity: &Identity, tokens: i64, cost: f64) -> bool {
    let ts = now_ms();
    let mut killed = false;
    if let Ok(guard) = db.lock() {
        if identity.scope != "global"
            && let Ok((_, _, k)) = Db::add_proxy_spend(&guard, &identity.scope, tokens, cost, ts)
        {
            killed = k;
        }
        let _ = Db::add_proxy_spend(&guard, "global", tokens, cost, ts);
    }
    killed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn db() -> SharedDb {
        Arc::new(Mutex::new(Db::open_memory().unwrap()))
    }

    #[test]
    fn unknown_key_is_global() {
        let db = db();
        let id = resolve_identity(&db, Some("nope"));
        assert_eq!(id.scope, "global");
        assert!(id.virtual_key.is_none());
    }

    #[test]
    fn known_key_resolves_scope() {
        let db = db();
        db.lock()
            .unwrap()
            .put_proxy_virtual_key("vk", "h", "rev", "agent:reviewer", None, 1)
            .unwrap();
        let id = resolve_identity(&db, Some("vk"));
        assert_eq!(id.scope, "agent:reviewer");
        assert_eq!(id.agent().as_deref(), Some("reviewer"));
    }

    #[test]
    fn kill_switch_refuses() {
        let db = db();
        db.lock()
            .unwrap()
            .set_proxy_kill_switch("global", true)
            .unwrap();
        let v = check_budget(&db, &Identity::global(), true);
        assert!(matches!(v, BudgetVerdict::Refuse(_)));
    }

    #[test]
    fn cap_refuses_or_downgrades() {
        let db = db();
        {
            let g = db.lock().unwrap();
            g.set_proxy_budget_limits("agent:x", "monthly", None, Some(1.0), 0)
                .unwrap();
            g.add_proxy_spend("agent:x", 0, 2.0, 1).unwrap(); // over the $1 cap
        }
        let id = Identity {
            virtual_key: None,
            scope: "agent:x".into(),
        };
        assert!(matches!(
            check_budget(&db, &id, true),
            BudgetVerdict::Refuse(_)
        ));
        assert_eq!(check_budget(&db, &id, false), BudgetVerdict::Downgrade);
    }

    #[test]
    fn spend_rolls_into_global_and_scope() {
        let db = db();
        let id = Identity {
            virtual_key: None,
            scope: "agent:y".into(),
        };
        record_spend(&db, &id, 100, 0.5);
        let g = db.lock().unwrap();
        assert_eq!(
            g.proxy_budget("agent:y").unwrap().unwrap().spent_tokens,
            100
        );
        assert_eq!(g.proxy_budget("global").unwrap().unwrap().spent_tokens, 100);
    }
}
