//! Hot-reload reconcile: old effective instance set × new → a per-instance
//! plan of start / stop / restart / refilter actions.
//!
//! Applying config changes to running upstreams is *applying this plan*, and
//! the diff is exhaustively unit-testable so the supervisor stays a dumb
//! executor. The key insight that keeps it cheap: an exposure-only change
//! (`proxy.tools` edited) is a **refilter** — the running upstream child keeps
//! running and only the advertised/routable tool set changes; a launch-spec
//! change (argv/env) is a **restart**.

use std::collections::BTreeMap;

/// A resolved upstream instance the supervisor should be running for one
/// partition key. Carries env **refs** (`env:`/`file:`/`keyring:`), never
/// resolved secret values — resolution happens at spawn in the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpec {
    pub upstream: String,
    pub partition_key: String,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The `proxy.tools` glob list (exposure policy) for this instance.
    pub exposure: Vec<String>,
}

impl InstanceSpec {
    /// The identity a supervisor keys an instance on: (upstream, partition key).
    /// Two config revisions describe "the same instance" iff these match.
    pub fn key(&self) -> (String, String) {
        (self.upstream.clone(), self.partition_key.clone())
    }

    /// Whether the launch spec (argv + env) differs — i.e. a restart is needed
    /// rather than a mere refilter.
    fn launch_differs(&self, other: &InstanceSpec) -> bool {
        self.argv != other.argv || self.env != other.env
    }
}

/// One reconcile action against the running instance set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Spawn a new upstream instance.
    Start(InstanceSpec),
    /// Kill and forget an instance no longer in the config.
    Stop {
        upstream: String,
        partition_key: String,
    },
    /// The launch spec changed — kill and re-spawn with the new spec.
    Restart(InstanceSpec),
    /// Only the exposure (`proxy.tools`) changed — keep the child, swap the
    /// filter (no process churn, no lost warm state).
    Refilter(InstanceSpec),
}

impl ReconcileAction {
    pub fn kind(&self) -> &'static str {
        match self {
            ReconcileAction::Start(_) => "start",
            ReconcileAction::Stop { .. } => "stop",
            ReconcileAction::Restart(_) => "restart",
            ReconcileAction::Refilter(_) => "refilter",
        }
    }
}

/// Diff the currently-running instances (`old`) against the desired set
/// (`new`). Deterministic: actions are ordered by instance key, with stops
/// last so a rename (stop old + start new) frees resources predictably. An
/// identical instance yields no action.
pub fn reconcile(old: &[InstanceSpec], new: &[InstanceSpec]) -> Vec<ReconcileAction> {
    let old_map: BTreeMap<(String, String), &InstanceSpec> =
        old.iter().map(|s| (s.key(), s)).collect();
    let new_map: BTreeMap<(String, String), &InstanceSpec> =
        new.iter().map(|s| (s.key(), s)).collect();

    let mut actions = Vec::new();

    // Starts / restarts / refilters, in key order.
    for (key, next) in &new_map {
        match old_map.get(key) {
            None => actions.push(ReconcileAction::Start((*next).clone())),
            Some(prev) => {
                if prev.launch_differs(next) {
                    actions.push(ReconcileAction::Restart((*next).clone()));
                } else if prev.exposure != next.exposure {
                    actions.push(ReconcileAction::Refilter((*next).clone()));
                }
                // else: identical — no action.
            }
        }
    }

    // Stops, in key order, after the starts.
    for (key, prev) in &old_map {
        if !new_map.contains_key(key) {
            actions.push(ReconcileAction::Stop {
                upstream: prev.upstream.clone(),
                partition_key: key.1.clone(),
            });
        }
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, key: &str, argv: &[&str], exposure: &[&str]) -> InstanceSpec {
        InstanceSpec {
            upstream: name.to_string(),
            partition_key: key.to_string(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            exposure: exposure.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn with_env(mut s: InstanceSpec, k: &str, v: &str) -> InstanceSpec {
        s.env.insert(k.to_string(), v.to_string());
        s
    }

    #[test]
    fn empty_to_empty_is_no_op() {
        assert!(reconcile(&[], &[]).is_empty());
    }

    #[test]
    fn added_upstream_starts() {
        let new = [spec("git", "global", &["git-mcp"], &["*"])];
        let actions = reconcile(&[], &new);
        assert_eq!(actions, vec![ReconcileAction::Start(new[0].clone())]);
    }

    #[test]
    fn removed_upstream_stops() {
        let old = [spec("git", "global", &["git-mcp"], &["*"])];
        let actions = reconcile(&old, &[]);
        assert_eq!(
            actions,
            vec![ReconcileAction::Stop {
                upstream: "git".into(),
                partition_key: "global".into()
            }]
        );
    }

    #[test]
    fn changed_argv_restarts() {
        let old = [spec("git", "global", &["git-mcp"], &["*"])];
        let new = [spec("git", "global", &["git-mcp", "--v2"], &["*"])];
        let actions = reconcile(&old, &new);
        assert_eq!(actions, vec![ReconcileAction::Restart(new[0].clone())]);
    }

    #[test]
    fn changed_env_restarts() {
        let old = [with_env(
            spec("git", "global", &["git-mcp"], &["*"]),
            "TOKEN",
            "keyring:a",
        )];
        let new = [with_env(
            spec("git", "global", &["git-mcp"], &["*"]),
            "TOKEN",
            "keyring:b",
        )];
        let actions = reconcile(&old, &new);
        assert_eq!(actions, vec![ReconcileAction::Restart(new[0].clone())]);
    }

    #[test]
    fn changed_exposure_only_refilters_no_restart() {
        let old = [spec("git", "global", &["git-mcp"], &["read_*"])];
        let new = [spec("git", "global", &["git-mcp"], &["read_*", "search"])];
        let actions = reconcile(&old, &new);
        assert_eq!(actions, vec![ReconcileAction::Refilter(new[0].clone())]);
    }

    #[test]
    fn identical_instance_is_no_op() {
        let s = [spec("git", "global", &["git-mcp"], &["*"])];
        assert!(reconcile(&s, &s).is_empty());
    }

    #[test]
    fn partition_change_is_start_plus_stop_not_restart() {
        // Same upstream, different partition key ⇒ a different instance.
        let old = [spec("mem", "workspace:a", &["mem"], &["*"])];
        let new = [spec("mem", "workspace:b", &["mem"], &["*"])];
        let actions = reconcile(&old, &new);
        assert_eq!(
            actions,
            vec![
                ReconcileAction::Start(new[0].clone()),
                ReconcileAction::Stop {
                    upstream: "mem".into(),
                    partition_key: "workspace:a".into()
                },
            ]
        );
    }

    #[test]
    fn mixed_plan_is_deterministic_starts_then_stops() {
        let old = [
            spec("a", "global", &["a"], &["*"]),     // kept identical
            spec("b", "global", &["b"], &["*"]),     // removed
            spec("c", "global", &["c-old"], &["*"]), // restarted
        ];
        let new = [
            spec("a", "global", &["a"], &["*"]),
            spec("c", "global", &["c-new"], &["*"]),
            spec("d", "global", &["d"], &["*"]), // added
        ];
        let actions = reconcile(&old, &new);
        // new keys in order: (a)=noop, (c)=restart, (d)=start; then stops: (b).
        assert_eq!(
            actions,
            vec![
                ReconcileAction::Restart(new[1].clone()),
                ReconcileAction::Start(new[2].clone()),
                ReconcileAction::Stop {
                    upstream: "b".into(),
                    partition_key: "global".into()
                },
            ]
        );
    }

    #[test]
    fn action_kind_labels() {
        assert_eq!(
            ReconcileAction::Start(spec("a", "global", &["a"], &["*"])).kind(),
            "start"
        );
        assert_eq!(
            ReconcileAction::Stop {
                upstream: "a".into(),
                partition_key: "global".into()
            }
            .kind(),
            "stop"
        );
        assert_eq!(
            ReconcileAction::Restart(spec("a", "global", &["a"], &["*"])).kind(),
            "restart"
        );
        assert_eq!(
            ReconcileAction::Refilter(spec("a", "global", &["a"], &["*"])).kind(),
            "refilter"
        );
    }
}
