//! Configuration for the embedded/user skill registry.
//!
//! This module validates syntax only. Directory expansion, discovery, and
//! existence checks belong to the host filesystem adapter.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// `[skills]` — project-local agent recipes seeded by thegn.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SkillsConfig {
    /// Seed skills during the existing worktree lifecycle hooks.
    pub enabled: bool,
    /// Additional directories whose immediate child packages contain a
    /// `SKILL.md`. Empty by default; discovery is host-owned.
    pub user_dirs: Vec<String>,
    /// Skill names withheld from seeding.
    pub exclude: Vec<String>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            user_dirs: Vec::new(),
            exclude: Vec::new(),
        }
    }
}

impl SkillsConfig {
    /// Validate config-boundary syntax without touching the filesystem.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        validate_dirs(&self.user_dirs, &mut errors);

        let mut seen = BTreeSet::new();
        for (index, name) in self.exclude.iter().enumerate() {
            if let Err(error) = crate::skills::validate_name(name) {
                errors.push(format!("skills.exclude[{index}]: {error}"));
            } else if !seen.insert(name.as_str()) {
                errors.push(format!(
                    "skills.exclude[{index}]: duplicate skill name {name:?}"
                ));
            }
        }
        errors
    }
}

fn validate_dirs(dirs: &[String], errors: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    for (index, raw) in dirs.iter().enumerate() {
        let dir = raw.trim();
        let key = format!("skills.user_dirs[{index}]");
        if dir.is_empty() {
            errors.push(format!("{key}: directory must not be empty"));
        } else if dir.len() > 4096 {
            errors.push(format!("{key}: directory is longer than 4096 bytes"));
        } else if dir.chars().any(char::is_control) {
            errors.push(format!(
                "{key}: directory must not contain control characters"
            ));
        } else if dir.contains(',') {
            errors.push(format!(
                "{key}: directory must not contain `,` (the environment list separator)"
            ));
        } else if !seen.insert(dir) {
            errors.push(format!("{key}: duplicate directory {dir:?}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_preserve_existing_skill_seeding() {
        let cfg = SkillsConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.user_dirs.is_empty());
        assert!(cfg.exclude.is_empty());
        assert!(cfg.validate().is_empty());
    }

    #[test]
    fn validates_skill_names_and_directory_syntax_only() {
        let cfg = SkillsConfig {
            enabled: true,
            user_dirs: vec![
                "~/.config/thegn/skills".into(),
                "relative/skills".into(),
                "relative/skills".into(),
                "bad,dir".into(),
                " ".into(),
            ],
            exclude: vec!["mq".into(), "../escape".into(), "mq".into()],
        };
        let errors = cfg.validate();
        assert_eq!(errors.len(), 5, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("duplicate directory")));
        assert!(errors.iter().any(|e| e.contains("list separator")));
        assert!(errors.iter().any(|e| e.contains("must not be empty")));
        assert!(errors.iter().any(|e| e.contains("path-safe")));
        assert!(errors.iter().any(|e| e.contains("duplicate skill")));
    }
}
