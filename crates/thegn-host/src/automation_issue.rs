//! The `issue_matched` producer edge (THE-2194, filled by b153 lane 3).
//!
//! Called from the existing tracker-refresh background job, and only when an
//! enabled `issue_matched` rule exists (config or DB). It diffs the cached and
//! fresh issue lists with `thegn_core::automation_issue::matched_candidates`,
//! projects facts (`facts_from_issue`) and submits `issue_event`s through
//! `automation_runtime::submit`, at most
//! `automation_issue::MAX_ISSUE_EVENTS_PER_REFRESH` per refresh. Phase 3 moves
//! the call site into the daemon unchanged.

use thegn_core::automation::{AutomationEventKind, AutomationRule};
use thegn_core::config::Config;
use thegn_core::issue::Issue;

/// One tracker refresh's inputs.
pub(crate) struct IssueRefresh<'a> {
    pub cfg: &'a Config,
    pub repo_key: &'a str,
    pub provider: &'a str,
    pub account: &'a str,
    pub assignee_me: bool,
    pub had_cache: bool,
}

/// True when any enabled `issue_matched` rule exists.
fn wanted(config_rules: &[AutomationRule], lead_rules: &[AutomationRule]) -> bool {
    config_rules
        .iter()
        .chain(lead_rules)
        .any(|r| r.enabled && r.event == AutomationEventKind::IssueMatched)
}

/// Submit an `issue_matched` event for each matched candidate.
pub(crate) fn emit_matched(
    db: &thegn_core::db::Db,
    ctx: &IssueRefresh<'_>,
    old: &[Issue],
    new: &[Issue],
) {
    use thegn_core::automation_issue as core;
    use thegn_core::store::AutomationRuleStore;
    if !ctx.had_cache {
        return;
    }
    let settings = ctx.cfg.effective_automations();
    if !settings.enabled {
        return;
    }
    let config_rules = settings.compiled_rules().unwrap_or_default();
    let lead_rules = match db.lead_rules() {
        Ok(rows) if !rows.is_empty() => {
            let names: Vec<String> = config_rules.iter().map(|r| r.id.clone()).collect();
            thegn_core::automation_authoring::compile_lead_rows(&rows, &names).0
        }
        _ => Vec::new(),
    };
    if !wanted(&config_rules, &lead_rules) {
        return;
    }
    let overflow = core::overflow_count(old, new, ctx.had_cache);
    if overflow > 0 {
        tracing::info!(
            target: "thegn::automation",
            overflow,
            "issue_matched candidates past the per-refresh cap are dropped"
        );
    }
    let now = thegn_core::util::now();
    for issue in core::matched_candidates(old, new, ctx.had_cache) {
        let facts = core::facts_from_issue(issue, ctx.account, ctx.assignee_me);
        crate::automation_runtime::submit(core::issue_event(facts, ctx.repo_key, now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wanted_requires_an_enabled_issue_matched_rule() {
        use thegn_core::automation::{ActionTemplate, AutomationPredicate};
        let rule = |event, enabled| AutomationRule {
            id: "r".into(),
            enabled,
            event,
            predicate: AutomationPredicate::default(),
            action: ActionTemplate {
                cap: "notify.push".into(),
                params: Default::default(),
            },
            debounce_secs: 0,
            idle_secs: None,
            once_per_key: true,
            max_per_hour: 1,
            max_action_per_hour: 1,
            schedule: None,
            autonomy: Default::default(),
        };
        assert!(!wanted(&[], &[]));
        assert!(!wanted(
            &[rule(AutomationEventKind::IssueMatched, false)],
            &[]
        ));
        assert!(!wanted(
            &[rule(AutomationEventKind::MergeLanded, true)],
            &[]
        ));
        assert!(wanted(
            &[],
            &[rule(AutomationEventKind::IssueMatched, true)]
        ));
        assert!(wanted(
            &[rule(AutomationEventKind::IssueMatched, true)],
            &[]
        ));
    }
}
