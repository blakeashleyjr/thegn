//! Host filesystem adapter for the embedded and configured skill registry.
//!
//! Core owns parsing, validation, markers, and the pure seed plan. This module
//! is the only runtime boundary that discovers user packages or writes skill
//! files into a worktree.

use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use thegn_core::config::Config;
use thegn_core::skills::{
    ExistingFile, GateState, SeedPhase, SkillDocument, SkillRegistry, SkillSource, WriteReason,
    inspect_managed, parse_document, plan_seed, render_document, validate_name,
};

const MAX_PACKAGES_PER_DIR: usize = 1024;

const MQ_COMMANDS: &[LegacyCommand] = &[
    LegacyCommand {
        relative: ".claude/commands/mq-add.md",
        body: include_str!("../../../extensions/commands/mq-add.md"),
    },
    LegacyCommand {
        relative: ".claude/commands/mq-drain.md",
        body: include_str!("../../../extensions/commands/mq-drain.md"),
    },
];

struct LegacyCommand {
    relative: &'static str,
    body: &'static str,
}

/// A registry plus every edge diagnostic encountered while discovering it.
pub(crate) struct RegistryLoad {
    pub registry: SkillRegistry,
    pub diagnostics: Vec<String>,
}

/// One deterministic result row from an explicit or automatic seed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeedFileResult {
    pub harness: String,
    pub path: String,
    pub status: String,
}

/// Stable seed output. File rows are ordered by harness and relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeedReport {
    pub worktree: String,
    pub files: Vec<SeedFileResult>,
    pub diagnostics: Vec<String>,
}

/// A bounded survey of one harness-native skill root.
pub(crate) struct SkillSurvey {
    pub files: Vec<ExistingFile>,
    pub diagnostics: Vec<String>,
    pub directory_found: bool,
}

/// Build the trusted-first registry. Each configured directory contributes
/// only immediate `<package>/SKILL.md` children; built-ins always win names.
pub(crate) fn load_registry(cfg: &Config) -> RegistryLoad {
    let mut diagnostics = Vec::new();
    let mut registry = match SkillRegistry::embedded() {
        Ok(registry) => registry,
        Err(error) => {
            diagnostics.push(format!("embedded skill registry: {error}"));
            SkillRegistry::new()
        }
    };

    for raw_dir in &cfg.skills.user_dirs {
        let dir = PathBuf::from(thegn_core::util::expand_tilde(raw_dir));
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                diagnostics.push(format!("{}: {error}", dir.display()));
                continue;
            }
        };
        let mut packages = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => packages.push(entry),
                Err(error) => diagnostics.push(format!("{}: {error}", dir.display())),
            }
        }
        packages.sort_by_key(std::fs::DirEntry::file_name);
        if packages.len() > MAX_PACKAGES_PER_DIR {
            diagnostics.push(format!(
                "{}: only the first {MAX_PACKAGES_PER_DIR} immediate packages are inspected",
                dir.display()
            ));
            packages.truncate(MAX_PACKAGES_PER_DIR);
        }
        for entry in packages {
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    diagnostics.push(format!("{}: {error}", entry.path().display()));
                    continue;
                }
            };
            // Symlinked packages are not immediate directory packages and can
            // escape the configured root between survey and read.
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let Some(package_name) = entry.file_name().to_str().map(str::to_string) else {
                diagnostics.push(format!(
                    "{}: package name is not UTF-8",
                    entry.path().display()
                ));
                continue;
            };
            // Validate the observed directory segment before using it to build
            // the document path. Frontmatter is never trusted as a path input.
            if let Err(error) = validate_name(&package_name) {
                diagnostics.push(format!("{}: {error}", entry.path().display()));
                continue;
            }
            let path = entry.path().join("SKILL.md");
            let bytes = match read_bounded(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    diagnostics.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            let source = SkillSource::user(path.display().to_string());
            match parse_document(&bytes, &package_name, source) {
                Ok(skill) => {
                    if let Err(error) = registry.insert(skill) {
                        diagnostics.push(format!("{}: {error}", path.display()));
                    }
                }
                Err(error) => diagnostics.push(error.to_string()),
            }
        }
    }
    RegistryLoad {
        registry,
        diagnostics,
    }
}

fn read_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take((thegn_core::skills::MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > thegn_core::skills::MAX_DOCUMENT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "document is larger than {} bytes",
                thegn_core::skills::MAX_DOCUMENT_BYTES
            ),
        ));
    }
    Ok(bytes)
}

/// Distinct configured harness ids. Agent entries and pipeline overrides are
/// resolved through the closed harness seam; an empty/default config retains
/// the historical Claude target.
pub(crate) fn configured_harnesses(cfg: &Config) -> (Vec<String>, Vec<String>) {
    let mut ids = BTreeSet::new();
    let mut diagnostics = Vec::new();
    for entry in &cfg.agents {
        if entry.name == "shell" || entry.command.trim() == "__shell__" {
            continue;
        }
        let id = thegn_core::agent_task::provider_id(entry);
        if thegn_core::harness::harness(&id).is_some() {
            ids.insert(id);
        } else {
            diagnostics.push(format!(
                "agent {:?}: unknown harness {id:?}; skills skipped",
                entry.name
            ));
        }
    }
    for stage in &cfg.pipeline.stages {
        let id = stage
            .harness
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| {
                cfg.agents
                    .iter()
                    .chain(cfg.tools.iter())
                    .find(|entry| entry.name == stage.agent)
                    .map(thegn_core::agent_task::provider_id)
            })
            .or_else(|| {
                thegn_core::harness::harness(stage.agent.trim()).map(|h| h.id().to_string())
            });
        match id {
            Some(id) if thegn_core::harness::harness(&id).is_some() => {
                ids.insert(id);
            }
            Some(id) => diagnostics.push(format!(
                "pipeline stage {:?}: unknown harness {id:?}; skills skipped",
                stage.name
            )),
            None => diagnostics.push(format!(
                "pipeline stage {:?}: agent {:?} does not resolve to a harness; skills skipped",
                stage.name, stage.agent
            )),
        }
    }
    if ids.is_empty() {
        ids.insert("claude".to_string());
    }
    (ids.into_iter().collect(), diagnostics)
}

fn gate_state(cfg: &Config) -> GateState {
    GateState {
        merge_queue_open: cfg.merge_queue.enabled,
        pipeline_open: !cfg.pipeline.stages.is_empty(),
    }
}

pub(crate) fn checked_layout_root(worktree: &Path, project_root: &str) -> Result<PathBuf, String> {
    let relative = Path::new(project_root);
    if relative.is_absolute()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!("unsafe harness skill root {project_root:?}"));
    }
    let mut current = worktree.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            unreachable!("filtered above")
        };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "{} is a symlink; target skipped",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", current.display())),
        }
    }
    Ok(current)
}

/// Survey immediate, path-safe packages in one native skill root. Unsafe or
/// unreadable entries are represented as unmarked conflicts where possible,
/// so a later plan can never reinterpret them as absent and overwrite them.
pub(crate) fn survey_skill_root(root: &Path) -> Result<SkillSurvey, String> {
    let directory_found = root.is_dir();
    if !root.exists() {
        return Ok(SkillSurvey {
            files: Vec::new(),
            diagnostics: Vec::new(),
            directory_found: false,
        });
    }
    let entries = std::fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let mut entries: Vec<_> = entries.collect();
    entries.sort_by_key(|entry| entry.as_ref().ok().map(std::fs::DirEntry::file_name));
    let mut files = Vec::new();
    let mut diagnostics = Vec::new();
    if entries.len() > MAX_PACKAGES_PER_DIR {
        diagnostics.push(format!(
            "{}: only the first {MAX_PACKAGES_PER_DIR} immediate packages are surveyed",
            root.display()
        ));
        entries.truncate(MAX_PACKAGES_PER_DIR);
    }
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                diagnostics.push(format!("{}: {error}", root.display()));
                continue;
            }
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            diagnostics.push(format!(
                "{}: package name is not UTF-8",
                entry.path().display()
            ));
            continue;
        };
        if let Err(error) = validate_name(&name) {
            diagnostics.push(format!("{}: {error}", entry.path().display()));
            continue;
        }
        let relative = format!("{name}/SKILL.md");
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                diagnostics.push(format!("{}: {error}", entry.path().display()));
                files.push(ExistingFile::new(relative, Vec::new()));
                continue;
            }
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            files.push(ExistingFile::new(relative, Vec::new()));
            continue;
        }
        let path = entry.path().join("SKILL.md");
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                files.push(ExistingFile::new(relative, Vec::new()));
            }
            Ok(_) => match read_bounded(&path) {
                Ok(bytes) => files.push(ExistingFile::new(relative, bytes)),
                Err(error) => {
                    diagnostics.push(format!("{}: {error}", path.display()));
                    files.push(ExistingFile::new(relative, Vec::new()));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                diagnostics.push(format!("{}: {error}", path.display()));
                files.push(ExistingFile::new(relative, Vec::new()));
            }
        }
    }
    Ok(SkillSurvey {
        files,
        diagnostics,
        directory_found,
    })
}

/// Seed every configured harness into `worktree` for one lifecycle phase.
pub fn seed(cfg: &Config, worktree: &Path, phase: SeedPhase) -> Result<SeedReport> {
    if !worktree.is_dir() {
        bail!(
            "skills seed target is not a directory: {}",
            worktree.display()
        );
    }
    let loaded = load_registry(cfg);
    let (harnesses, mut diagnostics) = configured_harnesses(cfg);
    diagnostics.extend(loaded.diagnostics);
    let mut files = Vec::new();
    let excludes: BTreeSet<String> = cfg.skills.exclude.iter().cloned().collect();

    for harness_id in harnesses {
        let Some(harness) = thegn_core::harness::harness(&harness_id) else {
            diagnostics.push(format!("unknown harness {harness_id:?}; skills skipped"));
            continue;
        };
        let Some(layout) = harness.skill_layout() else {
            diagnostics.push(format!(
                "harness {harness_id:?} has no project skill layout; skills skipped"
            ));
            continue;
        };
        let root = match checked_layout_root(worktree, layout.project_root) {
            Ok(root) => root,
            Err(error) => {
                diagnostics.push(format!("{harness_id}: {error}"));
                continue;
            }
        };
        let survey = match survey_skill_root(&root) {
            Ok(survey) => survey,
            Err(error) => {
                diagnostics.push(format!("{harness_id}: {error}"));
                continue;
            }
        };
        diagnostics.extend(
            survey
                .diagnostics
                .into_iter()
                .map(|d| format!("{harness_id}: {d}")),
        );
        let target =
            thegn_core::skills::SeedTarget::new(&harness_id, phase, excludes.iter().cloned());
        let plan = plan_seed(&loaded.registry, &target, &survey.files, gate_state(cfg));
        diagnostics.extend(
            plan.diagnostics
                .into_iter()
                .map(|d| format!("{harness_id}: {d}")),
        );
        for entry in plan.unchanged {
            files.push(result_row(
                &harness_id,
                layout.project_root,
                &entry.relative,
                "current",
            ));
        }
        for entry in plan.skipped_unmarked {
            files.push(result_row(
                &harness_id,
                layout.project_root,
                &entry.relative,
                "preserved_unmarked",
            ));
        }
        for entry in plan.skipped_adopted {
            files.push(result_row(
                &harness_id,
                layout.project_root,
                &entry.relative,
                "preserved_modified",
            ));
        }
        for operation in plan.writes {
            let status = match operation.reason {
                WriteReason::Absent => "written",
                WriteReason::ChangedManaged => "updated",
            };
            let dest = root.join(&operation.relative);
            match atomic_write(&dest, &operation.contents) {
                Ok(()) => files.push(result_row(
                    &harness_id,
                    layout.project_root,
                    &operation.relative,
                    status,
                )),
                Err(error) => diagnostics.push(format!("{}: {error}", dest.display())),
            }
        }
        for operation in plan.removed_managed {
            let dest = root.join(&operation.relative);
            match std::fs::remove_file(&dest) {
                Ok(()) => {
                    if let Some(parent) = dest.parent() {
                        let _ = std::fs::remove_dir(parent); // best-effort: cleanup: only an empty package directory is removed
                    }
                    files.push(result_row(
                        &harness_id,
                        layout.project_root,
                        &operation.relative,
                        match operation.reason {
                            thegn_core::skills::RemoveReason::Excluded => "removed_excluded",
                            thegn_core::skills::RemoveReason::Deprecated => "removed_deprecated",
                        },
                    ));
                }
                Err(error) => diagnostics.push(format!("{}: {error}", dest.display())),
            }
        }

        if harness_id == "claude" {
            seed_mq_commands(cfg, worktree, &excludes, &mut files, &mut diagnostics);
        }

        let patterns = loaded
            .registry
            .iter()
            .map(|(name, _)| format!("{}/{name}/", layout.project_root.trim_end_matches('/')));
        exclude_locally(worktree, patterns, &mut diagnostics);
    }

    files.sort_by(|a, b| (&a.harness, &a.path, &a.status).cmp(&(&b.harness, &b.path, &b.status)));
    diagnostics.sort();
    diagnostics.dedup();
    Ok(SeedReport {
        worktree: worktree.display().to_string(),
        files,
        diagnostics,
    })
}

fn result_row(harness: &str, root: &str, relative: &str, status: &str) -> SeedFileResult {
    SeedFileResult {
        harness: harness.to_string(),
        path: format!("{}/{relative}", root.trim_end_matches('/')),
        status: status.to_string(),
    }
}

fn seed_mq_commands(
    cfg: &Config,
    worktree: &Path,
    excludes: &BTreeSet<String>,
    files: &mut Vec<SeedFileResult>,
    diagnostics: &mut Vec<String>,
) {
    for command in MQ_COMMANDS {
        let dest = worktree.join(command.relative);
        let existing = match std::fs::symlink_metadata(&dest) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => Some(Vec::new()),
            Ok(_) => match read_bounded(&dest) {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    diagnostics.push(format!("{}: {error}", dest.display()));
                    Some(Vec::new())
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                diagnostics.push(format!("{}: {error}", dest.display()));
                Some(Vec::new())
            }
        };
        if excludes.contains("mq") {
            if existing
                .as_deref()
                .and_then(inspect_managed)
                .is_some_and(|managed| !managed.is_user_modified())
            {
                match std::fs::remove_file(&dest) {
                    Ok(()) => files.push(SeedFileResult {
                        harness: "claude".into(),
                        path: command.relative.into(),
                        status: "removed_excluded".into(),
                    }),
                    Err(error) => diagnostics.push(format!("{}: {error}", dest.display())),
                }
            }
            continue;
        }
        if !cfg.merge_queue.enabled {
            continue;
        }
        let desired = render_managed_legacy(command.body);
        let desired_hash = hash(command.body.as_bytes());
        let status = match existing.as_deref() {
            None => Some("written"),
            Some(bytes) if bytes == command.body.as_bytes() => Some("updated"),
            Some(bytes) => match inspect_managed(bytes) {
                None => {
                    files.push(SeedFileResult {
                        harness: "claude".into(),
                        path: command.relative.into(),
                        status: "preserved_unmarked".into(),
                    });
                    None
                }
                Some(managed) if managed.is_user_modified() => {
                    files.push(SeedFileResult {
                        harness: "claude".into(),
                        path: command.relative.into(),
                        status: "preserved_modified".into(),
                    });
                    None
                }
                Some(managed) if managed.marker.recorded_hash == desired_hash => {
                    files.push(SeedFileResult {
                        harness: "claude".into(),
                        path: command.relative.into(),
                        status: "current".into(),
                    });
                    None
                }
                Some(_) => Some("updated"),
            },
        };
        if let Some(status) = status {
            match atomic_write(&dest, desired.as_bytes()) {
                Ok(()) => files.push(SeedFileResult {
                    harness: "claude".into(),
                    path: command.relative.into(),
                    status: status.into(),
                }),
                Err(error) => diagnostics.push(format!("{}: {error}", dest.display())),
            }
        }
    }
    exclude_locally(
        worktree,
        MQ_COMMANDS
            .iter()
            .map(|command| command.relative.to_string()),
        diagnostics,
    );
}

fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn render_managed_legacy(body: &str) -> String {
    let marker = format!(
        "thegn_managed: true\nthegn_version: {}\nthegn_hash: {}\n",
        thegn_core::skills::SHIPPING_VERSION,
        hash(body.as_bytes())
    );
    body.replacen("---\n", &format!("---\n{marker}"), 1)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    if std::fs::symlink_metadata(parent).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(std::io::Error::other("destination parent is a symlink"));
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("SKILL.md");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{name}.thegn-skill.{}.{nanos}.tmp",
        std::process::id()
    ));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions()); // best-effort: preserve existing permissions where supported
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp); // best-effort: cleanup after a failed atomic replace
            Err(error)
        }
    }
}

fn exclude_locally(
    worktree: &Path,
    patterns: impl IntoIterator<Item = String>,
    diagnostics: &mut Vec<String>,
) {
    let exclude = thegn_core::util::git_common_dir(worktree)
        .join("info")
        .join("exclude");
    let contents = std::fs::read_to_string(&exclude).unwrap_or_default();
    let mut missing: Vec<String> = patterns
        .into_iter()
        .filter(|pattern| !contents.lines().any(|line| line.trim() == pattern))
        .collect();
    missing.sort();
    missing.dedup();
    if missing.is_empty() {
        return;
    }
    if let Some(parent) = exclude.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        diagnostics.push(format!("{}: {error}", parent.display()));
        return;
    }
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude);
    let mut file = match opened {
        Ok(file) => file,
        Err(error) => {
            diagnostics.push(format!("{}: {error}", exclude.display()));
            return;
        }
    };
    if !contents.is_empty()
        && !contents.ends_with('\n')
        && let Err(error) = writeln!(file)
    {
        diagnostics.push(format!("{}: {error}", exclude.display()));
        return;
    }
    for pattern in missing {
        if let Err(error) = writeln!(file, "{pattern}") {
            diagnostics.push(format!("{}: {error}", exclude.display()));
            break;
        }
    }
}

/// Best-effort worktree-creation hook.
pub fn seed_if_enabled(cfg: &Config, worktree: &Path, phase: SeedPhase) {
    if cfg.skills.enabled {
        let _ = seed(cfg, worktree, phase); // best-effort: discoverability must not make worktree creation fail
    }
}

/// Reconcile persisted local worktrees on a background worker. No timer, wake,
/// or render-plan state is introduced.
pub fn seed_persisted_worktrees(cfg: &Config) {
    if !cfg.skills.enabled {
        return;
    }
    let cfg = cfg.clone();
    let _ = std::thread::Builder::new()
        .name("thegn-skill-seed".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            if let Ok(db) = thegn_core::db::Db::open() {
                use thegn_core::store::WorkspaceStore;
                for worktree in db.worktrees().unwrap_or_default() {
                    if worktree.location.is_empty() {
                        let _ = seed(&cfg, Path::new(&worktree.worktree), SeedPhase::Startup); // best-effort: startup reconciliation is diagnostic-only
                    }
                }
            }
        }); // best-effort: a failed background spawn only skips reconciliation
}

/// Metadata projection shared by the CLI and control route.
pub(crate) fn metadata(skill: &SkillDocument) -> thegn_svc::control::SkillInfo {
    thegn_svc::control::SkillInfo {
        name: skill.name.clone(),
        description: skill.description.clone(),
        harnesses: skill.harnesses.iter().cloned().collect(),
        gate: skill.gate.as_str().to_string(),
        when: skill
            .when
            .iter()
            .map(|phase| phase.as_str().to_string())
            .collect(),
        source: skill.source.clone(),
    }
}

/// Canonical, unmarked content for a read-only `skills show` call.
pub(crate) fn shown_document(skill: &SkillDocument) -> String {
    render_document(skill)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str, command: &str) -> thegn_core::config::NamedCommand {
        thegn_core::config::NamedCommand {
            name: name.into(),
            command: command.into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
            resume: false,
            route_via_proxy: false,
        }
    }

    fn worktree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git/info")).unwrap();
        std::fs::write(dir.path().join(".git/info/exclude"), "# local\n").unwrap();
        dir
    }

    #[test]
    fn discovers_immediate_valid_packages_and_builtin_wins_duplicates() {
        let user = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(user.path().join("demo")).unwrap();
        std::fs::write(
            user.path().join("demo/SKILL.md"),
            "---\nname: demo\ndescription: demo skill\nharnesses: codex\ngate: always\nwhen: explicit\n---\nbody\n",
        )
        .unwrap();
        std::fs::create_dir_all(user.path().join("mq")).unwrap();
        std::fs::write(
            user.path().join("mq/SKILL.md"),
            "---\nname: mq\ndescription: duplicate\nharnesses: claude\ngate: always\nwhen: explicit\n---\nbody\n",
        )
        .unwrap();
        let mut cfg = Config::default();
        cfg.skills.user_dirs = vec![user.path().display().to_string()];
        let loaded = load_registry(&cfg);
        assert!(loaded.registry.get("demo").is_some());
        assert_ne!(loaded.registry.get("mq").unwrap().description, "duplicate");
        assert!(
            loaded
                .diagnostics
                .iter()
                .any(|d| d.contains("duplicate skill"))
        );
    }

    #[test]
    fn seeds_each_configured_harness_in_its_native_layout() {
        let wt = worktree();
        let mut cfg = Config::default();
        cfg.agents = vec![
            named("claude", "claude"),
            named("codex", "codex"),
            named("pi", "pi"),
        ];
        cfg.merge_queue.enabled = true;
        cfg.pipeline.stages.push(Default::default());
        let report = seed(&cfg, wt.path(), SeedPhase::Explicit).unwrap();
        for root in [".claude/skills", ".agents/skills", ".pi/skills"] {
            assert!(wt.path().join(root).join("mq/SKILL.md").is_file(), "{root}");
        }
        assert!(wt.path().join(".claude/commands/mq-add.md").is_file());
        assert!(report.files.iter().any(|row| row.status == "written"));
        let second = seed(&cfg, wt.path(), SeedPhase::Explicit).unwrap();
        assert!(second.files.iter().all(|row| row.status == "current"));
    }

    #[test]
    fn exact_legacy_commands_migrate_but_changed_files_survive() {
        let wt = worktree();
        let mut cfg = Config::default();
        cfg.merge_queue.enabled = true;
        std::fs::create_dir_all(wt.path().join(".claude/commands")).unwrap();
        std::fs::write(
            wt.path().join(".claude/commands/mq-add.md"),
            MQ_COMMANDS[0].body,
        )
        .unwrap();
        std::fs::write(
            wt.path().join(".claude/commands/mq-drain.md"),
            "user command",
        )
        .unwrap();
        let report = seed(&cfg, wt.path(), SeedPhase::Explicit).unwrap();
        assert!(
            inspect_managed(&std::fs::read(wt.path().join(".claude/commands/mq-add.md")).unwrap())
                .is_some()
        );
        assert_eq!(
            std::fs::read_to_string(wt.path().join(".claude/commands/mq-drain.md")).unwrap(),
            "user command"
        );
        assert!(
            report
                .files
                .iter()
                .any(|row| row.path.ends_with("mq-drain.md") && row.status == "preserved_unmarked")
        );
    }

    #[test]
    fn malformed_user_package_is_diagnostic_and_does_not_block_builtins() {
        let user = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(user.path().join("bad")).unwrap();
        std::fs::write(user.path().join("bad/SKILL.md"), "not frontmatter").unwrap();
        let wt = worktree();
        let mut cfg = Config::default();
        cfg.skills.user_dirs = vec![user.path().display().to_string()];
        let report = seed(&cfg, wt.path(), SeedPhase::Explicit).unwrap();
        assert!(report.diagnostics.iter().any(|d| d.contains("frontmatter")));
        assert!(
            wt.path()
                .join(".claude/skills/supervise/SKILL.md")
                .is_file()
        );
    }
}
