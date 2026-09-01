//! Pure policy and data contracts for `sessions.fork`.
//!
//! A fork is a plan for a fresh launch, never a copy of a process, PTY,
//! emulator, or scrollback. The daemon supplies a live recipe for raw sessions
//! or a credential-free recorded harness row; this module only validates those
//! inputs and selects the vendor-owned harness command. In particular,
//! [`ForkRecord`] deliberately has no recipe-shaped fields.

use crate::harness::{self, HarnessCaps};

/// The bounded raw launch recipe retained by a live daemon session.
///
/// This type is intentionally not serializable: its environment may contain
/// credentials and it must remain in daemon memory only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLaunchRecipe {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub worktree: Option<String>,
}

/// Short name for the daemon's memory-only raw recipe.
pub type RawRecipe = RawLaunchRecipe;

/// The kind of recipe retained for a live daemon session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonRecipe {
    /// Replay the caller's raw argv and environment, with daemon identity
    /// variables re-applied at spawn time.
    Raw(RawLaunchRecipe),
    /// A configured agent launch whose native conversation id is available.
    /// The host re-resolves the rest of the agent composition when it spawns.
    Agent {
        harness: String,
        native_session_id: Option<String>,
        agent: Option<String>,
        cwd: Option<String>,
        worktree: Option<String>,
    },
}

/// A source accepted by the fork policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkSource {
    /// A live daemon session. The source id is the daemon id and the recipe is
    /// memory-only on the host.
    DaemonSession { id: String, recipe: DaemonRecipe },
    /// A credential-free row selected from `agent.sessions`. The native id is
    /// validated and handed to the selected harness; no transcript is read.
    HarnessSession {
        harness: String,
        id: String,
        agent: Option<String>,
        worktree: Option<String>,
    },
}

/// Placement intent carried by the core contract. The compositor owns the
/// actual graft; a headless caller can ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForkPlacement {
    /// Adopt beside the source/current target.
    #[default]
    Sibling,
    /// Adopt in a new tab.
    NewTab,
}

/// Options that affect a fork plan but do not contain a process recipe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForkOptions {
    pub cwd: Option<String>,
    pub worktree: Option<String>,
    pub scrollback: bool,
    pub adopt: bool,
    pub placement: ForkPlacement,
}

/// A complete pure fork request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkRequest {
    pub source: ForkSource,
    pub options: ForkOptions,
}

impl ForkRequest {
    pub fn plan(&self) -> Result<ForkPlan, ForkError> {
        plan(&self.source, &self.options)
    }
}

/// A pure, daemon-consumable launch plan.
///
/// The raw variant contains sensitive data and is therefore deliberately not
/// serializable. `already_capped` is always false: the daemon owns the new
/// spawn and must apply its cap again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkPlan {
    Raw {
        source_id: String,
        argv: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        worktree: Option<String>,
        already_capped: bool,
    },
    Harness {
        /// The daemon id for a daemon source, or the native id for a recorded
        /// source. The stable display lineage is available via `lineage`.
        source_id: String,
        lineage: String,
        harness: String,
        native_session_id: String,
        agent: Option<String>,
        command: String,
        cwd: Option<String>,
        worktree: Option<String>,
    },
}

impl ForkPlan {
    /// The source kind used by a credential-free lineage record.
    pub fn source_kind(&self) -> ForkSourceKind {
        match self {
            Self::Raw { .. } => ForkSourceKind::Daemon,
            Self::Harness { .. } => ForkSourceKind::Harness,
        }
    }

    /// Stable source display form for `THEGN_FORKED_FROM` and listings.
    pub fn lineage(&self) -> &str {
        match self {
            Self::Raw { source_id, .. } => source_id,
            Self::Harness { lineage, .. } => lineage,
        }
    }

    /// Compose the raw plan's environment with the identity values allocated
    /// by the daemon. Harness plans return `None` because the host first
    /// resolves their current credential/sandbox environment.
    pub fn raw_environment(
        &self,
        child_id: &str,
        control_socket: &str,
        scrollback_path: Option<&str>,
    ) -> Option<Vec<(String, String)>> {
        match self {
            Self::Raw { env, .. } => Some(compose_identity_env(
                env,
                child_id,
                control_socket,
                self.lineage(),
                scrollback_path,
            )),
            Self::Harness { .. } => None,
        }
    }
}

/// Why a fork request could not be planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkError {
    InvalidSessionId(String),
    InvalidHarnessId(String),
    EmptyRawArgv,
    RawArgvTooLarge,
    RawEnvironmentTooLarge,
    NativeSessionIdUnavailable { harness: String },
    ReservedHarness { harness: String },
}

impl std::fmt::Display for ForkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSessionId(id) => write!(f, "invalid session id `{id}`"),
            Self::InvalidHarnessId(id) => write!(f, "unknown harness `{id}`"),
            Self::EmptyRawArgv => f.write_str("raw fork source has no argv"),
            Self::RawArgvTooLarge => f.write_str("raw fork argv exceeds its bound"),
            Self::RawEnvironmentTooLarge => f.write_str("raw fork environment exceeds its bound"),
            Self::NativeSessionIdUnavailable { harness } => {
                write!(f, "native session id unavailable for harness `{harness}`")
            }
            Self::ReservedHarness { harness } => {
                write!(
                    f,
                    "harness `{harness}` does not support native session fork (reserved)"
                )
            }
        }
    }
}

impl std::error::Error for ForkError {}

const MAX_RAW_ARGV: usize = 4096;
const MAX_RAW_ENV: usize = 4096;

fn checked_session_id(id: &str) -> Result<(), ForkError> {
    harness::session_id_ok(id)
        .then_some(())
        .ok_or_else(|| ForkError::InvalidSessionId(id.into()))
}

fn checked_harness(id: &str) -> Result<&'static dyn harness::Harness, ForkError> {
    harness::harness(id).ok_or_else(|| ForkError::InvalidHarnessId(id.into()))
}

fn harness_command(harness_id: &str, native_session_id: &str) -> Result<String, ForkError> {
    let h = checked_harness(harness_id)?;
    if !h.caps().contains(HarnessCaps::FORK) {
        return Err(ForkError::ReservedHarness {
            harness: harness_id.into(),
        });
    }
    h.fork_command(native_session_id)
        .filter(|command| !command.is_empty())
        .ok_or_else(|| ForkError::ReservedHarness {
            harness: harness_id.into(),
        })
}

/// Plan a fork without performing I/O, spawning, or credential resolution.
pub fn plan(source: &ForkSource, options: &ForkOptions) -> Result<ForkPlan, ForkError> {
    match source {
        ForkSource::DaemonSession { id, recipe } => {
            checked_session_id(id)?;
            match recipe {
                DaemonRecipe::Raw(recipe) => {
                    if recipe.argv.is_empty() {
                        return Err(ForkError::EmptyRawArgv);
                    }
                    if recipe.argv.len() > MAX_RAW_ARGV {
                        return Err(ForkError::RawArgvTooLarge);
                    }
                    if recipe.env.len() > MAX_RAW_ENV {
                        return Err(ForkError::RawEnvironmentTooLarge);
                    }
                    Ok(ForkPlan::Raw {
                        source_id: id.clone(),
                        argv: recipe.argv.clone(),
                        cwd: options.cwd.clone().or_else(|| recipe.cwd.clone()),
                        env: recipe.env.clone(),
                        worktree: options.worktree.clone().or_else(|| recipe.worktree.clone()),
                        already_capped: false,
                    })
                }
                DaemonRecipe::Agent {
                    harness,
                    native_session_id,
                    agent,
                    cwd,
                    worktree,
                } => plan_harness(
                    id,
                    harness,
                    native_session_id.as_deref(),
                    agent,
                    cwd,
                    worktree,
                    options,
                ),
            }
        }
        ForkSource::HarnessSession {
            harness,
            id,
            agent,
            worktree,
        } => plan_harness(id, harness, Some(id), agent, &None, worktree, options),
    }
}

fn plan_harness(
    source_id: &str,
    harness_id: &str,
    native_session_id: Option<&str>,
    agent: &Option<String>,
    cwd: &Option<String>,
    worktree: &Option<String>,
    options: &ForkOptions,
) -> Result<ForkPlan, ForkError> {
    checked_session_id(source_id)?;
    let native_session_id = native_session_id
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ForkError::NativeSessionIdUnavailable {
            harness: harness_id.into(),
        })?;
    checked_session_id(native_session_id)?;
    let command = harness_command(harness_id, native_session_id)?;
    let lineage = format!("{harness_id}:{native_session_id}");
    Ok(ForkPlan::Harness {
        source_id: source_id.into(),
        lineage,
        harness: harness_id.into(),
        native_session_id: native_session_id.into(),
        agent: agent.clone(),
        command,
        cwd: options.cwd.clone().or_else(|| cwd.clone()),
        worktree: options.worktree.clone().or_else(|| worktree.clone()),
    })
}

/// Add daemon identity variables to a child environment.
///
/// Existing values are removed before the new values are appended. This makes
/// identity overwrite deterministic and prevents a raw source or inherited
/// environment from spoofing the child identity. Passing `None` for
/// `scrollback_path` also removes a stale handoff variable.
pub fn compose_identity_env(
    base: &[(String, String)],
    child_id: &str,
    control_socket: &str,
    forked_from: &str,
    scrollback_path: Option<&str>,
) -> Vec<(String, String)> {
    const IDENTITY_KEYS: [&str; 4] = [
        "THEGN_SESSION_ID",
        "THEGN_CONTROL_SOCKET",
        "THEGN_FORKED_FROM",
        "THEGN_FORK_SCROLLBACK",
    ];
    let mut env: Vec<(String, String)> = base
        .iter()
        .filter(|(key, _)| !IDENTITY_KEYS.contains(&key.as_str()))
        .cloned()
        .collect();
    env.push(("THEGN_SESSION_ID".into(), child_id.into()));
    env.push(("THEGN_CONTROL_SOCKET".into(), control_socket.into()));
    env.push(("THEGN_FORKED_FROM".into(), forked_from.into()));
    if let Some(path) = scrollback_path {
        env.push(("THEGN_FORK_SCROLLBACK".into(), path.into()));
    }
    env
}

/// The two source kinds that can appear in a lineage record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkSourceKind {
    Daemon,
    Harness,
}

/// Credential-free cache metadata for one successful fork.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForkRecord {
    pub child_id: String,
    pub source_kind: ForkSourceKind,
    pub source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    pub created_at: i64,
}

impl ForkRecord {
    /// Create metadata from a pure plan. No command, argv, environment,
    /// prompt, transcript, or credential data is retained.
    pub fn from_plan(child_id: &str, plan: &ForkPlan, created_at: i64) -> Self {
        match plan {
            ForkPlan::Raw {
                source_id,
                worktree,
                ..
            } => Self {
                child_id: child_id.into(),
                source_kind: ForkSourceKind::Daemon,
                source_id: source_id.clone(),
                harness: None,
                worktree: worktree.clone(),
                created_at,
            },
            ForkPlan::Harness {
                native_session_id,
                harness,
                worktree,
                ..
            } => Self {
                child_id: child_id.into(),
                source_kind: ForkSourceKind::Harness,
                source_id: native_session_id.clone(),
                harness: Some(harness.clone()),
                worktree: worktree.clone(),
                created_at,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> ForkSource {
        ForkSource::DaemonSession {
            id: "daemon-1".into(),
            recipe: DaemonRecipe::Raw(RawLaunchRecipe {
                argv: vec!["sh".into(), "-lc".into(), "printf ok".into()],
                cwd: Some("/old".into()),
                env: vec![
                    ("KEEP".into(), "yes".into()),
                    ("THEGN_SESSION_ID".into(), "spoofed".into()),
                ],
                worktree: Some("/wt/old".into()),
            }),
        }
    }

    fn native(harness: &str, id: &str) -> ForkSource {
        ForkSource::HarnessSession {
            harness: harness.into(),
            id: id.into(),
            agent: Some("worker".into()),
            worktree: Some("/wt".into()),
        }
    }

    #[test]
    fn raw_plan_replays_recipe_with_overrides_and_resets_cap() {
        let plan = plan(
            &raw(),
            &ForkOptions {
                cwd: Some("/new".into()),
                worktree: Some("/wt/new".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            plan,
            ForkPlan::Raw {
                source_id: "daemon-1".into(),
                argv: vec!["sh".into(), "-lc".into(), "printf ok".into()],
                cwd: Some("/new".into()),
                env: vec![
                    ("KEEP".into(), "yes".into()),
                    ("THEGN_SESSION_ID".into(), "spoofed".into()),
                ],
                worktree: Some("/wt/new".into()),
                already_capped: false,
            }
        );
        assert_eq!(
            plan.raw_environment("child-1", "sock", None).unwrap(),
            vec![
                ("KEEP".into(), "yes".into()),
                ("THEGN_SESSION_ID".into(), "child-1".into()),
                ("THEGN_CONTROL_SOCKET".into(), "sock".into()),
                ("THEGN_FORKED_FROM".into(), "daemon-1".into()),
            ]
        );
    }

    #[test]
    fn native_plan_uses_harness_operation_and_stable_lineage() {
        let plan = plan(&native("claude", "native-1"), &ForkOptions::default()).unwrap();
        match plan {
            ForkPlan::Harness {
                command,
                lineage,
                agent,
                ..
            } => {
                assert_eq!(command, "claude --resume native-1 --fork-session");
                assert_eq!(lineage, "claude:native-1");
                assert_eq!(agent.as_deref(), Some("worker"));
            }
            ForkPlan::Raw { .. } => panic!("expected harness plan"),
        }
    }

    #[test]
    fn invalid_and_unsupported_sources_are_refused() {
        let bad = native("claude", "bad/id");
        assert!(matches!(
            plan(&bad, &ForkOptions::default()),
            Err(ForkError::InvalidSessionId(_))
        ));
        assert!(matches!(
            plan(&native("pi", "native-1"), &ForkOptions::default()),
            Err(ForkError::ReservedHarness { .. })
        ));
        assert!(matches!(
            plan(
                &ForkSource::DaemonSession {
                    id: "daemon-1".into(),
                    recipe: DaemonRecipe::Agent {
                        harness: "claude".into(),
                        native_session_id: None,
                        agent: None,
                        cwd: None,
                        worktree: None,
                    },
                },
                &ForkOptions::default(),
            ),
            Err(ForkError::NativeSessionIdUnavailable { .. })
        ));
    }

    #[test]
    fn identity_environment_overwrites_and_orders_identity_values() {
        let env = compose_identity_env(
            &[
                ("KEEP".into(), "yes".into()),
                ("THEGN_SESSION_ID".into(), "old".into()),
                ("THEGN_FORK_SCROLLBACK".into(), "stale".into()),
            ],
            "child-1",
            "/run/thegn.sock",
            "daemon-1",
            Some("/state/forks/child-1.txt"),
        );
        assert_eq!(
            env,
            vec![
                ("KEEP".into(), "yes".into()),
                ("THEGN_SESSION_ID".into(), "child-1".into()),
                ("THEGN_CONTROL_SOCKET".into(), "/run/thegn.sock".into()),
                ("THEGN_FORKED_FROM".into(), "daemon-1".into()),
                (
                    "THEGN_FORK_SCROLLBACK".into(),
                    "/state/forks/child-1.txt".into()
                ),
            ]
        );
        assert_eq!(
            compose_identity_env(&env, "child-2", "sock", "daemon-1", None)
                .iter()
                .filter(|(key, _)| key == "THEGN_FORK_SCROLLBACK")
                .count(),
            0
        );
    }

    #[test]
    fn fork_record_is_credential_free_lineage_only() {
        let plan = plan(&raw(), &ForkOptions::default()).unwrap();
        let record = ForkRecord::from_plan("child-1", &plan, 42);
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(record.source_kind, ForkSourceKind::Daemon);
        assert!(!json.contains("argv"));
        assert!(!json.contains("env"));
        assert!(!json.contains("prompt"));
        assert!(!json.contains("transcript"));
        assert!(!json.contains("credential"));
        assert!(!json.contains("printf ok"));
        assert!(!json.contains("spoofed"));
    }
}
