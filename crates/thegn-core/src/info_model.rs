//! The Info screen's model. One `InfoReport` feeds the TUI popup and `thegn info --json`, so the two cannot drift. Every value carries the layer that set it, or says not measured or unknown; never a guess.

use serde::Serialize;

pub const INFO_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "layer", content = "detail", rename_all = "snake_case")]
pub enum Source {
    Builtin,
    Global,
    ProfileFile,
    Runtime,
    LoadedDiffers,
    ProfileTable(String),
    Workspace(String),
    WorkspaceDb,
    Repo(String),
    Env(String),
    Placement,
    Zone(String),
    WorktreeDb,
    WorktreePick,
    Probe,
    Recorded,
    Cache { name: String, at: Option<i64> },
    Git,
    Daemon,
    Database,
    Sidebar,
}

impl Source {
    /// Human label.
    pub fn label(&self, now: i64) -> String {
        match self {
            Self::Builtin => "builtin".to_string(),
            Self::Global => "global".to_string(),
            Self::ProfileFile => "profile file".to_string(),
            Self::Runtime => "runtime".to_string(),
            Self::LoadedDiffers => "loaded (differs)".to_string(),
            Self::ProfileTable(name) if name.is_empty() => "profile table".to_string(),
            Self::ProfileTable(name) => format!("profile table {name}"),
            Self::Workspace(name) if name.is_empty() => "workspace".to_string(),
            Self::Workspace(name) => format!("workspace {name}"),
            Self::WorkspaceDb => "workspace db".to_string(),
            Self::Repo(path) if path.is_empty() => "repo".to_string(),
            Self::Repo(path) => format!("repo {path}"),
            Self::Env(name) if name.is_empty() => "env".to_string(),
            Self::Env(name) => format!("env {name}"),
            Self::Placement => "placement".to_string(),
            Self::Zone(name) if name.is_empty() => "zone".to_string(),
            Self::Zone(name) => format!("zone {name}"),
            Self::WorktreeDb => "worktree db".to_string(),
            Self::WorktreePick => "worktree pick".to_string(),
            Self::Probe => "probed now".to_string(),
            Self::Recorded => "recorded".to_string(),
            Self::Cache { name, at: Some(at) } => format!("cache {name}, {}", age(now, *at)),
            Self::Cache { name, at: None } => format!("cache {name}"),
            Self::Git => "git".to_string(),
            Self::Daemon => "daemon".to_string(),
            Self::Database => "database".to_string(),
            Self::Sidebar => "sidebar".to_string(),
        }
    }

    /// Convert a configuration trust layer into its Info source.
    pub fn from_trust(level: crate::config_resolve::TrustLevel) -> Source {
        use crate::config_resolve::TrustLevel;

        match level {
            TrustLevel::Builtin => Self::Builtin,
            TrustLevel::UserGlobal => Self::Global,
            TrustLevel::Profile => Self::ProfileFile,
            TrustLevel::Runtime => Self::Runtime,
            TrustLevel::Zone => Self::Zone(String::new()),
            TrustLevel::Workspace => Self::Workspace(String::new()),
            TrustLevel::Repo => Self::Repo(String::new()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum Shown {
    Value(String),
    Unset,
    NotMeasured,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Field {
    pub key: String,
    pub label: String,
    pub shown: Shown,
    pub source: Option<Source>,
    pub warn: bool,
}

impl Field {
    pub fn value(key: &str, label: &str, value: impl Into<String>, source: Source) -> Field {
        Field {
            key: key.to_string(),
            label: label.to_string(),
            shown: Shown::Value(value.into()),
            source: Some(source),
            warn: false,
        }
    }

    pub fn unset(key: &str, label: &str, source: Source) -> Field {
        Field {
            key: key.to_string(),
            label: label.to_string(),
            shown: Shown::Unset,
            source: Some(source),
            warn: false,
        }
    }

    pub fn not_measured(key: &str, label: &str) -> Field {
        Field {
            key: key.to_string(),
            label: label.to_string(),
            shown: Shown::NotMeasured,
            source: None,
            warn: false,
        }
    }

    pub fn unknown(key: &str, label: &str, why: impl Into<String>) -> Field {
        Field {
            key: key.to_string(),
            label: label.to_string(),
            shown: Shown::Unknown(why.into()),
            source: None,
            warn: false,
        }
    }

    pub fn warned(mut self) -> Field {
        self.warn = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackendRow {
    pub name: String,
    pub state: String,
    pub isolation: Option<String>,
    pub meets_floor: Option<bool>,
    pub chosen: bool,
    pub remedy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRow {
    pub id: String,
    pub program: String,
    pub worktree: Option<String>,
    pub attached: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InfoTarget {
    pub repo_root: String,
    pub worktree: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GitFacts {
    pub dirty_files: Option<u32>,
    pub ahead_base: Option<u32>,
    pub behind_base: Option<u32>,
    pub ahead_upstream: Option<u32>,
    pub behind_upstream: Option<u32>,
    pub upstream: Option<String>,
}

/// What the caller already knows (TUI: the sidebar row and daemon list) or may read (CLI).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveFacts {
    pub read_git: bool,
    pub git: Option<GitFacts>,
    pub git_source: Option<Source>,
    pub activity: Option<String>,
    pub pipeline_stage: Option<String>,
    pub devcontainer: Option<String>,
    /// Every live session (unfiltered). None = not measured.
    pub sessions: Option<Vec<SessionRow>>,
    pub sessions_source: Option<Source>,
    /// (worktrees ahead of base, worktrees with a known count)
    pub workspace_ahead: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InfoReport {
    pub schema: u32,
    pub generated_at: i64,
    pub target: InfoTarget,
    pub workspace: Vec<Field>,
    pub workspace_stats: Vec<Field>,
    pub worktree: Vec<Field>,
    pub sessions: Option<Vec<SessionRow>>,
    pub settings_scope: String,
    pub settings: Vec<Field>,
    pub backends: Vec<BackendRow>,
    pub overrides: Vec<Field>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoLine {
    pub label: String,
    pub value: String,
    pub source: String,
    pub tone: Tone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoSection {
    pub title: String,
    pub lines: Vec<InfoLine>,
}

/// WORKSPACE, WORKTREE (when present), EFFECTIVE SETTINGS (`<scope>`), BACKEND CANDIDATES,
/// OVERRIDES, SESSIONS, STATS, NOTES; empty sections are skipped.
pub fn sections(report: &InfoReport) -> Vec<InfoSection> {
    let mut result = Vec::new();
    push_fields(
        &mut result,
        "WORKSPACE",
        &report.workspace,
        report.generated_at,
    );
    push_fields(
        &mut result,
        "WORKTREE",
        &report.worktree,
        report.generated_at,
    );
    push_fields(
        &mut result,
        &format!("EFFECTIVE SETTINGS ({})", report.settings_scope),
        &report.settings,
        report.generated_at,
    );

    if !report.backends.is_empty() {
        let lines = report
            .backends
            .iter()
            .map(|backend| {
                let mut value = backend.state.clone();
                if let Some(isolation) = &backend.isolation {
                    value.push_str(", ");
                    value.push_str(isolation);
                }
                match backend.meets_floor {
                    Some(true) => value.push_str(", meets floor"),
                    Some(false) => value.push_str(", below floor"),
                    None => {}
                }
                if backend.chosen {
                    value.push_str(" (chosen)");
                }
                InfoLine {
                    label: backend.name.clone(),
                    value,
                    source: if backend.meets_floor.is_some() {
                        "probed now".to_string()
                    } else {
                        "not probed".to_string()
                    },
                    tone: Tone::Normal,
                }
            })
            .collect();
        result.push(InfoSection {
            title: "BACKEND CANDIDATES".to_string(),
            lines,
        });
    }

    push_fields(
        &mut result,
        "OVERRIDES",
        &report.overrides,
        report.generated_at,
    );

    if let Some(sessions) = &report.sessions
        && !sessions.is_empty()
    {
        result.push(InfoSection {
            title: "SESSIONS".to_string(),
            lines: sessions
                .iter()
                .map(|session| InfoLine {
                    label: session.program.clone(),
                    value: format!("{}, attached {}", session.id, session.attached),
                    source: "".to_string(),
                    tone: Tone::Normal,
                })
                .collect(),
        });
    }

    push_fields(
        &mut result,
        "STATS",
        &report.workspace_stats,
        report.generated_at,
    );

    if !report.notes.is_empty() {
        result.push(InfoSection {
            title: "NOTES".to_string(),
            lines: report
                .notes
                .iter()
                .map(|note| InfoLine {
                    label: "note".to_string(),
                    value: note.clone(),
                    source: "".to_string(),
                    tone: Tone::Normal,
                })
                .collect(),
        });
    }

    result
}

fn push_fields(sections: &mut Vec<InfoSection>, title: &str, fields: &[Field], generated_at: i64) {
    if fields.is_empty() {
        return;
    }
    sections.push(InfoSection {
        title: title.to_string(),
        lines: fields
            .iter()
            .map(|field| field_line(field, generated_at))
            .collect(),
    });
}

fn field_line(field: &Field, generated_at: i64) -> InfoLine {
    let (value, tone) = match &field.shown {
        Shown::Value(value) => (
            value.clone(),
            if field.warn { Tone::Warn } else { Tone::Normal },
        ),
        Shown::Unset => ("(unset)".to_string(), Tone::Muted),
        Shown::NotMeasured => ("not measured".to_string(), Tone::Muted),
        Shown::Unknown(why) => (format!("unknown: {why}"), Tone::Muted),
    };
    InfoLine {
        label: field.label.clone(),
        value,
        source: field
            .source
            .as_ref()
            .map(|source| source.label(generated_at))
            .unwrap_or_default(),
        tone,
    }
}

/// Plain aligned text (no ANSI) built from `sections`: `TITLE` lines, then
/// `  label  value  source`, columns padded to the widest label (cap 18) and value (cap 48).
pub fn render_text(report: &InfoReport) -> String {
    let sections = sections(report);
    let mut rendered = Vec::new();
    for section in sections {
        let label_width = section
            .lines
            .iter()
            .map(|line| clean(&line.label, 18).chars().count())
            .max()
            .unwrap_or(0);
        let value_width = section
            .lines
            .iter()
            .map(|line| clean(&line.value, 48).chars().count())
            .max()
            .unwrap_or(0);
        rendered.push(section.title);
        for line in section.lines {
            let label = clean(&line.label, 18);
            let value = clean(&line.value, 48);
            let source = clean(&line.source, 48);
            let mut text = format!(
                "  {:<label_width$}  {:<value_width$}",
                label,
                value,
                label_width = label_width,
                value_width = value_width,
            );
            if !source.is_empty() {
                text.push_str("  ");
                text.push_str(&source);
            }
            rendered.push(text);
        }
    }
    rendered.join("\n")
}

/// `config explain`'s last-differing-stage rule.
pub fn origin_of(stages: &[(Source, serde_json::Value)]) -> Option<Source> {
    let first = stages.first()?;
    let mut origin = first.0.clone();
    for (index, (source, value)) in stages.iter().enumerate().skip(1) {
        if value != &stages[index - 1].1 {
            origin = source.clone();
        }
    }
    Some(origin)
}

/// Redact and render a config value in the same compact style as `config get`.
pub fn config_value_text(key: &str, value: serde_json::Value) -> String {
    let segments: Vec<&str> = key.split('.').collect();
    let last = sensitive_segment(key).unwrap_or_else(|| segments.last().copied().unwrap_or(key));
    let mut wrapped = serde_json::json!({ last: value });
    if sensitive_segment(key).is_some() {
        crate::redact::redact_json(&mut wrapped);
    } else {
        crate::redact::redact_json_at(&segments[..segments.len().saturating_sub(1)], &mut wrapped);
    }
    let value = wrapped
        .get_mut(last)
        .map(serde_json::Value::take)
        .unwrap_or(serde_json::Value::Null);
    render_config_value(value)
}

fn sensitive_segment(key: &str) -> Option<&str> {
    key.split('.')
        .find(|segment| crate::redact::is_sensitive(segment))
}

fn render_config_value(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value,
        serde_json::Value::Null => String::new(),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(render_config_value)
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// Drop `user[:pass]@` from the authority of a URL; scp-style `git@host:path` is kept.
pub fn scrub_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map(|offset| authority_start + offset)
        .unwrap_or(url.len());
    let authority = &url[authority_start..authority_end];
    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };
    format!(
        "{}{}{}",
        &url[..authority_start],
        &authority[at + 1..],
        &url[authority_end..]
    )
}

/// Control characters dropped, clipped to `max` chars with a trailing "…".
pub fn clean(text: &str, max: usize) -> String {
    let cleaned: String = text
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '\u{200B}'..='\u{200F}'
                        | '\u{202A}'..='\u{202E}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{FEFF}'
                )
        })
        .collect();
    let count = cleaned.chars().count();
    if count <= max {
        return cleaned;
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return "…".to_string();
    }
    let mut clipped: String = cleaned.chars().take(max - 1).collect();
    clipped.push('…');
    clipped
}

/// Human age for a timestamp in seconds.
pub fn age(now: i64, then: i64) -> String {
    if then > now {
        return "in the future".to_string();
    }
    let seconds = now.saturating_sub(then);
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 60 * 60 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 24 * 60 * 60 {
        format!("{}h ago", seconds / (60 * 60))
    } else {
        format!("{}d ago", seconds / (24 * 60 * 60))
    }
}

pub mod keys {
    pub const WORKSPACE_NAME: &str = "workspace.name";
    pub const WORKSPACE_SLUG: &str = "workspace.slug";
    pub const WORKSPACE_REPO: &str = "workspace.repo";
    pub const WORKSPACE_KIND: &str = "workspace.kind";
    pub const WORKSPACE_DEFAULT_BRANCH: &str = "workspace.default_branch";
    pub const WORKSPACE_ORIGIN: &str = "workspace.origin";
    pub const WORKSPACE_BLOCK: &str = "workspace.block";
    pub const WORKSPACE_REPO_OVERLAY: &str = "workspace.repo_overlay";

    pub const STATS_WORKTREES: &str = "stats.worktrees";
    pub const STATS_SESSIONS: &str = "stats.sessions";
    pub const STATS_DISK: &str = "stats.disk";
    pub const STATS_AHEAD: &str = "stats.ahead";
    pub const STATS_LAST_ACTIVE: &str = "stats.last_active";

    pub const WORKTREE_PATH: &str = "worktree.path";
    pub const WORKTREE_BRANCH: &str = "worktree.branch";
    pub const WORKTREE_BASE: &str = "worktree.base";
    pub const WORKTREE_UPSTREAM: &str = "worktree.upstream";
    pub const WORKTREE_AHEAD_BASE: &str = "worktree.ahead_base";
    pub const WORKTREE_AHEAD_UPSTREAM: &str = "worktree.ahead_upstream";
    pub const WORKTREE_DIRTY: &str = "worktree.dirty";
    pub const WORKTREE_PR: &str = "worktree.pr";
    pub const WORKTREE_CI: &str = "worktree.ci";
    pub const WORKTREE_ISSUE: &str = "worktree.issue";
    pub const WORKTREE_DISPATCH: &str = "worktree.dispatch";
    pub const WORKTREE_STAGE: &str = "worktree.stage";
    pub const WORKTREE_MERGE_QUEUE: &str = "worktree.merge_queue";
    pub const WORKTREE_CREATED: &str = "worktree.created";
    pub const WORKTREE_DISK: &str = "worktree.disk";
    pub const WORKTREE_LOCATION: &str = "worktree.location";
    pub const WORKTREE_AGENT: &str = "worktree.agent";
    pub const WORKTREE_ACTIVITY: &str = "worktree.activity";
    pub const WORKTREE_DEVCONTAINER: &str = "worktree.devcontainer";
    pub const WORKTREE_OBSERVED_BACKEND: &str = "worktree.observed_backend";

    pub const ENV_NAME: &str = "env.name";
    pub const ENV_PLACEMENT: &str = "env.placement";
    pub const ENV_PROVIDER: &str = "env.provider";
    pub const ENV_DATA: &str = "env.data";
    pub const SANDBOX_ENABLED: &str = "sandbox.enabled";
    pub const SANDBOX_BACKEND: &str = "sandbox.backend";
    pub const SANDBOX_PROFILE: &str = "sandbox.profile";
    pub const SANDBOX_NETWORK: &str = "sandbox.network";
    pub const SANDBOX_DEFAULT_ENV: &str = "sandbox.default_env";
    pub const SANDBOX_MAIN_ENV: &str = "sandbox.main_env";
    pub const SANDBOX_LAUNCH_BACKEND: &str = "sandbox.launch_backend";
    pub const SANDBOX_SELECTED_BACKEND: &str = "sandbox.selected_backend";
    pub const SANDBOX_ISOLATION_FLOOR: &str = "sandbox.isolation_floor";
    pub const SANDBOX_ON_FLOOR_MISS: &str = "sandbox.on_floor_miss";
    pub const AGENTS_DEFAULT: &str = "agents.default";
    pub const MERGE_QUEUE_GATE_COMMAND: &str = "merge_queue.gate_command";
    pub const WORKTREES_DIR: &str = "worktrees_dir";
    pub const ISSUES_PROVIDER: &str = "issues.provider";

    pub const OVERRIDE_WORKTREE_ENV: &str = "override.worktree_env";
    pub const OVERRIDE_PROFILE_PIN: &str = "override.profile_pin";
    pub const OVERRIDE_BACKEND_PICK: &str = "override.backend_pick";
    pub const OVERRIDE_WORKSPACE_ENV: &str = "override.workspace_env";
    pub const OVERRIDE_WORKSPACE_BLOCK: &str = "override.workspace_block";
    pub const OVERRIDE_REPO_OVERLAY: &str = "override.repo_overlay";
    pub const OVERRIDE_ZONE: &str = "override.zone";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn origin_of_table() {
        let builtin = Source::Builtin;
        let global = Source::Global;
        let profile = Source::ProfileFile;
        assert_eq!(origin_of(&[]), None);
        assert_eq!(
            origin_of(&[(builtin.clone(), json!(1)), (global.clone(), json!(1))]),
            Some(builtin.clone())
        );
        assert_eq!(
            origin_of(&[
                (builtin.clone(), json!(1)),
                (global.clone(), json!(2)),
                (profile.clone(), json!(3))
            ]),
            Some(profile.clone())
        );
        assert_eq!(
            origin_of(&[
                (builtin, json!(1)),
                (global.clone(), json!(2)),
                (profile.clone(), json!(1))
            ]),
            Some(profile)
        );
    }

    #[test]
    fn config_value_text_redacts_and_preserves_compact_values() {
        assert_eq!(
            config_value_text("issues.issue_accounts.0.token", json!("s3cr3t")),
            crate::redact::PLACEHOLDER
        );
        assert_eq!(
            config_value_text("sandbox.backend", json!("podman")),
            "podman"
        );
        assert_eq!(
            config_value_text("model_proxy.routes.0.auto_max_tokens", json!(5)),
            "5"
        );
    }

    #[test]
    fn scrub_url_removes_only_url_authority_credentials() {
        assert_eq!(
            scrub_url("https://u:t@github.com/a/b"),
            "https://github.com/a/b"
        );
        assert_eq!(scrub_url("git@github.com:a/b"), "git@github.com:a/b");
        assert_eq!(scrub_url("ssh://git@h/x"), "ssh://h/x");
    }

    #[test]
    fn clean_drops_controls_and_clips_at_character_boundaries() {
        assert_eq!(clean("a\n😀b", 4), "a😀b");
        assert_eq!(clean("😀ab", 2), "😀…");
        assert_eq!(clean("éclair", 3), "éc…");
        assert_eq!(clean("a\u{202E}b\u{2066}c", 10), "abc");
    }

    #[test]
    fn age_table() {
        assert_eq!(age(100, 100), "just now");
        assert_eq!(age(100, 41), "just now");
        assert_eq!(age(100, 40), "1m ago");
        assert_eq!(age(3_700, 100), "1h ago");
        assert_eq!(age(86_500, 100), "1d ago");
        assert_eq!(age(100, 101), "in the future");
    }

    #[test]
    fn sections_skip_empty_sections_and_preserve_tones() {
        let report = InfoReport {
            schema: INFO_SCHEMA,
            generated_at: 0,
            target: InfoTarget {
                repo_root: "/r".into(),
                worktree: None,
            },
            workspace: vec![Field::unknown(keys::WORKSPACE_NAME, "Name", "missing")],
            workspace_stats: Vec::new(),
            worktree: Vec::new(),
            sessions: None,
            settings_scope: "workspace".into(),
            settings: vec![
                Field::value(keys::SANDBOX_BACKEND, "Backend", "podman", Source::Global).warned(),
            ],
            backends: Vec::new(),
            overrides: Vec::new(),
            notes: Vec::new(),
        };
        let sections = sections(&report);
        assert_eq!(
            sections
                .iter()
                .map(|section| section.title.as_str())
                .collect::<Vec<_>>(),
            ["WORKSPACE", "EFFECTIVE SETTINGS (workspace)"]
        );
        assert_eq!(sections[0].lines[0].value, "unknown: missing");
        assert_eq!(sections[0].lines[0].tone, Tone::Muted);
        assert_eq!(sections[1].lines[0].tone, Tone::Warn);
    }

    #[test]
    fn report_json_has_schema_source_and_state_shapes() {
        let report = InfoReport {
            schema: INFO_SCHEMA,
            generated_at: 0,
            target: InfoTarget {
                repo_root: "/r".into(),
                worktree: None,
            },
            workspace: vec![Field::value(
                keys::WORKSPACE_REPO,
                "Repo",
                "/r",
                Source::Repo("/r/.thegn.toml".into()),
            )],
            workspace_stats: vec![Field::not_measured(keys::STATS_DISK, "Disk")],
            worktree: Vec::new(),
            sessions: None,
            settings_scope: "workspace".into(),
            settings: Vec::new(),
            backends: Vec::new(),
            overrides: Vec::new(),
            notes: Vec::new(),
        };
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["schema"], 1);
        assert_eq!(
            value["workspace"][0]["source"],
            json!({"layer": "repo", "detail": "/r/.thegn.toml"})
        );
        assert_eq!(
            value["workspace_stats"][0]["shown"],
            json!({"state": "not_measured"})
        );
    }

    #[test]
    fn rendered_text_has_no_ansi() {
        let report = InfoReport {
            schema: INFO_SCHEMA,
            generated_at: 0,
            target: InfoTarget {
                repo_root: "/r".into(),
                worktree: None,
            },
            workspace: vec![Field::value(
                keys::WORKSPACE_NAME,
                "Name",
                "\u{1b}[31mrepo",
                Source::Builtin,
            )],
            workspace_stats: Vec::new(),
            worktree: Vec::new(),
            sessions: None,
            settings_scope: String::new(),
            settings: Vec::new(),
            backends: Vec::new(),
            overrides: Vec::new(),
            notes: Vec::new(),
        };
        assert!(!render_text(&report).contains('\u{1b}'));
    }
}
