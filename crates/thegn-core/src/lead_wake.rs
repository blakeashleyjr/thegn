//! `lead.wake`: an automation rule waking the Lead with a bounded event
//! (THE-2197, filled by b153 lane 6).
//!
//! The action runs a headless Lead turn whose prompt is the rule's `prompt`
//! template, rendered against [`crate::automation::LEAD_WAKE_TEMPLATE_VARS`]
//! (never `{message}`). Untrusted event text (notification bodies, issue
//! titles, inbox items) reaches the Lead only inside a bounded, delimited
//! event block appended after that prompt: data, never instructions, never
//! argv, env, a path, a branch, a scope, a rule or an autonomy level.
//! `lead.wake` is a rule-only action (`automation::RULE_ONLY_ACTIONS`): no
//! MCP tool, HTTP route or Lead tool reaches it except through a rule.
