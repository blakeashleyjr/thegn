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
    /// False when configured discovery could not prove that the returned
    /// registry is exhaustive. Deprecated-file removal is unsafe in that case.
    pub complete: bool,
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
    /// False when an entry could have been omitted by the package bound.
    pub complete: bool,
}

/// Build the trusted-first registry. Each configured directory contributes
/// only immediate `<package>/SKILL.md` children; built-ins always win names.
pub(crate) fn load_registry(cfg: &Config) -> RegistryLoad {
    let mut diagnostics = Vec::new();
    let mut complete = true;
    let mut registry = match SkillRegistry::embedded() {
        Ok(registry) => registry,
        Err(error) => {
            complete = false;
            diagnostics.push(format!("embedded skill registry: {error}"));
            SkillRegistry::new()
        }
    };

    for raw_dir in &cfg.skills.user_dirs {
        let dir = PathBuf::from(thegn_core::util::expand_tilde(raw_dir));
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                complete = false;
                diagnostics.push(format!("{}: {error}", dir.display()));
                continue;
            }
        };
        let mut packages: Vec<_> = entries.take(MAX_PACKAGES_PER_DIR + 1).collect();
        if packages.len() > MAX_PACKAGES_PER_DIR {
            complete = false;
            diagnostics.push(format!(
                "{}: more than {MAX_PACKAGES_PER_DIR} immediate packages; directory skipped",
                dir.display()
            ));
            continue;
        }
        packages.sort_by_key(|entry| entry.as_ref().ok().map(std::fs::DirEntry::file_name));
        for entry in packages {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    complete = false;
                    diagnostics.push(format!("{}: {error}", dir.display()));
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    complete = false;
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
                complete = false;
                diagnostics.push(format!(
                    "{}: package name is not UTF-8",
                    entry.path().display()
                ));
                continue;
            };
            // Validate the observed directory segment before using it to build
            // the document path. Frontmatter is never trusted as a path input.
            if let Err(error) = validate_name(&package_name) {
                complete = false;
                diagnostics.push(format!("{}: {error}", entry.path().display()));
                continue;
            }
            let path = entry.path().join("SKILL.md");
            let bytes = match read_bounded(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    complete = false;
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
                Err(error) => {
                    complete = false;
                    diagnostics.push(error.to_string());
                }
            }
        }
    }
    RegistryLoad {
        registry,
        diagnostics,
        complete,
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
            complete: true,
        });
    }
    let entries = std::fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let mut entries: Vec<_> = entries.take(MAX_PACKAGES_PER_DIR + 1).collect();
    let mut files = Vec::new();
    let mut diagnostics = Vec::new();
    if entries.len() > MAX_PACKAGES_PER_DIR {
        diagnostics.push(format!(
            "{}: more than {MAX_PACKAGES_PER_DIR} immediate packages; target preserved",
            root.display()
        ));
        return Ok(SkillSurvey {
            files,
            diagnostics,
            directory_found,
            complete: false,
        });
    }
    entries.sort_by_key(|entry| entry.as_ref().ok().map(std::fs::DirEntry::file_name));
    let mut complete = true;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                complete = false;
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
        complete,
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
    let canonical_worktree = std::fs::canonicalize(worktree)?;
    if thegn_core::repo::worktree_root_for_cwd(&canonical_worktree).as_deref()
        != Some(canonical_worktree.as_path())
    {
        bail!(
            "skills seed target is not a git worktree root: {}",
            worktree.display()
        );
    }
    let loaded = load_registry(cfg);
    let (harnesses, mut diagnostics) = configured_harnesses(cfg);
    diagnostics.extend(loaded.diagnostics);
    let mut files = Vec::new();
    let mut access_failures = Vec::new();
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
                access_failures.push(format!("{harness_id}: {error}"));
                continue;
            }
        };
        let survey = match survey_skill_root(&root) {
            Ok(survey) => survey,
            Err(error) => {
                diagnostics.push(format!("{harness_id}: {error}"));
                access_failures.push(format!("{harness_id}: {error}"));
                continue;
            }
        };
        diagnostics.extend(
            survey
                .diagnostics
                .into_iter()
                .map(|d| format!("{harness_id}: {d}")),
        );
        if !survey.complete {
            diagnostics.push(format!(
                "{harness_id}: skill-root survey was incomplete; target preserved"
            ));
            continue;
        }
        let target =
            thegn_core::skills::SeedTarget::new(&harness_id, phase, excludes.iter().cloned());
        let mut plan = plan_seed(&loaded.registry, &target, &survey.files, gate_state(cfg));
        if !loaded.complete {
            plan.removed_managed.retain(|operation| {
                operation.reason != thegn_core::skills::RemoveReason::Deprecated
            });
            diagnostics.push(
                "skill registry discovery was incomplete; deprecated managed entries preserved"
                    .to_string(),
            );
        }
        diagnostics.extend(
            plan.diagnostics
                .into_iter()
                .map(|d| format!("{harness_id}: {d}")),
        );
        let mut managed_patterns = BTreeSet::new();
        let mut user_patterns = BTreeSet::new();
        for entry in plan.unchanged {
            managed_patterns.insert(skill_exclude_pattern(layout.project_root, &entry.relative));
            files.push(result_row(
                &harness_id,
                layout.project_root,
                &entry.relative,
                "current",
            ));
        }
        for entry in plan.skipped_unmarked {
            user_patterns.insert(skill_exclude_pattern(layout.project_root, &entry.relative));
            files.push(result_row(
                &harness_id,
                layout.project_root,
                &entry.relative,
                "preserved_unmarked",
            ));
        }
        for entry in plan.skipped_adopted {
            user_patterns.insert(skill_exclude_pattern(layout.project_root, &entry.relative));
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
                Ok(()) => {
                    managed_patterns.insert(skill_exclude_pattern(
                        layout.project_root,
                        &operation.relative,
                    ));
                    files.push(result_row(
                        &harness_id,
                        layout.project_root,
                        &operation.relative,
                        status,
                    ));
                }
                Err(error) => {
                    let message = format!("{}: {error}", dest.display());
                    diagnostics.push(message.clone());
                    access_failures.push(message);
                }
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
                Err(error) => {
                    let message = format!("{}: {error}", dest.display());
                    diagnostics.push(message.clone());
                    access_failures.push(message);
                }
            }
        }

        if harness_id == "claude" {
            seed_mq_commands(
                cfg,
                worktree,
                &excludes,
                &mut files,
                &mut diagnostics,
                &mut access_failures,
            );
        }

        exclude_locally(worktree, managed_patterns, user_patterns, &mut diagnostics);
    }

    if !access_failures.is_empty() {
        bail!(
            "skills seed could not access target: {}",
            access_failures.join("; ")
        );
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

fn skill_exclude_pattern(root: &str, relative: &str) -> String {
    let package = relative
        .strip_suffix("/SKILL.md")
        .expect("seed plans contain canonical skill-relative paths");
    format!("{}/{package}/", root.trim_end_matches('/'))
}

fn seed_mq_commands(
    cfg: &Config,
    worktree: &Path,
    excludes: &BTreeSet<String>,
    files: &mut Vec<SeedFileResult>,
    diagnostics: &mut Vec<String>,
    access_failures: &mut Vec<String>,
) {
    let mut managed_patterns = Vec::new();
    for command in MQ_COMMANDS {
        let mut managed_path = false;
        let dest = worktree.join(command.relative);
        let existing = match std::fs::symlink_metadata(&dest) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => Some(Vec::new()),
            Ok(_) => match read_bounded(&dest) {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    let message = format!("{}: {error}", dest.display());
                    diagnostics.push(message.clone());
                    access_failures.push(message);
                    Some(Vec::new())
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                let message = format!("{}: {error}", dest.display());
                diagnostics.push(message.clone());
                access_failures.push(message);
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
                    Err(error) => {
                        let message = format!("{}: {error}", dest.display());
                        diagnostics.push(message.clone());
                        access_failures.push(message);
                    }
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
                    managed_path = true;
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
                Ok(()) => {
                    managed_path = true;
                    files.push(SeedFileResult {
                        harness: "claude".into(),
                        path: command.relative.into(),
                        status: status.into(),
                    });
                }
                Err(error) => {
                    let message = format!("{}: {error}", dest.display());
                    diagnostics.push(message.clone());
                    access_failures.push(message);
                }
            }
        }
        if managed_path {
            managed_patterns.push(command.relative.to_string());
        }
    }
    exclude_locally(worktree, managed_patterns, std::iter::empty(), diagnostics);
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
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&tmp); // best-effort: remove our incomplete temp file
        return Err(error);
    }
    drop(file);
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
    add_patterns: impl IntoIterator<Item = String>,
    remove_patterns: impl IntoIterator<Item = String>,
    diagnostics: &mut Vec<String>,
) {
    let exclude = thegn_core::util::git_common_dir(worktree)
        .join("info")
        .join("exclude");
    if std::fs::symlink_metadata(&exclude).is_ok_and(|meta| meta.file_type().is_symlink()) {
        diagnostics.push(format!(
            "{}: is a symlink; local excludes skipped",
            exclude.display()
        ));
        return;
    }
    let contents = match std::fs::read_to_string(&exclude) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            diagnostics.push(format!("{}: {error}", exclude.display()));
            return;
        }
    };
    let remove: BTreeSet<String> = remove_patterns.into_iter().collect();
    let mut updated = String::with_capacity(contents.len());
    for line in contents.split_inclusive('\n') {
        if !remove.contains(line.trim()) {
            updated.push_str(line);
        }
    }
    let mut missing: Vec<String> = add_patterns
        .into_iter()
        .filter(|pattern| !updated.lines().any(|line| line.trim() == pattern))
        .collect();
    missing.sort();
    missing.dedup();
    if missing.is_empty() && updated == contents {
        return;
    }
    if let Some(parent) = exclude.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        diagnostics.push(format!("{}: {error}", parent.display()));
        return;
    }
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    for pattern in missing {
        updated.push_str(&pattern);
        updated.push('\n');
    }
    if let Err(error) = atomic_write(&exclude, updated.as_bytes()) {
        diagnostics.push(format!("{}: {error}", exclude.display()));
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

    /// Extract literal `thegn ...` command lines from fenced blocks. This is a
    /// deliberately small shell lexer: it keeps quoted flag values together,
    /// joins `\` continuations, and stops before comments/pipelines/pseudocode.
    fn fenced_thegn_argv(document: &str) -> Result<Vec<Vec<String>>, String> {
        let mut commands = Vec::new();
        let mut in_fence = false;
        let mut logical = String::new();
        for line in document.lines() {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if !in_fence {
                continue;
            }
            let trimmed = line.trim();
            if logical.is_empty() && !trimmed.starts_with("thegn ") {
                continue;
            }
            let continued = trimmed.ends_with('\\');
            if !logical.is_empty() {
                logical.push(' ');
            }
            logical.push_str(trimmed.trim_end_matches('\\').trim_end());
            if continued {
                continue;
            }
            commands.push(shell_argv(&logical)?);
            logical.clear();
        }
        if !logical.is_empty() {
            return Err(format!("unterminated command continuation: {logical}"));
        }
        Ok(commands)
    }

    fn shell_argv(line: &str) -> Result<Vec<String>, String> {
        let mut argv = Vec::new();
        let mut token = String::new();
        let mut quote = None;
        let mut escaped = false;
        for ch in line.chars() {
            if escaped {
                token.push(ch);
                escaped = false;
                continue;
            }
            match quote {
                Some(end) if ch == end => quote = None,
                Some(_) if ch == '\\' => escaped = true,
                Some(_) => token.push(ch),
                None if matches!(ch, '\'' | '"') => quote = Some(ch),
                None if ch == '#' => break,
                None if ch.is_whitespace() => {
                    if !token.is_empty() {
                        argv.push(std::mem::take(&mut token));
                    }
                }
                None => token.push(ch),
            }
        }
        if let Some(end) = quote {
            return Err(format!("unterminated {end} quote in {line:?}"));
        }
        if !token.is_empty() {
            argv.push(token);
        }
        if let Some(stop) = argv
            .iter()
            .position(|arg| matches!(arg.as_str(), "|" | "||" | "&&" | ";" | "->"))
        {
            argv.truncate(stop);
        }
        Ok(argv)
    }

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
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
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
    fn preserved_unmarked_skill_is_not_hidden_from_git_status() {
        let wt = worktree();
        let path = wt.path().join(".claude/skills/supervise/SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "user-owned skill\n").unwrap();
        std::fs::write(
            wt.path().join(".git/info/exclude"),
            "# local\n.claude/skills/supervise/\n",
        )
        .unwrap();

        let report = seed(&Config::default(), wt.path(), SeedPhase::Explicit).unwrap();
        assert!(report.files.iter().any(|row| {
            row.path.ends_with("supervise/SKILL.md") && row.status == "preserved_unmarked"
        }));
        let exclude = std::fs::read_to_string(wt.path().join(".git/info/exclude")).unwrap();
        assert!(!exclude.contains(".claude/skills/supervise/"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "user-owned skill\n");
    }

    #[test]
    fn oversized_skill_root_is_bounded_and_preserved() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=MAX_PACKAGES_PER_DIR {
            std::fs::write(root.path().join(format!("entry-{index:04}")), []).unwrap();
        }
        let survey = survey_skill_root(root.path()).unwrap();
        assert!(!survey.complete);
        assert!(survey.files.is_empty());
        assert!(
            survey
                .diagnostics
                .iter()
                .any(|row| row.contains("target preserved"))
        );
    }

    #[test]
    fn malformed_user_package_is_diagnostic_and_does_not_block_builtins() {
        let user = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(user.path().join("bad")).unwrap();
        std::fs::write(user.path().join("bad/SKILL.md"), "not frontmatter").unwrap();
        let wt = worktree();
        let prior = parse_document(
            b"---\nname: bad\ndescription: prior valid package\nharnesses: claude\ngate: always\nwhen: explicit\n---\nbody\n",
            "bad",
            SkillSource::user("prior/bad/SKILL.md"),
        )
        .unwrap();
        let installed = thegn_core::skills::render_managed(&prior);
        let installed_path = wt.path().join(".claude/skills/bad/SKILL.md");
        std::fs::create_dir_all(installed_path.parent().unwrap()).unwrap();
        std::fs::write(&installed_path, &installed).unwrap();
        let mut cfg = Config::default();
        cfg.skills.user_dirs = vec![user.path().display().to_string()];
        let report = seed(&cfg, wt.path(), SeedPhase::Explicit).unwrap();
        assert!(report.diagnostics.iter().any(|d| d.contains("frontmatter")));
        assert_eq!(
            std::fs::read_to_string(installed_path).unwrap(),
            installed,
            "an invalid package must not be mistaken for a retired package"
        );
        assert!(
            wt.path()
                .join(".claude/skills/supervise/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn explicit_seed_rejects_an_ordinary_directory() {
        let directory = tempfile::tempdir().unwrap();
        let error = seed(&Config::default(), directory.path(), SeedPhase::Explicit).unwrap_err();
        assert!(error.to_string().contains("not a git worktree"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn explicit_seed_rejects_a_fake_dot_git_marker() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let error = seed(&Config::default(), directory.path(), SeedPhase::Explicit).unwrap_err();
        assert!(error.to_string().contains("not a git worktree root"));
        assert!(!directory.path().join(".claude").exists());
    }

    #[test]
    fn explicit_seed_propagates_an_inaccessible_layout_root() {
        let wt = worktree();
        std::fs::write(wt.path().join(".claude"), "blocks the skill directory").unwrap();
        let error = seed(&Config::default(), wt.path(), SeedPhase::Explicit).unwrap_err();
        assert!(error.to_string().contains("could not access target"));
        assert!(error.to_string().contains("claude"));
    }

    /// Shipped prose is operational UI: every fenced command must continue to
    /// parse after CLI renames, including its documented flags and arguments.
    #[test]
    fn embedded_skill_commands_parse_against_the_live_cli() {
        use clap::Parser as _;

        let mut checked = 0;
        let mut failures = Vec::new();
        for entry in thegn_core::skills::EMBEDDED_MANIFEST {
            let commands = fenced_thegn_argv(entry.document)
                .unwrap_or_else(|error| panic!("{}: {error}", entry.origin));
            for mut argv in commands {
                checked += 1;
                for arg in &mut argv {
                    if matches!(arg.as_str(), "{row}" | "row" | "<row-id>" | "<dispatch-id>") {
                        *arg = "1".to_string();
                    }
                }
                if let Err(error) = crate::Cli::try_parse_from(&argv) {
                    failures.push(format!(
                        "{}: `{}`: {}",
                        entry.origin,
                        argv.join(" "),
                        error.render().ansi().to_string().trim()
                    ));
                }
            }
        }
        assert!(checked > 0, "embedded registry contains no fenced commands");
        assert!(
            failures.is_empty(),
            "embedded skill commands drifted from the live CLI:\n{}",
            failures.join("\n")
        );
    }
}
