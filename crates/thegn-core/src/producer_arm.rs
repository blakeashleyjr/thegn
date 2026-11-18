//! Which headless producers the daemon arms (THE-2208).
//!
//! A kind is armed only when `[daemon] enabled` and `headless_producers` are
//! on AND something needs it (an enabled rule, autopilot, the PR queue, a
//! mention consumer). Arming is by need: with nothing that needs a producer
//! the daemon starts no task, no timer and writes no lease.
//!
//! Pure: the daemon gathers [`ArmInputs`] off-loop and calls [`armed_kinds`].

use crate::automation::{AutomationEventKind, AutomationRule};
use crate::notification::NotificationKind;
use crate::producer_lease::ProducerKind;

/// The tracker producer's cadence: the UI's issue refresh interval. The host
/// names this constant instead of keeping its own copy.
pub const TRACKER_CADENCE_SECS: u64 = 60;

/// The floor on `[watch] pr_interval_secs` for the PR poll and mentions.
pub const PR_POLL_FLOOR_SECS: u64 = 30;

/// The floor on `[pr_queue] poll_interval_secs`.
pub const PR_QUEUE_FLOOR_SECS: u64 = 15;

/// Everything [`armed`] decides from, gathered by the caller.
pub struct ArmInputs<'a> {
    /// `[daemon] enabled`.
    pub daemon_enabled: bool,
    /// `[daemon] headless_producers`.
    pub headless: bool,
    /// Every automation rule (config and Lead-authored).
    pub rules: &'a [crate::automation::AutomationRule],
    /// Any repo has `[autopilot]` enabled.
    pub autopilot_any: bool,
    /// `[pr_queue] enabled`.
    pub pr_queue_enabled: bool,
    /// `[notifications] github_mentions`.
    pub github_mentions: bool,
    /// A push topic is configured.
    pub push_configured: bool,
    /// `[inbox] enabled` with at least one enabled account of an implemented
    /// kind ([`inbox_armable`]). The inbox needs no rule: its items are for
    /// the owner to see.
    pub inbox_any: bool,
}

/// True when `[inbox]` has something to poll: the section is enabled and at
/// least one enabled account is of an implemented kind.
pub fn inbox_armable(cfg: &crate::config::Config) -> bool {
    cfg.inbox.enabled && cfg.inbox.implemented_enabled().next().is_some()
}

/// True when the daemon should run `kind`'s producer: the master switches
/// (`[daemon] enabled` and `headless_producers`) are both on AND something
/// needs this kind. Disabled rules never arm anything.
pub fn armed(kind: ProducerKind, i: &ArmInputs<'_>) -> bool {
    if !(i.daemon_enabled && i.headless) {
        return false;
    }
    let mut enabled = i.rules.iter().filter(|rule| rule.enabled);
    match kind {
        ProducerKind::Tracker => {
            i.autopilot_any || enabled.any(|r| r.event == AutomationEventKind::IssueMatched)
        }
        ProducerKind::PrPoll => enabled.any(needs_pr_poll),
        ProducerKind::Mentions => {
            i.github_mentions && (i.push_configured || enabled.any(needs_mentions))
        }
        ProducerKind::PrQueue => i.pr_queue_enabled,
        ProducerKind::Inbox => i.inbox_any,
    }
}

/// A rule fed by the PR poll: PR checks, review requests, landings, or a
/// notification rule on a PR kind (or on any kind).
fn needs_pr_poll(rule: &AutomationRule) -> bool {
    match rule.event {
        AutomationEventKind::PrChecks
        | AutomationEventKind::PrReviewRequested
        | AutomationEventKind::MergeLanded => true,
        AutomationEventKind::Notification => matches!(
            rule.predicate.notification_kind,
            None | Some(NotificationKind::PrStateChanged | NotificationKind::PrLinked)
        ),
        _ => false,
    }
}

/// A notification rule a forge mention can match (`mentioned`, or any kind).
fn needs_mentions(rule: &AutomationRule) -> bool {
    rule.event == AutomationEventKind::Notification
        && matches!(
            rule.predicate.notification_kind,
            None | Some(NotificationKind::Mentioned)
        )
}

/// Every armed kind, in [`ProducerKind::ALL`] order.
pub fn armed_kinds(i: &ArmInputs<'_>) -> Vec<ProducerKind> {
    ProducerKind::ALL
        .into_iter()
        .filter(|kind| armed(*kind, i))
        .collect()
}

/// How often `kind`'s producer ticks, in seconds. Every cadence is floored,
/// so a `0` in config can never spin a poll loop against a forge.
pub fn cadence_secs(kind: ProducerKind, cfg: &crate::config::Config) -> u64 {
    match kind {
        ProducerKind::Tracker => TRACKER_CADENCE_SECS,
        ProducerKind::PrPoll | ProducerKind::Mentions => {
            cfg.watch.pr_interval_secs.max(PR_POLL_FLOOR_SECS)
        }
        ProducerKind::PrQueue => cfg.pr_queue.poll_interval_secs.max(PR_QUEUE_FLOOR_SECS),
        ProducerKind::Inbox => inbox_cadence_secs(&cfg.inbox),
    }
}

/// The inbox producer ticks at its most frequent account's cadence (each
/// account's own cadence is floored at [`crate::config_inbox::MIN_POLL_SECS`]);
/// with no pollable account it falls back to the floored global interval.
fn inbox_cadence_secs(inbox: &crate::config::InboxConfig) -> u64 {
    inbox
        .implemented_enabled()
        .map(|account| account.effective_poll_secs(inbox.poll_interval_secs))
        .min()
        .unwrap_or_else(|| {
            inbox
                .poll_interval_secs
                .max(crate::config_inbox::MIN_POLL_SECS)
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::automation::{ActionTemplate, AutomationPredicate};
    use crate::automation_autonomy::Autonomy;

    fn rule(event: AutomationEventKind, kind: Option<NotificationKind>) -> AutomationRule {
        AutomationRule {
            id: "r".into(),
            enabled: true,
            event,
            predicate: AutomationPredicate {
                notification_kind: kind,
                ..AutomationPredicate::default()
            },
            action: ActionTemplate {
                cap: "notify.push".into(),
                params: BTreeMap::new(),
            },
            debounce_secs: 0,
            idle_secs: None,
            once_per_key: false,
            max_per_hour: 30,
            max_action_per_hour: 30,
            schedule: None,
            autonomy: Autonomy::Review,
        }
    }

    fn on(rules: &[AutomationRule]) -> ArmInputs<'_> {
        ArmInputs {
            daemon_enabled: true,
            headless: true,
            rules,
            autopilot_any: false,
            pr_queue_enabled: false,
            github_mentions: false,
            push_configured: false,
            inbox_any: false,
        }
    }

    fn with(rules: &[AutomationRule], push: bool) -> ArmInputs<'_> {
        ArmInputs {
            github_mentions: true,
            push_configured: push,
            ..on(rules)
        }
    }

    fn kinds(i: &ArmInputs<'_>) -> Vec<ProducerKind> {
        armed_kinds(i)
    }

    #[test]
    fn nothing_arms_with_everything_off() {
        let inputs = ArmInputs {
            daemon_enabled: false,
            headless: false,
            rules: &[],
            autopilot_any: false,
            pr_queue_enabled: false,
            github_mentions: false,
            push_configured: false,
            inbox_any: false,
        };
        assert!(armed_kinds(&inputs).is_empty());
    }

    #[test]
    fn nothing_arms_with_the_masters_on_and_no_need() {
        assert!(kinds(&on(&[])).is_empty(), "arming is by need");
    }

    #[test]
    fn nothing_arms_with_either_master_off_whatever_needs_it() {
        let rules = [
            rule(AutomationEventKind::IssueMatched, None),
            rule(AutomationEventKind::PrChecks, None),
            rule(AutomationEventKind::Notification, None),
        ];
        let everything = ArmInputs {
            autopilot_any: true,
            pr_queue_enabled: true,
            github_mentions: true,
            push_configured: true,
            inbox_any: true,
            ..on(&rules)
        };
        assert_eq!(kinds(&everything), ProducerKind::ALL.to_vec());
        let daemon_off = ArmInputs {
            daemon_enabled: false,
            ..everything
        };
        assert!(kinds(&daemon_off).is_empty(), "[daemon] enabled off");
        let master_off = ArmInputs {
            daemon_enabled: true,
            headless: false,
            ..daemon_off
        };
        assert!(kinds(&master_off).is_empty(), "headless_producers off");
    }

    #[test]
    fn tracker_arms_on_an_issue_matched_rule_or_autopilot() {
        let rules = [rule(AutomationEventKind::IssueMatched, None)];
        assert_eq!(kinds(&on(&rules)), [ProducerKind::Tracker]);
        let autopilot = ArmInputs {
            autopilot_any: true,
            ..on(&[])
        };
        assert_eq!(kinds(&autopilot), [ProducerKind::Tracker]);
        let mut disabled = rules.clone();
        disabled[0].enabled = false;
        assert!(
            kinds(&on(&disabled)).is_empty(),
            "a disabled rule arms nothing"
        );
    }

    #[test]
    fn pr_poll_arms_on_pr_rules_and_pr_notification_kinds() {
        for event in [
            AutomationEventKind::PrChecks,
            AutomationEventKind::PrReviewRequested,
            AutomationEventKind::MergeLanded,
        ] {
            let rules = [rule(event, None)];
            assert_eq!(kinds(&on(&rules)), [ProducerKind::PrPoll], "{event:?}");
        }
        for kind in [
            None,
            Some(NotificationKind::PrStateChanged),
            Some(NotificationKind::PrLinked),
        ] {
            let rules = [rule(AutomationEventKind::Notification, kind)];
            assert!(armed(ProducerKind::PrPoll, &on(&rules)), "{kind:?}");
        }
        let rules = [rule(
            AutomationEventKind::Notification,
            Some(NotificationKind::TestFailed),
        )];
        assert!(!armed(ProducerKind::PrPoll, &on(&rules)));
        for event in [
            AutomationEventKind::AgentNeedsYou,
            AutomationEventKind::ScheduleDue,
            AutomationEventKind::IssueMatched,
        ] {
            let rules = [rule(event, None)];
            assert!(!armed(ProducerKind::PrPoll, &on(&rules)), "{event:?}");
        }
    }

    #[test]
    fn mentions_needs_github_mentions_and_a_consumer() {
        let mentioned = [rule(
            AutomationEventKind::Notification,
            Some(NotificationKind::Mentioned),
        )];
        assert!(
            !armed(ProducerKind::Mentions, &on(&mentioned)),
            "github_mentions off"
        );
        assert!(
            !armed(ProducerKind::Mentions, &with(&[], false)),
            "no consumer"
        );
        assert!(
            armed(ProducerKind::Mentions, &with(&[], true)),
            "push topic"
        );
        assert!(armed(ProducerKind::Mentions, &with(&mentioned, false)));
        let any_kind = [rule(AutomationEventKind::Notification, None)];
        assert!(armed(ProducerKind::Mentions, &with(&any_kind, false)));
        let other = [rule(
            AutomationEventKind::Notification,
            Some(NotificationKind::PrLinked),
        )];
        assert!(!armed(ProducerKind::Mentions, &with(&other, false)));
        let mut disabled = mentioned.clone();
        disabled[0].enabled = false;
        assert!(!armed(ProducerKind::Mentions, &with(&disabled, false)));
    }

    #[test]
    fn pr_queue_arms_on_its_switch_only() {
        let queue = ArmInputs {
            pr_queue_enabled: true,
            ..on(&[])
        };
        assert_eq!(kinds(&queue), [ProducerKind::PrQueue]);
    }

    #[test]
    fn inbox_arms_on_its_input_only_and_needs_an_implemented_account() {
        let inbox = ArmInputs {
            inbox_any: true,
            ..on(&[])
        };
        assert_eq!(kinds(&inbox), [ProducerKind::Inbox]);
        let mut cfg = crate::config::Config::default();
        assert!(!inbox_armable(&cfg), "off by default");
        cfg.inbox.enabled = true;
        assert!(!inbox_armable(&cfg), "no accounts");
        cfg.inbox.accounts.push(crate::config::InboxAccount {
            name: "chat".into(),
            kind: crate::config::InboxKind::Slack,
            ..Default::default()
        });
        assert!(!inbox_armable(&cfg), "a reserved kind arms nothing");
        cfg.inbox.accounts.push(crate::config::InboxAccount {
            name: "work".into(),
            enabled: false,
            ..Default::default()
        });
        assert!(!inbox_armable(&cfg), "a disabled account arms nothing");
        cfg.inbox.accounts[1].enabled = true;
        assert!(inbox_armable(&cfg));
        cfg.inbox.enabled = false;
        assert!(!inbox_armable(&cfg), "the section switch wins");
    }

    #[test]
    fn inbox_cadence_is_the_fastest_account_and_floored() {
        let mut cfg = crate::config::Config::default();
        assert_eq!(cadence_secs(ProducerKind::Inbox, &cfg), 300);
        cfg.inbox.poll_interval_secs = 0;
        assert_eq!(cadence_secs(ProducerKind::Inbox, &cfg), 60);
        cfg.inbox.poll_interval_secs = 600;
        cfg.inbox.accounts = vec![
            crate::config::InboxAccount {
                name: "a".into(),
                poll_interval_secs: 120,
                ..Default::default()
            },
            crate::config::InboxAccount {
                name: "b".into(),
                ..Default::default()
            },
        ];
        assert_eq!(cadence_secs(ProducerKind::Inbox, &cfg), 120);
        cfg.inbox.accounts[0].poll_interval_secs = 5;
        assert_eq!(cadence_secs(ProducerKind::Inbox, &cfg), 60, "floored");
    }

    #[test]
    fn cadences_are_floored() {
        let mut cfg = crate::config::Config::default();
        assert_eq!(
            cadence_secs(ProducerKind::Tracker, &cfg),
            TRACKER_CADENCE_SECS
        );
        cfg.watch.pr_interval_secs = 0;
        cfg.pr_queue.poll_interval_secs = 0;
        assert_eq!(cadence_secs(ProducerKind::PrPoll, &cfg), 30);
        assert_eq!(cadence_secs(ProducerKind::Mentions, &cfg), 30);
        assert_eq!(cadence_secs(ProducerKind::PrQueue, &cfg), 15);
        cfg.watch.pr_interval_secs = 120;
        cfg.pr_queue.poll_interval_secs = 90;
        assert_eq!(cadence_secs(ProducerKind::PrPoll, &cfg), 120);
        assert_eq!(cadence_secs(ProducerKind::Mentions, &cfg), 120);
        assert_eq!(cadence_secs(ProducerKind::PrQueue, &cfg), 90);
        for kind in ProducerKind::ALL {
            assert!(cadence_secs(kind, &cfg) > 0);
        }
    }
}
