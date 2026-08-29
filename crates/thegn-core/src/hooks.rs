//! Pure worktree lifecycle-hook policy.
//!
//! This module deliberately stops at policy resolution. It does not read the
//! process environment, inspect the filesystem, or execute commands; the host
//! owns those concerns. Repo hooks are represented as one trust request per
//! event and are omitted until that request is approved.

use crate::config_resolve::{self, Approvals, GatedRequest};
use serde::{Deserialize, Serialize};

/// Lifecycle edge at which a hook is run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreCreate,
    PostCreate,
    PreDestroy,
    PostDestroy,
    SessionStart,
    SessionEnd,
}

impl HookEvent {
    pub const ALL: [Self; 6] = [
        Self::PreCreate,
        Self::PostCreate,
        Self::PreDestroy,
        Self::PostDestroy,
        Self::SessionStart,
        Self::SessionEnd,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreCreate => "pre_create",
            Self::PostCreate => "post_create",
            Self::PreDestroy => "pre_destroy",
            Self::PostDestroy => "post_destroy",
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
        }
    }
}

/// Source layer of a resolved hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HookScope {
    Global,
    Workspace,
    Repo,
}

/// Action to take when a hook exits unsuccessfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HookFailure {
    Block,
    Warn,
}

/// A string or object accepted by a `[hooks]` event list.
///
/// Object members are optional so the event default can be applied after the
/// entry has been associated with an event. The public enum keeps the config
/// shape visible to callers while [`HookSpec`] is the normalized host-facing
/// representation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum HookEntry {
    Command(String),
    Spec(HookEntrySpec),
}

/// Object form of [`HookEntry`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct HookEntrySpec {
    pub command: String,
    pub wait: Option<bool>,
    pub timeout_secs: Option<u64>,
    pub on_failure: Option<HookFailure>,
}

/// Typed hook lists at a config layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct HooksConfig {
    pub pre_create: Vec<HookEntry>,
    pub post_create: Vec<HookEntry>,
    pub pre_destroy: Vec<HookEntry>,
    pub post_destroy: Vec<HookEntry>,
    pub session_start: Vec<HookEntry>,
    pub session_end: Vec<HookEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct HooksConfigInput {
    pre_create: Vec<HookEntry>,
    post_create: Vec<HookEntry>,
    pre_destroy: Vec<HookEntry>,
    post_destroy: Vec<HookEntry>,
    session_start: Vec<HookEntry>,
    session_end: Vec<HookEntry>,
}

impl<'de> Deserialize<'de> for HooksConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let input = HooksConfigInput::deserialize(deserializer)?;
        let cfg = Self {
            pre_create: input.pre_create,
            post_create: input.post_create,
            pre_destroy: input.pre_destroy,
            post_destroy: input.post_destroy,
            session_start: input.session_start,
            session_end: input.session_end,
        };
        cfg.validate().map_err(serde::de::Error::custom)?;
        Ok(cfg)
    }
}

impl HooksConfig {
    /// Validate policy values that cannot be represented by a useful runner.
    /// Unknown enum values are rejected by serde; zero is not a valid timeout,
    /// and `wait` only has meaning for post-create.
    pub fn validate(&self) -> Result<(), String> {
        for event in HookEvent::ALL {
            for (index, entry) in self.entries(event).iter().enumerate() {
                let HookEntry::Spec(spec) = entry else {
                    continue;
                };
                if spec.timeout_secs == Some(0) {
                    return Err(format!(
                        "hooks.{}[{}].timeout_secs must be greater than zero",
                        event.as_str(),
                        index
                    ));
                }
                if event != HookEvent::PostCreate && spec.wait == Some(true) {
                    return Err(format!(
                        "hooks.{}[{}].wait is only valid for post_create",
                        event.as_str(),
                        index
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn entries(&self, event: HookEvent) -> &[HookEntry] {
        match event {
            HookEvent::PreCreate => &self.pre_create,
            HookEvent::PostCreate => &self.post_create,
            HookEvent::PreDestroy => &self.pre_destroy,
            HookEvent::PostDestroy => &self.post_destroy,
            HookEvent::SessionStart => &self.session_start,
            HookEvent::SessionEnd => &self.session_end,
        }
    }
}

/// A fully normalized hook ready for the host runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct HookSpec {
    pub command: String,
    pub wait: bool,
    pub timeout_secs: u64,
    pub on_failure: HookFailure,
    pub scope: HookScope,
}

impl HookSpec {
    pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

    /// Whether this hook's failure blocks in the supplied operation mode.
    /// Force and unattended cleanup are explicitly non-blocking.
    pub fn blocks_failure(&self, mode: HookExecutionMode) -> bool {
        self.on_failure == HookFailure::Block && mode == HookExecutionMode::User
    }
}

/// Context supplied by the host for environment projection and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookContext {
    pub event: HookEvent,
    pub repo_root: String,
    pub worktree: String,
    pub branch: String,
    pub workspace: String,
}

impl HookContext {
    /// The exact five values a host runner may add after clearing its env.
    pub fn environment(&self) -> [(String, String); 5] {
        [
            ("THEGN_EVENT".into(), self.event.as_str().into()),
            ("THEGN_REPO_ROOT".into(), self.repo_root.clone()),
            ("THEGN_WORKTREE".into(), self.worktree.clone()),
            ("THEGN_BRANCH".into(), self.branch.clone()),
            ("THEGN_WORKSPACE".into(), self.workspace.clone()),
        ]
    }
}

/// Execution context for failure handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookExecutionMode {
    User,
    Force,
    Unattended,
}

/// The normalized policy for all six events, plus repo trust requests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedHooks {
    pub pre_create: Vec<HookSpec>,
    pub post_create: Vec<HookSpec>,
    pub pre_destroy: Vec<HookSpec>,
    pub post_destroy: Vec<HookSpec>,
    pub session_start: Vec<HookSpec>,
    pub session_end: Vec<HookSpec>,
    pub pending: Vec<GatedRequest>,
}

impl ResolvedHooks {
    pub fn entries(&self, event: HookEvent) -> &[HookSpec] {
        match event {
            HookEvent::PreCreate => &self.pre_create,
            HookEvent::PostCreate => &self.post_create,
            HookEvent::PreDestroy => &self.pre_destroy,
            HookEvent::PostDestroy => &self.post_destroy,
            HookEvent::SessionStart => &self.session_start,
            HookEvent::SessionEnd => &self.session_end,
        }
    }
}

/// Default failure policy for a hook before execution-mode overrides.
pub const fn default_failure(event: HookEvent) -> HookFailure {
    match event {
        HookEvent::PreCreate | HookEvent::PreDestroy => HookFailure::Block,
        HookEvent::PostCreate
        | HookEvent::PostDestroy
        | HookEvent::SessionStart
        | HookEvent::SessionEnd => HookFailure::Warn,
    }
}

fn normalized_entry(
    entry: &HookEntry,
    event: HookEvent,
    scope: HookScope,
    legacy_prepare: bool,
) -> HookSpec {
    let (command, wait, timeout_secs, requested_failure) = match entry {
        HookEntry::Command(command) => (command.clone(), false, None, None),
        HookEntry::Spec(spec) => (
            spec.command.clone(),
            spec.wait.unwrap_or(false),
            spec.timeout_secs,
            spec.on_failure,
        ),
    };
    let on_failure = if scope == HookScope::Repo || legacy_prepare {
        HookFailure::Warn
    } else {
        requested_failure.unwrap_or_else(|| default_failure(event))
    };
    HookSpec {
        command,
        wait,
        timeout_secs: timeout_secs.unwrap_or(HookSpec::DEFAULT_TIMEOUT_SECS),
        on_failure,
        scope,
    }
}

fn normalized_prepare(commands: &[String], scope: HookScope) -> Vec<HookSpec> {
    commands
        .iter()
        .map(|command| {
            normalized_entry(
                &HookEntry::Command(command.clone()),
                HookEvent::PostCreate,
                scope,
                true,
            )
        })
        .collect()
}

fn append_nonempty(dst: &mut Vec<HookSpec>, entries: impl IntoIterator<Item = HookSpec>) {
    dst.extend(
        entries
            .into_iter()
            .filter(|entry| !entry.command.trim().is_empty()),
    );
}

/// Resolve already-loaded config layers. This is the pure policy seam: the
/// caller supplies the optional repo overlay and legacy prepare list rather
/// than having this function read a file.
pub fn resolve(
    global: &HooksConfig,
    workspace: Option<&HooksConfig>,
    repo: Option<&HooksConfig>,
    global_prepare: &[String],
    repo_prepare: &[String],
    approvals: &Approvals,
) -> ResolvedHooks {
    let mut out = ResolvedHooks::default();
    for event in HookEvent::ALL {
        let mut entries = Vec::new();
        if event == HookEvent::PostCreate {
            append_nonempty(
                &mut entries,
                normalized_prepare(global_prepare, HookScope::Global),
            );
        }
        append_nonempty(
            &mut entries,
            global
                .entries(event)
                .iter()
                .map(|entry| normalized_entry(entry, event, HookScope::Global, false)),
        );
        if let Some(workspace) = workspace {
            append_nonempty(
                &mut entries,
                workspace
                    .entries(event)
                    .iter()
                    .map(|entry| normalized_entry(entry, event, HookScope::Workspace, false)),
            );
        }

        let mut repo_entries = Vec::new();
        if event == HookEvent::PostCreate {
            append_nonempty(
                &mut repo_entries,
                normalized_prepare(repo_prepare, HookScope::Repo),
            );
        }
        if let Some(repo) = repo {
            append_nonempty(
                &mut repo_entries,
                repo.entries(event)
                    .iter()
                    .map(|entry| normalized_entry(entry, event, HookScope::Repo, false)),
            );
        }
        if !repo_entries.is_empty() {
            let request = config_resolve::repo_hooks_request(event, &repo_entries);
            if approvals.is_approved(&request) {
                entries.extend(repo_entries);
            } else {
                out.pending.push(request);
            }
        }
        match event {
            HookEvent::PreCreate => out.pre_create = entries,
            HookEvent::PostCreate => out.post_create = entries,
            HookEvent::PreDestroy => out.pre_destroy = entries,
            HookEvent::PostDestroy => out.post_destroy = entries,
            HookEvent::SessionStart => out.session_start = entries,
            HookEvent::SessionEnd => out.session_end = entries,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(command: &str) -> HookEntry {
        HookEntry::Command(command.into())
    }

    #[test]
    fn ordered_accumulation_filters_empty_commands_and_applies_defaults() {
        let global = HooksConfig {
            pre_create: vec![command(" global "), command(" ")],
            ..Default::default()
        };
        let workspace = HooksConfig {
            pre_create: vec![command("workspace")],
            ..Default::default()
        };
        let repo = HooksConfig {
            pre_create: vec![command("repo")],
            ..Default::default()
        };

        let resolved = resolve(
            &global,
            Some(&workspace),
            Some(&repo),
            &[],
            &[],
            &Approvals::deny_all(),
        );
        assert_eq!(
            resolved
                .pre_create
                .iter()
                .map(|h| h.command.as_str())
                .collect::<Vec<_>>(),
            [" global ", "workspace"]
        );
        assert_eq!(resolved.pre_create[0].on_failure, HookFailure::Block);
        assert_eq!(resolved.pending.len(), 1);
    }

    #[test]
    fn approved_repo_hooks_are_warn_only_and_legacy_prepare_is_first() {
        let global = HooksConfig {
            post_create: vec![command("global")],
            ..Default::default()
        };
        let repo = HooksConfig {
            post_create: vec![HookEntry::Spec(HookEntrySpec {
                command: "repo".into(),
                wait: Some(true),
                timeout_secs: Some(9),
                on_failure: Some(HookFailure::Block),
            })],
            ..Default::default()
        };
        let repo_entry = HookSpec {
            command: "repo".into(),
            wait: true,
            timeout_secs: 9,
            on_failure: HookFailure::Warn,
            scope: HookScope::Repo,
        };
        let req = config_resolve::repo_hooks_request(
            HookEvent::PostCreate,
            &[
                HookSpec {
                    command: "legacy repo".into(),
                    wait: false,
                    timeout_secs: 120,
                    on_failure: HookFailure::Warn,
                    scope: HookScope::Repo,
                },
                repo_entry.clone(),
            ],
        );
        let resolved = resolve(
            &global,
            None,
            Some(&repo),
            &["prepare".into()],
            &["legacy repo".into()],
            &Approvals::from_canonical([req.canonical()]),
        );
        assert_eq!(resolved.pending.len(), 0);
        assert_eq!(resolved.post_create[0].command, "prepare");
        assert_eq!(resolved.post_create[1].command, "global");
        assert_eq!(resolved.post_create[2].command, "legacy repo");
        assert_eq!(resolved.post_create[3], repo_entry);
    }

    #[test]
    fn repo_request_is_pending_until_the_normalized_event_list_is_approved() {
        let mut repo = HooksConfig {
            pre_destroy: vec![command("echo one")],
            ..Default::default()
        };
        let denied = resolve(
            &HooksConfig::default(),
            None,
            Some(&repo),
            &[],
            &[],
            &Approvals::deny_all(),
        );
        assert!(denied.pre_destroy.is_empty());
        assert_eq!(denied.pending[0].key, "hooks.pre_destroy");

        let request = &denied.pending[0];
        let approved = resolve(
            &HooksConfig::default(),
            None,
            Some(&repo),
            &[],
            &[],
            &Approvals::from_canonical([request.canonical()]),
        );
        assert_eq!(approved.pre_destroy[0].command, "echo one");

        repo.pre_destroy = vec![command("echo changed")];
        let edited = resolve(
            &HooksConfig::default(),
            None,
            Some(&repo),
            &[],
            &[],
            &Approvals::from_canonical([request.canonical()]),
        );
        assert!(edited.pre_destroy.is_empty());
        assert_eq!(edited.pending[0].key, "hooks.pre_destroy");
    }

    #[test]
    fn config_rejects_invalid_policy_values() {
        let zero = r#"post_create = [{ command = "x", timeout_secs = 0 }]"#;
        let err = toml::from_str::<HooksConfig>(zero).unwrap_err().to_string();
        assert!(err.contains("greater than zero"));
        let wait = r#"pre_create = [{ command = "x", wait = true }]"#;
        let err = toml::from_str::<HooksConfig>(wait).unwrap_err().to_string();
        assert!(err.contains("only valid for post_create"));
    }

    #[test]
    fn force_and_unattended_never_block() {
        let spec = HookSpec {
            command: "x".into(),
            wait: false,
            timeout_secs: 120,
            on_failure: HookFailure::Block,
            scope: HookScope::Global,
        };
        assert!(spec.blocks_failure(HookExecutionMode::User));
        assert!(!spec.blocks_failure(HookExecutionMode::Force));
        assert!(!spec.blocks_failure(HookExecutionMode::Unattended));
    }

    #[test]
    fn context_environment_is_exact_and_secret_free() {
        let context = HookContext {
            event: HookEvent::SessionStart,
            repo_root: "/repo".into(),
            worktree: "/worktree".into(),
            branch: "feature".into(),
            workspace: "demo".into(),
        };
        let env = context.environment();
        assert_eq!(env.len(), 5);
        assert_eq!(env[0], ("THEGN_EVENT".into(), "session_start".into()));
        assert!(!env.iter().any(|(key, _)| key.contains("TOKEN")));
    }

    #[test]
    fn request_value_is_normalized_json() {
        let spec = HookSpec {
            command: "echo x".into(),
            wait: false,
            timeout_secs: 120,
            on_failure: HookFailure::Warn,
            scope: HookScope::Repo,
        };
        let request = config_resolve::repo_hooks_request(HookEvent::PostCreate, &[spec]);
        assert_eq!(request.key, "hooks.post_create");
        assert_eq!(request.value[0]["command"], "echo x");
    }
}
