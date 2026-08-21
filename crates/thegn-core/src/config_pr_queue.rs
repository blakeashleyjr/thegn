//! The `[pr_queue]` config family — babysitting pull requests on a forge.
//!
//! The team-mode counterpart to `[merge_queue]`. Where the merge queue folds
//! local branches onto a local target, this one watches **remote** pull
//! requests: poll their state, classify what is blocking them, optionally hand a
//! blocker to a configured agent, and let the forge merge them once green.
//!
//! Kept in a sibling module rather than the god-file `config.rs`; `config.rs`
//! re-exports everything here.
//!
//! Every default is chosen so that turning the feature on does the *least*
//! surprising thing on a shared repo: nothing is enqueued automatically, nothing
//! is merged by thegn itself, and no agent writes to a pull request you did not
//! author.

use serde::{Deserialize, Serialize};

use crate::config::{config_enum, config_warn};

config_enum! {
    /// `[pr_queue] merge_mode` — what happens when a queued PR is finally green.
    ///
    /// `"auto_merge"` (the default) hands the merge to the **forge**, so branch
    /// protection, required reviews, and any server-side merge queue stay
    /// authoritative — thegn's view of "ready" can never race them. `"thegn"`
    /// merges directly. `"ready"` never merges; it just tells you.
    pub enum PrMergeMode : "PR merge mode" {
        AutoMerge = "auto_merge" | "auto" | "forge",
        Thegn     = "thegn" | "direct",
        Ready     = "ready" | "off" | "notify",
    } default = AutoMerge;
}

config_enum! {
    /// `[pr_queue] merge_method` — how the forge should combine the commits.
    pub enum PrMergeMethod : "PR merge method" {
        Squash = "squash",
        Merge  = "merge" | "commit",
        Rebase = "rebase",
    } default = Squash;
}

config_enum! {
    /// `[pr_queue] auto_enqueue` — whether opening a PR queues it automatically.
    /// Off by default: enqueueing is a deliberate act, because a queued PR is
    /// one thegn may write to.
    pub enum PrAutoEnqueue : "PR auto-enqueue" {
        Off      = "off" | "none" | "false",
        Worktree = "worktree" | "on" | "true",
    } default = Off;
}

config_enum! {
    /// A blocker class the agent may be woken for (`[pr_queue] watch`).
    pub enum PrWatchKind : "PR watch kind" {
        Ci       = "ci" | "checks",
        Conflict = "conflict" | "base",
        Review   = "review" | "comments",
    } default = Ci;
}

/// `[pr_queue]` — watch queued pull requests on the forge, optionally fixing
/// blockers with an agent, and merge them once they are green.
///
/// **Off by default.** This is the one part of the shell that makes network
/// *writes*, so it is opt-in.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PrQueueConfig {
    /// Master switch. When off there is no polling, no command, no panel
    /// section, and no badge.
    pub enabled: bool,
    /// How often a queued PR's remote state is refreshed, in seconds. Clamped to
    /// a floor at use so a typo can't hammer the forge's rate limit.
    pub poll_interval_secs: u64,
    /// Whether opening a PR from a worktree queues it automatically.
    pub auto_enqueue: PrAutoEnqueue,
    /// Which blocker classes may wake the agent. A blocker whose kind is absent
    /// here is still tracked and displayed — it just never gets an agent.
    pub watch: Vec<PrWatchKind>,
    /// A configured `[[agents]]`/`[[tools]]` entry to run headlessly. Its
    /// provider supplies the non-interactive flags. `agent_command` overrides
    /// this; with neither set, blockers are reported but never fixed.
    pub agent: String,
    /// Full command template for the agent, overriding `agent`. Placeholders are
    /// shell-quoted, so write them **bare** (`claude -p {prompt}`).
    pub agent_command: String,
    /// Agent-dispatch cycles per PR before it is marked `needs_human`.
    pub agent_max_attempts: u32,
    /// Watchdog (seconds) for one agent invocation. 0 disables it.
    pub agent_timeout_secs: u64,
    /// Refill the attempt budget when the PR's head commit changes.
    ///
    /// On by default, and the reason is structural: a pull request lives for
    /// days, so the merge queue's one-shot budget would exhaust once and leave
    /// the row stuck forever. Only a head thegn did **not** create refills it, so
    /// an agent cannot top up its own budget by pushing.
    pub reset_attempts_on_push: bool,
    /// Stop and ask for a human when the PR's head moved and thegn didn't move
    /// it — someone else pushed. Prevents an agent from racing a teammate.
    pub pause_on_foreign_push: bool,
    /// Never let the agent write to a pull request you did not author. Watching
    /// and displaying someone else's PR is still fine.
    pub own_prs_only: bool,
    /// What to do with a PR that is finally green.
    pub merge_mode: PrMergeMode,
    /// Merge strategy requested of the forge.
    pub merge_method: PrMergeMethod,
    /// Ask the forge to delete the head branch after a successful merge.
    pub delete_branch_on_merge: bool,
    /// Hold a PR until its review decision is approving. On by default: merging
    /// unreviewed work is exactly the thing a team does not want automated.
    pub require_approval: bool,
    /// Hold a PR until every check has passed.
    pub require_checks: bool,
    /// File a queued PR's worktree into sidebar folders as it moves, reusing the
    /// merge queue's lifecycle machinery.
    pub organize_folders: bool,
    /// Folder for a worktree whose PR is queued. Empty ⇒ don't file.
    pub queued_folder: String,
    /// Folder for a PR that needs a human. Empty ⇒ leave it be.
    pub failed_folder: String,
    /// `[pr_queue.prompts]` — what the agent is told, per blocker kind.
    pub prompts: PrQueuePrompts,
}

impl Default for PrQueueConfig {
    fn default() -> Self {
        PrQueueConfig {
            enabled: false,
            poll_interval_secs: 60,
            auto_enqueue: PrAutoEnqueue::Off,
            // All three watched by default *if* an agent is configured — the
            // agent keys are what actually arm this, and they are empty.
            watch: vec![PrWatchKind::Ci, PrWatchKind::Conflict, PrWatchKind::Review],
            agent: String::new(),
            agent_command: String::new(),
            agent_max_attempts: 2,
            agent_timeout_secs: 900,
            reset_attempts_on_push: true,
            pause_on_foreign_push: true,
            own_prs_only: true,
            merge_mode: PrMergeMode::AutoMerge,
            merge_method: PrMergeMethod::Squash,
            delete_branch_on_merge: true,
            require_approval: true,
            require_checks: true,
            organize_folders: true,
            queued_folder: "In review".to_string(),
            failed_folder: "Needs attention".to_string(),
            prompts: PrQueuePrompts::default(),
        }
    }
}

impl PrQueueConfig {
    /// The poll cadence, floored so a misconfigured `0` cannot spin against the
    /// forge's rate limit.
    pub fn poll_secs(&self) -> u64 {
        self.poll_interval_secs.max(15)
    }

    /// Whether this blocker class is allowed to wake the agent.
    pub fn watches(&self, kind: PrWatchKind) -> bool {
        self.watch.contains(&kind)
    }
}

/// `[pr_queue.prompts]` — the task prompt handed to the agent, per blocker kind.
/// An empty template means "use thegn's built-in instructions", so unsetting a
/// key restores the default rather than sending an empty prompt.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PrQueuePrompts {
    /// Checks are red. Vars: `{branch}`, `{base}`, `{worktree}`, `{pr_number}`,
    /// `{pr_url}`, `{pr_title}`, `{checks}`, `{log}`.
    pub ci_failure: String,
    /// Conflicts with / behind the base. Vars as above, minus `{checks}`/`{log}`.
    pub conflict: String,
    /// Unresolved review feedback. Vars as above, with `{threads}`.
    pub review: String,
}

impl PrQueuePrompts {
    /// The configured template for a kind, or `None` to use the built-in.
    pub fn for_kind(&self, kind: crate::agent_task::TaskKind) -> Option<&str> {
        use crate::agent_task::TaskKind as K;
        let t = match kind {
            K::PrCiFailure => &self.ci_failure,
            K::PrConflict => &self.conflict,
            K::PrReview => &self.review,
            // The merge kinds belong to `[merge_queue.prompts]`.
            _ => return None,
        };
        (!t.trim().is_empty()).then_some(t.as_str())
    }

    /// The template actually used: the configured one, else the built-in.
    pub fn resolve(&self, kind: crate::agent_task::TaskKind) -> &str {
        self.for_kind(kind)
            .unwrap_or_else(|| crate::agent_task::default_prompt(kind))
    }
}

/// An `Option`-per-field overlay of `[pr_queue]`, for the `[workspace.<slug>]`
/// layer. A repo's base branch conventions, merge method, and review rules are
/// repository facts, exactly like `merge_queue.gate_command`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PrQueueOverlay {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_enqueue: Option<PrAutoEnqueue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<Vec<PrWatchKind>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_max_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_attempts_on_push: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause_on_foreign_push: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub own_prs_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_mode: Option<PrMergeMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_method: Option<PrMergeMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_branch_on_merge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_approval: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_checks: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organize_folders: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_folder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_folder: Option<String>,
    /// Merged field-wise, like `[merge_queue.prompts]`.
    #[serde(skip_serializing_if = "PrQueuePromptsOverlay::is_empty")]
    pub prompts: PrQueuePromptsOverlay,
}

/// The `Option`-per-field overlay of `[pr_queue.prompts]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PrQueuePromptsOverlay {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
}

impl PrQueuePromptsOverlay {
    pub fn is_empty(&self) -> bool {
        self.ci_failure.is_none() && self.conflict.is_none() && self.review.is_none()
    }

    /// Apply present fields onto `base`. Exhaustively destructured on purpose.
    pub fn apply(self, base: &mut PrQueuePrompts) {
        let PrQueuePromptsOverlay {
            ci_failure,
            conflict,
            review,
        } = self;
        if let Some(v) = ci_failure {
            base.ci_failure = v;
        }
        if let Some(v) = conflict {
            base.conflict = v;
        }
        if let Some(v) = review {
            base.review = v;
        }
    }
}

impl PrQueueOverlay {
    /// True when nothing is set (lets the carrying struct skip serialization).
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.poll_interval_secs.is_none()
            && self.auto_enqueue.is_none()
            && self.watch.is_none()
            && self.agent.is_none()
            && self.agent_command.is_none()
            && self.agent_max_attempts.is_none()
            && self.agent_timeout_secs.is_none()
            && self.reset_attempts_on_push.is_none()
            && self.pause_on_foreign_push.is_none()
            && self.own_prs_only.is_none()
            && self.merge_mode.is_none()
            && self.merge_method.is_none()
            && self.delete_branch_on_merge.is_none()
            && self.require_approval.is_none()
            && self.require_checks.is_none()
            && self.organize_folders.is_none()
            && self.queued_folder.is_none()
            && self.failed_folder.is_none()
            && self.prompts.is_empty()
    }

    /// Apply present fields onto `base` (present wins, absent inherits).
    ///
    /// Destructured exhaustively on purpose — no `..`. A field added to
    /// `PrQueueConfig` later must fail to compile here rather than be silently
    /// dropped from the per-repo layer.
    pub fn apply(self, base: &mut PrQueueConfig) {
        let PrQueueOverlay {
            enabled,
            poll_interval_secs,
            auto_enqueue,
            watch,
            agent,
            agent_command,
            agent_max_attempts,
            agent_timeout_secs,
            reset_attempts_on_push,
            pause_on_foreign_push,
            own_prs_only,
            merge_mode,
            merge_method,
            delete_branch_on_merge,
            require_approval,
            require_checks,
            organize_folders,
            queued_folder,
            failed_folder,
            prompts,
        } = self;
        prompts.apply(&mut base.prompts);
        if let Some(v) = enabled {
            base.enabled = v;
        }
        if let Some(v) = poll_interval_secs {
            base.poll_interval_secs = v;
        }
        if let Some(v) = auto_enqueue {
            base.auto_enqueue = v;
        }
        if let Some(v) = watch {
            base.watch = v;
        }
        if let Some(v) = agent {
            base.agent = v;
        }
        if let Some(v) = agent_command {
            base.agent_command = v;
        }
        if let Some(v) = agent_max_attempts {
            base.agent_max_attempts = v;
        }
        if let Some(v) = agent_timeout_secs {
            base.agent_timeout_secs = v;
        }
        if let Some(v) = reset_attempts_on_push {
            base.reset_attempts_on_push = v;
        }
        if let Some(v) = pause_on_foreign_push {
            base.pause_on_foreign_push = v;
        }
        if let Some(v) = own_prs_only {
            base.own_prs_only = v;
        }
        if let Some(v) = merge_mode {
            base.merge_mode = v;
        }
        if let Some(v) = merge_method {
            base.merge_method = v;
        }
        if let Some(v) = delete_branch_on_merge {
            base.delete_branch_on_merge = v;
        }
        if let Some(v) = require_approval {
            base.require_approval = v;
        }
        if let Some(v) = require_checks {
            base.require_checks = v;
        }
        if let Some(v) = organize_folders {
            base.organize_folders = v;
        }
        if let Some(v) = queued_folder {
            base.queued_folder = v;
        }
        if let Some(v) = failed_folder {
            base.failed_folder = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::TaskKind;

    #[test]
    fn defaults_are_safe_for_a_shared_repo() {
        let d = PrQueueConfig::default();
        // Off entirely — this is the one part of the shell that writes to a forge.
        assert!(!d.enabled);
        // Nothing is enqueued behind your back...
        assert_eq!(d.auto_enqueue, PrAutoEnqueue::Off);
        // ...thegn does not merge anything itself...
        assert_eq!(d.merge_mode, PrMergeMode::AutoMerge);
        // ...unreviewed or red work is held...
        assert!(d.require_approval);
        assert!(d.require_checks);
        // ...someone else's PR is never written to...
        assert!(d.own_prs_only);
        // ...and a teammate's push stops the agent rather than racing it.
        assert!(d.pause_on_foreign_push);
        // No agent configured ⇒ blockers are reported, never fixed.
        assert_eq!(d.agent, "");
        assert_eq!(d.agent_command, "");
    }

    #[test]
    fn poll_interval_is_floored_against_rate_limits() {
        let mut c = PrQueueConfig::default();
        assert_eq!(c.poll_secs(), 60);
        c.poll_interval_secs = 0;
        assert_eq!(c.poll_secs(), 15, "a 0 must not spin against the forge");
        c.poll_interval_secs = 5;
        assert_eq!(c.poll_secs(), 15);
        c.poll_interval_secs = 600;
        assert_eq!(c.poll_secs(), 600, "a long interval is honored as-is");
    }

    #[test]
    fn watch_list_gates_each_blocker_class() {
        let mut c = PrQueueConfig::default();
        for k in [PrWatchKind::Ci, PrWatchKind::Conflict, PrWatchKind::Review] {
            assert!(c.watches(k), "{k} watched by default");
        }
        c.watch = vec![PrWatchKind::Ci];
        assert!(c.watches(PrWatchKind::Ci));
        assert!(!c.watches(PrWatchKind::Conflict));
        assert!(!c.watches(PrWatchKind::Review));
        c.watch = vec![];
        assert!(!c.watches(PrWatchKind::Ci), "an empty list watches nothing");
    }

    #[test]
    fn prompts_fall_back_to_builtins_and_ignore_merge_kinds() {
        let mut p = PrQueuePrompts::default();
        for k in [
            TaskKind::PrCiFailure,
            TaskKind::PrConflict,
            TaskKind::PrReview,
        ] {
            assert_eq!(p.for_kind(k), None);
            assert_eq!(p.resolve(k), crate::agent_task::default_prompt(k));
        }
        // This table has no opinion on the merge-queue kinds.
        assert_eq!(p.for_kind(TaskKind::MergeConflict), None);
        assert_eq!(p.for_kind(TaskKind::GateFailure), None);

        p.ci_failure = "mine".into();
        assert_eq!(p.resolve(TaskKind::PrCiFailure), "mine");
        // Whitespace-only is still "unset", so blanking a key restores the
        // default rather than sending an empty prompt.
        p.conflict = "  \n ".into();
        assert_eq!(
            p.resolve(TaskKind::PrConflict),
            crate::agent_task::default_prompt(TaskKind::PrConflict)
        );
    }

    #[test]
    fn overlay_applies_present_fields_and_inherits_absent_ones() {
        let mut base = PrQueueConfig::default();
        PrQueueOverlay {
            enabled: Some(true),
            merge_mode: Some(PrMergeMode::Thegn),
            require_approval: Some(false),
            ..PrQueueOverlay::default()
        }
        .apply(&mut base);
        assert!(base.enabled);
        assert_eq!(base.merge_mode, PrMergeMode::Thegn);
        assert!(!base.require_approval);
        // Absent keys inherit.
        assert!(base.own_prs_only);
        assert_eq!(base.merge_method, PrMergeMethod::Squash);
    }

    #[test]
    fn overlay_is_empty_only_when_nothing_is_set() {
        assert!(PrQueueOverlay::default().is_empty());
        // A `false` is still "set" — the overlay is Option-based precisely so a
        // repo can turn something OFF, which a truthiness check would lose.
        assert!(
            !PrQueueOverlay {
                own_prs_only: Some(false),
                ..PrQueueOverlay::default()
            }
            .is_empty()
        );
        assert!(
            !PrQueueOverlay {
                prompts: PrQueuePromptsOverlay {
                    review: Some("x".into()),
                    ..PrQueuePromptsOverlay::default()
                },
                ..PrQueueOverlay::default()
            }
            .is_empty(),
            "a prompts-only overlay is not empty"
        );
    }

    #[test]
    fn prompts_overlay_merges_field_wise() {
        // Field-wise, NOT wholesale: a repo overriding only the CI prompt keeps
        // the global conflict/review ones.
        let mut base = PrQueuePrompts {
            ci_failure: "g-ci".into(),
            conflict: "g-conflict".into(),
            review: "g-review".into(),
        };
        PrQueuePromptsOverlay {
            ci_failure: Some("r-ci".into()),
            ..PrQueuePromptsOverlay::default()
        }
        .apply(&mut base);
        assert_eq!(base.ci_failure, "r-ci");
        assert_eq!(base.conflict, "g-conflict");
        assert_eq!(base.review, "g-review");
    }

    #[test]
    fn merge_mode_aliases_round_trip() {
        for (raw, want) in [
            ("auto_merge", PrMergeMode::AutoMerge),
            ("forge", PrMergeMode::AutoMerge),
            ("thegn", PrMergeMode::Thegn),
            ("direct", PrMergeMode::Thegn),
            ("ready", PrMergeMode::Ready),
            ("notify", PrMergeMode::Ready),
        ] {
            assert_eq!(PrMergeMode::from_str_validated(raw), Ok(want), "{raw}");
        }
        assert!(PrMergeMode::from_str_validated("nope").is_err());
    }
}
