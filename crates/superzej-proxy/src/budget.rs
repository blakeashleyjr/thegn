//! Per-agent identity, spend attribution, and budget enforcement (group V).
//!
//! A request authenticates with a virtual key (V 287) that resolves to a caller
//! scope; spend is attributed to that scope plus the `global` scope (V 290), and
//! a pre-routing check refuses (or downgrades) when a cap or the kill-switch is
//! hit (V 292/293/296). This is net-new versus the Go proxy.

#[cfg(test)]
use superzej_core::db::Db;
use superzej_core::store::{ProxyStore, WorkspaceStore, ZoneStore};

use crate::shared::{SharedDb, now_ms};

/// The resolved caller behind a request.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub virtual_key: Option<String>,
    /// Budget scope, e.g. `global`, `agent:<name>`, `worktree:<path>`,
    /// `workspace:<repo_path>`.
    pub scope: String,
    /// The enclosing workspace scope (`workspace:<repo_path>`), derived from a
    /// `worktree:<path>` scope. Spend rolls up scope → workspace → zone →
    /// global, so per-workspace caps govern all of a workspace's worktrees.
    pub workspace: Option<String>,
    /// The worktree's zone scope (`zone:<name>`), when the worktree belongs to
    /// one. Resolved per-request from the shared DB (no push/sync — szproxy
    /// opens the same per-profile DB).
    pub zone: Option<String>,
    /// The virtual key's upstream account binding: routing prefers this
    /// provider's lanes, so a workspace's traffic sticks to the account scoped
    /// to it (V 287, scoped accounts per workspace).
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

    /// The workspace label (repo path) for audit rows: a `workspace:<repo>`
    /// scope directly, else the workspace derived from the worktree scope.
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
/// identity. An unknown/absent key falls back to the global scope.
pub fn resolve_identity(db: &SharedDb, virtual_key: Option<&str>) -> Identity {
    if let Some(key) = virtual_key
        && let Ok(guard) = db.lock()
        && let Ok(Some((scope, upstream))) = guard.proxy_virtual_key(key)
    {
        // Roll the enclosing workspace + zone into the identity. A worktree
        // scope derives both; a workspace scope derives its zone directly;
        // other scopes (`agent:<name>`) carry no path, so neither.
        let repo = scope
            .strip_prefix("worktree:")
            .and_then(|wt| guard.repo_root_for(wt).ok().flatten())
            .or_else(|| scope.strip_prefix("workspace:").map(str::to_string));
        let workspace = repo
            .as_deref()
            .filter(|_| scope.starts_with("worktree:"))
            .map(|r| format!("workspace:{r}"));
        let zone = repo
            .as_deref()
            .and_then(|r| guard.zone_of_workspace(r).ok().flatten())
            .map(|z| format!("zone:{}", z.name));
        return Identity {
            virtual_key: Some(key.to_string()),
            scope,
            workspace,
            zone,
            upstream,
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

/// Checks the kill-switch and caps along the identity's rollup chain
/// (scope → workspace → zone → global). A member request is refused by any
/// enclosing cap even when under its own. `refuse_on_breach` selects refuse
/// (true) vs. downgrade (false) when a cap is exceeded; the kill-switch always
/// refuses. `now_ms` makes the check window-aware: a budget whose rolling
/// window has lapsed counts as zero spend (the next attribution rolls it over).
pub fn check_budget(
    db: &SharedDb,
    identity: &Identity,
    refuse_on_breach: bool,
    now_ms: i64,
) -> BudgetVerdict {
    for scope in identity.budget_scopes() {
        let row = match db.lock() {
            Ok(g) => g.proxy_budget(scope).ok().flatten(),
            Err(_) => None,
        };
        let Some(b) = row else { continue };
        if b.killed {
            return BudgetVerdict::Refuse(format!("budget kill-switch active for scope '{scope}'"));
        }
        // A lapsed window means the accumulated spend belongs to the previous
        // period — nothing has been spent in the current one yet.
        let window_lapsed = b.reset_ms > 0 && b.reset_ms <= now_ms;
        let (spent_tokens, spent_cost) = if window_lapsed {
            (0, 0.0)
        } else {
            (b.spent_tokens, b.spent_cost)
        };
        let over_tokens = b.limit_tokens.is_some_and(|lim| spent_tokens >= lim);
        let over_cost = b.limit_cost.is_some_and(|lim| spent_cost >= lim);
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

/// Attributes spend along the identity's rollup chain (scope → workspace →
/// zone → global, deduped). Returns the post-update `killed` flag for the
/// request's own scope (so a breach mid-flight can be surfaced). Mirrors the
/// V 290 attribution rollup.
pub fn record_spend(db: &SharedDb, identity: &Identity, tokens: i64, cost: f64) -> bool {
    let ts = now_ms();
    let mut killed = false;
    if let Ok(guard) = db.lock() {
        for scope in identity.budget_scopes() {
            if let Ok((_, _, k)) = guard.add_proxy_spend(scope, tokens, cost, ts)
                && scope == identity.scope
            {
                killed = k;
            }
        }
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
        let v = check_budget(&db, &Identity::global(), true, 0);
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
            scope: "agent:x".into(),
            ..Identity::default()
        };
        assert!(matches!(
            check_budget(&db, &id, true, 0),
            BudgetVerdict::Refuse(_)
        ));
        assert_eq!(check_budget(&db, &id, false, 0), BudgetVerdict::Downgrade);
    }

    #[test]
    fn lapsed_window_counts_as_zero_spend() {
        let db = db();
        {
            let g = db.lock().unwrap();
            // $1 cap, window anchored at t=1000 — already spent over the cap.
            g.set_proxy_budget_limits("agent:x", "daily", None, Some(1.0), 1000)
                .unwrap();
            g.add_proxy_spend("agent:x", 0, 2.0, 1).unwrap();
        }
        let id = Identity {
            scope: "agent:x".into(),
            ..Identity::default()
        };
        // Inside the window: over cap → refused.
        assert!(matches!(
            check_budget(&db, &id, true, 999),
            BudgetVerdict::Refuse(_)
        ));
        // Past the window anchor: the old spend no longer counts.
        assert_eq!(check_budget(&db, &id, true, 1000), BudgetVerdict::Allow);
    }

    #[test]
    fn spend_rolls_into_global_and_scope() {
        let db = db();
        let id = Identity {
            scope: "agent:y".into(),
            ..Identity::default()
        };
        record_spend(&db, &id, 100, 0.5);
        let g = db.lock().unwrap();
        assert_eq!(
            g.proxy_budget("agent:y").unwrap().unwrap().spent_tokens,
            100
        );
        assert_eq!(g.proxy_budget("global").unwrap().unwrap().spent_tokens, 100);
    }

    #[test]
    fn resolve_identity_rolls_in_workspace_and_zone() {
        use superzej_core::store::{WorkspaceStore, ZoneStore};
        let db = db();
        {
            let g = db.lock().unwrap();
            g.put_workspace("/repo", "ws", "repo").unwrap();
            g.put_worktree("t", "/repo", "/repo/wt", "main", None, None)
                .unwrap();
            let z = g.create_zone("clientA", 1).unwrap();
            g.assign_workspace_zone("/repo", Some(z)).unwrap();
            g.put_proxy_virtual_key("vk", "h", "rev", "worktree:/repo/wt", Some("openrouter"), 1)
                .unwrap();
        }
        let id = resolve_identity(&db, Some("vk"));
        assert_eq!(id.scope, "worktree:/repo/wt");
        assert_eq!(id.workspace.as_deref(), Some("workspace:/repo"));
        assert_eq!(id.zone.as_deref(), Some("zone:clientA"));
        assert_eq!(id.upstream.as_deref(), Some("openrouter"));
        assert_eq!(id.workspace_label().as_deref(), Some("/repo"));
        assert_eq!(
            id.budget_scopes(),
            vec![
                "worktree:/repo/wt",
                "workspace:/repo",
                "zone:clientA",
                "global"
            ]
        );
    }

    #[test]
    fn resolve_identity_workspace_scope_direct() {
        use superzej_core::store::WorkspaceStore;
        let db = db();
        {
            let g = db.lock().unwrap();
            g.put_workspace("/repo", "ws", "repo").unwrap();
            g.put_proxy_virtual_key("vk", "h", "ws-key", "workspace:/repo", None, 1)
                .unwrap();
        }
        let id = resolve_identity(&db, Some("vk"));
        assert_eq!(id.scope, "workspace:/repo");
        // The scope IS the workspace — no duplicate rollup entry.
        assert!(id.workspace.is_none());
        assert_eq!(id.workspace_label().as_deref(), Some("/repo"));
        assert_eq!(id.budget_scopes(), vec!["workspace:/repo", "global"]);
    }

    #[test]
    fn zone_cap_refuses_member_under_own_cap() {
        let db = db();
        {
            let g = db.lock().unwrap();
            // Member has no cap; the zone cap is $1 and already spent.
            g.set_proxy_budget_limits("zone:clientA", "monthly", None, Some(1.0), 0)
                .unwrap();
            g.add_proxy_spend("zone:clientA", 0, 2.0, 1).unwrap();
        }
        let id = Identity {
            scope: "worktree:/repo/wt".into(),
            zone: Some("zone:clientA".into()),
            ..Identity::default()
        };
        assert!(matches!(
            check_budget(&db, &id, true, 0),
            BudgetVerdict::Refuse(_)
        ));
    }

    #[test]
    fn workspace_cap_refuses_member_worktree() {
        let db = db();
        {
            let g = db.lock().unwrap();
            g.set_proxy_budget_limits("workspace:/repo", "monthly", Some(10), None, 0)
                .unwrap();
            g.add_proxy_spend("workspace:/repo", 20, 0.0, 1).unwrap();
        }
        let id = Identity {
            scope: "worktree:/repo/wt".into(),
            workspace: Some("workspace:/repo".into()),
            ..Identity::default()
        };
        assert!(matches!(
            check_budget(&db, &id, true, 0),
            BudgetVerdict::Refuse(_)
        ));
    }

    #[test]
    fn spend_attributes_full_chain() {
        let db = db();
        let id = Identity {
            scope: "worktree:/repo/wt".into(),
            workspace: Some("workspace:/repo".into()),
            zone: Some("zone:clientA".into()),
            ..Identity::default()
        };
        record_spend(&db, &id, 50, 0.25);
        let g = db.lock().unwrap();
        for scope in [
            "worktree:/repo/wt",
            "workspace:/repo",
            "zone:clientA",
            "global",
        ] {
            assert_eq!(
                g.proxy_budget(scope).unwrap().unwrap().spent_tokens,
                50,
                "scope {scope}"
            );
        }
    }
}
