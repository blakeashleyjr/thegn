//! Read-only skill installation state for `thegn doctor`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;
use thegn_core::config::Config;
use thegn_core::skills::{document_hash, inspect_managed, skill_relative};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillsDoctorReport {
    pub enabled: bool,
    pub worktree: String,
    pub harnesses: Vec<HarnessSkillState>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HarnessSkillState {
    pub id: String,
    pub configured: bool,
    pub supported: bool,
    pub project_root: Option<String>,
    pub target_directory_found: bool,
    pub managed_current: usize,
    pub managed_stale: usize,
    pub managed_user_modified: usize,
    pub unmarked: usize,
    pub absent: usize,
}

/// Survey the selected/current worktree and configured user package roots.
/// This never calls the seed/apply path.
pub fn inspect(cfg: &Config, worktree: &Path) -> SkillsDoctorReport {
    let loaded = crate::skill_seed::load_registry(cfg);
    let (configured, mut diagnostics) = crate::skill_seed::configured_harnesses(cfg);
    diagnostics.extend(loaded.diagnostics);
    let configured: BTreeSet<String> = configured.into_iter().collect();
    let excluded: BTreeSet<&str> = cfg.skills.exclude.iter().map(String::as_str).collect();
    let mut harnesses = Vec::new();

    for harness in thegn_core::harness::HARNESSES {
        let layout = harness.skill_layout();
        let mut state = HarnessSkillState {
            id: harness.id().to_string(),
            configured: configured.contains(harness.id()),
            supported: layout.is_some(),
            project_root: layout.map(|layout| layout.project_root.to_string()),
            target_directory_found: false,
            managed_current: 0,
            managed_stale: 0,
            managed_user_modified: 0,
            unmarked: 0,
            absent: 0,
        };
        let Some(layout) = layout else {
            harnesses.push(state);
            continue;
        };
        let root = match crate::skill_seed::checked_layout_root(worktree, layout.project_root) {
            Ok(root) => root,
            Err(error) => {
                diagnostics.push(format!("{}: {error}", harness.id()));
                harnesses.push(state);
                continue;
            }
        };
        let survey = match crate::skill_seed::survey_skill_root(&root) {
            Ok(survey) => survey,
            Err(error) => {
                diagnostics.push(format!("{}: {error}", harness.id()));
                harnesses.push(state);
                continue;
            }
        };
        state.target_directory_found = survey.directory_found;
        diagnostics.extend(
            survey
                .diagnostics
                .into_iter()
                .map(|d| format!("{}: {d}", harness.id())),
        );
        let existing: BTreeMap<&str, &[u8]> = survey
            .files
            .iter()
            .map(|file| (file.relative.as_str(), file.bytes.as_slice()))
            .collect();
        for (name, skill) in loaded.registry.iter() {
            if excluded.contains(name) || !skill.harnesses.contains(harness.id()) {
                continue;
            }
            let relative = skill_relative(name).expect("registry names are validated");
            let Some(bytes) = existing.get(relative.as_str()).copied() else {
                state.absent += 1;
                continue;
            };
            match inspect_managed(bytes) {
                None => state.unmarked += 1,
                Some(managed) if managed.is_user_modified() => state.managed_user_modified += 1,
                Some(managed) if managed.marker.recorded_hash == document_hash(skill) => {
                    state.managed_current += 1;
                }
                Some(_) => state.managed_stale += 1,
            }
        }
        harnesses.push(state);
    }
    diagnostics.sort();
    diagnostics.dedup();
    SkillsDoctorReport {
        enabled: cfg.skills.enabled,
        worktree: worktree.display().to_string(),
        harnesses,
        diagnostics,
    }
}

pub fn print(report: &SkillsDoctorReport) {
    thegn_core::outln!("Skills ([skills])");
    thegn_core::outln!(
        "  enabled: {} · worktree: {}",
        report.enabled,
        report.worktree
    );
    for row in &report.harnesses {
        let target = match (&row.project_root, row.supported) {
            (Some(root), true) => root.as_str(),
            _ => "unsupported",
        };
        thegn_core::outln!(
            "  {:<12} configured: {:<3} target: {:<18} found: {:<3} current: {} · stale: {} · modified: {} · unmarked: {} · absent: {}",
            row.id,
            if row.configured { "yes" } else { "no" },
            target,
            if row.target_directory_found {
                "yes"
            } else {
                "no"
            },
            row.managed_current,
            row.managed_stale,
            row.managed_user_modified,
            row.unmarked,
            row.absent,
        );
    }
    for diagnostic in &report.diagnostics {
        thegn_core::outln!("  ! {diagnostic}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_is_read_only_and_uses_one_state_model() {
        let worktree = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        let before = std::fs::read_dir(worktree.path()).unwrap().count();
        let report = inspect(&cfg, worktree.path());
        assert_eq!(before, std::fs::read_dir(worktree.path()).unwrap().count());
        let claude = report
            .harnesses
            .iter()
            .find(|row| row.id == "claude")
            .unwrap();
        assert!(claude.configured && claude.supported);
        assert_eq!(claude.project_root.as_deref(), Some(".claude/skills"));
        assert_eq!(claude.absent, 3);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["harnesses"][1]["id"], "claude");
    }
}
