//! Pure policy for configured bottom-drawer occupants.
//!
//! The drawer reuses the existing `[[tools]]` catalog.  This module only
//! selects and describes eligible entries; it does not inspect the filesystem,
//! resolve environment references, probe `PATH`, or launch anything.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;
use serde::de::Deserializer;

use crate::config::{Config, NamedCommand, config_enum, config_warn};

config_enum! {
    /// Where a configured drawer tool is reused: per worktree or globally for
    /// the lifetime of the current thegn process.
    pub enum DrawerScope: "drawer scope" {
        Worktree = "worktree",
        Global = "global",
    } default = Worktree;
}

/// Stable ID of the built-in file-manager drawer occupant.
pub const FILES_OCCUPANT_ID: &str = "files";
/// Stable key used for the one process-local global drawer slot.
pub const GLOBAL_SCOPE_KEY: &str = "global";

/// Deserialize an optional drawer scope without turning a malformed value into
/// a valid worktree occupant.  The regular config enum contract is still
/// lenient (warn, never block), but an invalid optional scope means "not an
/// occupant" so the rest of the tool catalog remains usable.
pub(crate) fn deserialize_scope<'de, D>(deserializer: D) -> Result<Option<DrawerScope>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(
        raw.and_then(|value| match DrawerScope::from_str_validated(&value) {
            Ok(scope) => Some(scope),
            Err(error) => {
                config_warn(&error);
                None
            }
        }),
    )
}

/// One effective drawer occupant.  The built-in files entry has `scope = None`;
/// configured entries carry their scope and the launch metadata copied from
/// `NamedCommand`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawerOccupant {
    pub id: String,
    pub name: String,
    pub scope: Option<DrawerScope>,
    pub command: String,
    pub drawer_cwd: Option<String>,
    pub env: BTreeMap<String, String>,
}

impl DrawerOccupant {
    fn files() -> Self {
        Self {
            id: FILES_OCCUPANT_ID.to_string(),
            name: "files".to_string(),
            scope: None,
            command: String::new(),
            drawer_cwd: None,
            env: BTreeMap::new(),
        }
    }

    /// Whether this occupant is available in the given scope.
    pub fn available_in(&self, scope: DrawerScope) -> bool {
        self.scope.is_none() || self.scope == Some(scope)
    }
}

/// The ordered, validated view of the drawer catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawerPolicy {
    occupants: Vec<DrawerOccupant>,
    warnings: Vec<String>,
}

impl DrawerPolicy {
    /// Build the policy from the existing tools list, preserving its order.
    /// Files is always the first occupant.
    pub fn from_tools(tools: &[NamedCommand]) -> Self {
        let mut policy = Self {
            occupants: vec![DrawerOccupant::files()],
            warnings: Vec::new(),
        };
        let mut ids = std::collections::BTreeSet::new();
        ids.insert(FILES_OCCUPANT_ID.to_string());

        for (index, tool) in tools.iter().enumerate() {
            let Some(scope) = tool.drawer_scope else {
                if tool.drawer_cwd.is_some() {
                    policy.warnings.push(format!(
                        "[[tools]][{index}] {:?}.drawer_cwd requires drawer_scope; ignoring drawer metadata",
                        tool.name.trim()
                    ));
                }
                continue;
            };

            let name = tool.name.trim();
            let command = tool.command.trim();
            if name.is_empty() {
                policy.warnings.push(format!(
                    "[[tools]][{index}].name is required for a drawer occupant; omitting"
                ));
                continue;
            }
            if command.is_empty() {
                policy.warnings.push(format!(
                    "[[tools]][{index}] {name:?}.command is required for a drawer occupant; omitting"
                ));
                continue;
            }
            if let Some(cwd) = &tool.drawer_cwd
                && let Err(error) = validate_drawer_cwd(scope, cwd)
            {
                policy.warnings.push(format!(
                    "[[tools]][{index}] {name:?}.drawer_cwd: {error}; omitting"
                ));
                continue;
            }

            let id = format!("tool:{name}");
            if !ids.insert(id.clone()) {
                policy.warnings.push(format!(
                    "drawer occupant ID {id:?} is duplicated; omitting later entry"
                ));
                continue;
            }
            policy.occupants.push(DrawerOccupant {
                id,
                name: name.to_string(),
                scope: Some(scope),
                command: tool.command.clone(),
                drawer_cwd: tool.drawer_cwd.clone(),
                env: tool.env.clone(),
            });
        }
        policy
    }

    /// Build the policy from a complete config.
    pub fn from_config(cfg: &Config) -> Self {
        Self::from_tools(&cfg.tools)
    }

    pub fn occupants(&self) -> &[DrawerOccupant] {
        &self.occupants
    }

    /// The files occupant plus tools available in `scope`, in registry order.
    pub fn occupants_for(&self, scope: DrawerScope) -> Vec<&DrawerOccupant> {
        self.occupants
            .iter()
            .filter(|occupant| occupant.available_in(scope))
            .collect()
    }

    pub fn occupant(&self, id: &str) -> Option<&DrawerOccupant> {
        self.occupants.iter().find(|occupant| occupant.id == id)
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn configured_count(&self) -> usize {
        self.occupants.len().saturating_sub(1)
    }

    pub fn configured_count_for(&self, scope: DrawerScope) -> usize {
        self.occupants_for(scope).len().saturating_sub(1)
    }
}

/// Build the ordered effective registry for `cfg`.
pub fn drawer_policy(cfg: &Config) -> DrawerPolicy {
    DrawerPolicy::from_config(cfg)
}

/// Emit each normal-loading registry warning once per process. Config is
/// reloaded during hydration, so reporting the same malformed row on every
/// reload would turn one typo into a warning storm.
pub(crate) fn warn_policy_issues(cfg: &Config) {
    static WARNED: OnceLock<Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(std::collections::BTreeSet::new()));
    let policy = drawer_policy(cfg);
    for warning in policy.warnings() {
        if let Ok(mut seen) = warned.lock()
            && seen.insert(warning.clone())
        {
            config_warn(warning);
        }
    }
}

/// Validate the drawer metadata and return strict `config validate` errors.
/// This is deliberately separate from [`DrawerPolicy::from_config`], whose
/// warn-and-omit behavior is used by normal layered loading.
pub fn validate_drawer_config(cfg: &Config) -> Vec<String> {
    let mut errors = Vec::new();
    for (index, agent) in cfg.agents.iter().enumerate() {
        if agent.drawer_scope.is_some() {
            errors.push(format!(
                "[[agents]][{index}].drawer_scope: drawer metadata is only valid on [[tools]]"
            ));
        }
        if agent.drawer_cwd.is_some() {
            errors.push(format!(
                "[[agents]][{index}].drawer_cwd: drawer metadata is only valid on [[tools]]"
            ));
        }
    }

    let mut seen = std::collections::BTreeSet::new();
    for (index, tool) in cfg.tools.iter().enumerate() {
        let Some(scope) = tool.drawer_scope else {
            if tool.drawer_cwd.is_some() {
                errors.push(format!(
                    "[[tools]][{index}].drawer_cwd: requires drawer_scope"
                ));
            }
            continue;
        };
        let name = tool.name.trim();
        if name.is_empty() {
            errors.push(format!(
                "[[tools]][{index}].name: required for drawer metadata"
            ));
        }
        if tool.command.trim().is_empty() {
            errors.push(format!(
                "[[tools]][{index}].command: required for drawer metadata"
            ));
        }
        if let Some(cwd) = &tool.drawer_cwd
            && let Err(error) = validate_drawer_cwd(scope, cwd)
        {
            errors.push(format!("[[tools]][{index}].drawer_cwd: {error}"));
        }
        let id = format!("tool:{name}");
        if !name.is_empty() && !seen.insert(id.clone()) {
            errors.push(format!(
                "[[tools]][{index}]: drawer occupant ID {id:?} is duplicated"
            ));
        }
    }
    errors
}

/// Remove drawer-only metadata from `[[agents]]` during lenient loading.  The
/// strict path reports it before this normalization; regular loading keeps the
/// agent usable as an ordinary agent.
pub(crate) fn strip_agent_metadata(agents: &mut [NamedCommand]) {
    for agent in agents {
        if agent.drawer_scope.is_some() || agent.drawer_cwd.is_some() {
            config_warn(&format!(
                "[[agents]] {:?}: drawer metadata is only valid on [[tools]]; ignoring it",
                agent.name
            ));
            agent.drawer_scope = None;
            agent.drawer_cwd = None;
        }
    }
}

/// Validate and resolve a scope-relative drawer cwd without touching the
/// filesystem. `home` is explicit so callers can expand `~` off-loop.
pub fn resolve_drawer_cwd(
    scope: DrawerScope,
    cwd: Option<&str>,
    worktree: &Path,
    home: &Path,
) -> Result<PathBuf, String> {
    let cwd = cwd.map(str::trim);
    if let Some(cwd) = cwd {
        validate_drawer_cwd(scope, cwd)?;
    }
    match scope {
        DrawerScope::Worktree => Ok(cwd
            .filter(|cwd| !cwd.is_empty())
            .map_or_else(|| worktree.to_path_buf(), |cwd| worktree.join(cwd))),
        DrawerScope::Global => {
            let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) else {
                return Ok(home.to_path_buf());
            };
            if cwd == "~" {
                Ok(home.to_path_buf())
            } else if let Some(rest) = cwd.strip_prefix("~/") {
                Ok(home.join(rest))
            } else {
                Ok(PathBuf::from(cwd))
            }
        }
    }
}

/// Validate a drawer cwd according to its scope. No filesystem access occurs.
pub fn validate_drawer_cwd(scope: DrawerScope, cwd: &str) -> Result<(), String> {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return Err("must not be empty when provided".into());
    }
    match scope {
        DrawerScope::Worktree => {
            let path = Path::new(cwd);
            if path.is_absolute() || cwd == "~" || cwd.starts_with("~/") {
                return Err("worktree drawer_cwd must be relative to the worktree".into());
            }
            if path
                .components()
                .any(|component| component == Component::ParentDir)
            {
                return Err("worktree drawer_cwd must not escape the worktree".into());
            }
        }
        DrawerScope::Global => {
            if !(Path::new(cwd).is_absolute() || cwd == "~" || cwd.starts_with("~/")) {
                return Err("global drawer_cwd must be absolute or start with `~`".into());
            }
        }
    }
    Ok(())
}

/// Return the memory/state key for a drawer scope. Worktree callers provide
/// the already-resolved absolute directory; this function does not canonicalize
/// or otherwise inspect it.
pub fn drawer_scope_key(scope: DrawerScope, worktree: &Path) -> String {
    match scope {
        DrawerScope::Worktree => crate::util::slugify(&worktree.to_string_lossy()),
        DrawerScope::Global => GLOBAL_SCOPE_KEY.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, command: &str, scope: Option<DrawerScope>) -> NamedCommand {
        NamedCommand {
            name: name.into(),
            command: command.into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            model: None,
            env: BTreeMap::new(),
            permissions: Vec::new(),
            resume: false,
            route_via_proxy: false,
            drawer_scope: scope,
            drawer_cwd: None,
        }
    }

    #[test]
    fn files_first_and_tools_keep_config_order() {
        let policy = DrawerPolicy::from_tools(&[
            tool("global", "db", Some(DrawerScope::Global)),
            tool("local", "atac", Some(DrawerScope::Worktree)),
        ]);
        assert_eq!(
            policy
                .occupants()
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["files", "tool:global", "tool:local"]
        );
        assert_eq!(policy.configured_count_for(DrawerScope::Global), 1);
        assert_eq!(policy.configured_count_for(DrawerScope::Worktree), 1);
    }

    #[test]
    fn duplicate_empty_and_blank_commands_are_omitted_with_warnings() {
        let policy = DrawerPolicy::from_tools(&[
            tool("same", "one", Some(DrawerScope::Global)),
            tool(" same ", "two", Some(DrawerScope::Worktree)),
            tool("", "three", Some(DrawerScope::Global)),
            tool("blank", "  ", Some(DrawerScope::Global)),
        ]);
        assert_eq!(policy.occupants().len(), 2);
        assert_eq!(policy.occupants()[1].id, "tool:same");
        assert_eq!(policy.warnings().len(), 3);
    }

    #[test]
    fn scope_and_cwd_policy_is_pure() {
        assert!(validate_drawer_cwd(DrawerScope::Worktree, ".atac").is_ok());
        assert!(validate_drawer_cwd(DrawerScope::Worktree, "/tmp/atac").is_err());
        assert!(validate_drawer_cwd(DrawerScope::Worktree, "../atac").is_err());
        assert!(validate_drawer_cwd(DrawerScope::Global, "~/.db").is_ok());
        assert!(validate_drawer_cwd(DrawerScope::Global, "/tmp/db").is_ok());
        assert!(validate_drawer_cwd(DrawerScope::Global, ".db").is_err());

        let cwd = resolve_drawer_cwd(
            DrawerScope::Worktree,
            Some(".atac"),
            Path::new("/worktree"),
            Path::new("/home/user"),
        )
        .unwrap();
        assert_eq!(cwd, PathBuf::from("/worktree/.atac"));
        let cwd = resolve_drawer_cwd(
            DrawerScope::Global,
            Some("~/.db"),
            Path::new("/worktree"),
            Path::new("/home/user"),
        )
        .unwrap();
        assert_eq!(cwd, PathBuf::from("/home/user/.db"));
    }

    #[test]
    fn scope_keys_are_slugged_or_global_without_io() {
        assert_eq!(
            drawer_scope_key(DrawerScope::Worktree, Path::new("/Work Trees/One")),
            "work-trees-one"
        );
        assert_eq!(
            drawer_scope_key(DrawerScope::Global, Path::new("/ignored")),
            GLOBAL_SCOPE_KEY
        );
    }

    #[test]
    fn metadata_defaults_and_invalid_scope_degrade() {
        let cfg: Config = toml::from_str(
            r#"
[[tools]]
name = "legacy"
command = "legacy"

[[tools]]
name = "bad"
command = "bad"
drawer_scope = "not-a-scope"
"#,
        )
        .unwrap();
        assert_eq!(cfg.tools[0].drawer_scope, None);
        assert_eq!(cfg.tools[0].drawer_cwd, None);
        assert_eq!(cfg.tools[1].drawer_scope, None);
        assert_eq!(DrawerPolicy::from_config(&cfg).configured_count(), 0);
    }

    #[test]
    fn strict_validation_reports_agent_and_tool_metadata_errors() {
        let mut cfg = Config::default();
        let mut agent = tool("agent", "agent", Some(DrawerScope::Global));
        agent.drawer_cwd = Some("relative".into());
        cfg.agents = vec![agent];
        let mut bad = tool("bad", "bad", Some(DrawerScope::Global));
        bad.drawer_cwd = Some("relative".into());
        cfg.tools = vec![bad];
        let errors = validate_drawer_config(&cfg);
        assert!(errors.iter().any(|e| e.contains("[[agents]]")));
        assert!(errors.iter().any(|e| e.contains("global drawer_cwd")));
    }

    #[test]
    fn strict_toml_validation_reports_invalid_scope_and_agent_metadata() {
        let invalid = crate::config_validate::validate_str(
            "[[tools]]\nname = 'bad'\ncommand = 'bad'\ndrawer_scope = 'nowhere'\n",
        );
        assert!(
            invalid
                .iter()
                .any(|error| error.contains("tools[0].drawer_scope")),
            "{invalid:?}"
        );

        let agent = crate::config_validate::validate_str(
            "[[agents]]\nname = 'agent'\ncommand = 'agent'\ndrawer_scope = 'global'\n",
        );
        assert!(
            agent
                .iter()
                .any(|error| error.contains("drawer metadata is only valid on [[tools]]")),
            "{agent:?}"
        );
    }
}
