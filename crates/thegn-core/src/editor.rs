//! Pure planning for handing a worktree or one of its files to an editor.
//!
//! [`Editor`] is the only editor/IDE abstraction. Logical providers build
//! structured argv in the sibling implementation modules; the custom command
//! ladder remains for compatibility and is represented as a shell argv.

use crate::config::{Config, EditorOpenIn};
use crate::seam::{Availability, ErrorClass, Probe, ProbeReport, SeamError};
use crate::util;
use std::path::{Component, Path, PathBuf};

mod cursor;
mod emacs;
mod jetbrains;
mod nvim_remote;
pub mod providers;
mod vscode;
mod zed;

pub use providers::EditorProvider;

/// The legacy file-open request. New handoff callers should construct an
/// [`EditorTarget`], which carries the worktree containment invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenRequest<'a> {
    pub path: &'a str,
    pub line: Option<usize>,
    pub col: Option<usize>,
}

/// A validated handoff target.
///
/// `worktree` is absolute. `file`, when present, is normalized and relative to
/// it; lexical traversal above the worktree is rejected without touching the
/// filesystem. Line and column numbers are 1-based.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct EditorTarget {
    worktree: PathBuf,
    file: Option<PathBuf>,
    line: Option<usize>,
    col: Option<usize>,
}

impl EditorTarget {
    pub fn new(
        worktree: impl Into<PathBuf>,
        file: Option<impl AsRef<Path>>,
        line: Option<usize>,
        col: Option<usize>,
    ) -> Result<Self, EditorError> {
        let worktree = worktree.into();
        if worktree.as_os_str().is_empty() {
            return Err(EditorError::InvalidTarget("worktree is empty".into()));
        }
        if !worktree.is_absolute() {
            return Err(EditorError::InvalidTarget(
                "worktree must be an absolute path".into(),
            ));
        }
        if line == Some(0) || col == Some(0) {
            return Err(EditorError::InvalidTarget(
                "line and column are 1-based".into(),
            ));
        }

        let file = match file {
            Some(path) => Some(normalize_relative(path.as_ref())?),
            None => None,
        };
        if file.is_none() && (line.is_some() || col.is_some()) {
            return Err(EditorError::InvalidTarget(
                "line or column requires a file".into(),
            ));
        }
        if col.is_some() && line.is_none() {
            return Err(EditorError::InvalidTarget("column requires a line".into()));
        }

        Ok(Self {
            worktree,
            file,
            line,
            col,
        })
    }

    pub fn project(worktree: impl Into<PathBuf>) -> Result<Self, EditorError> {
        Self::new(worktree, Option::<&Path>::None, None, None)
    }

    pub fn file(
        worktree: impl Into<PathBuf>,
        file: impl AsRef<Path>,
        line: Option<usize>,
        col: Option<usize>,
    ) -> Result<Self, EditorError> {
        Self::new(worktree, Some(file), line, col)
    }

    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    pub fn relative_file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    pub fn line(&self) -> Option<usize> {
        self.line
    }

    pub fn col(&self) -> Option<usize> {
        self.col
    }

    pub fn path(&self) -> PathBuf {
        self.file
            .as_ref()
            .map_or_else(|| self.worktree.clone(), |file| self.worktree.join(file))
    }

    pub fn operation(&self) -> EditorOperation {
        if self.file.is_some() {
            EditorOperation::OpenFile
        } else {
            EditorOperation::OpenDirectory
        }
    }

    /// Compatibility for callers that have not yet supplied a worktree. This
    /// is intentionally private: only [`OpenRequest`] can bypass target policy.
    fn legacy(req: &OpenRequest<'_>) -> Self {
        Self {
            worktree: PathBuf::new(),
            file: Some(PathBuf::from(req.path)),
            line: req.line,
            col: req.col,
        }
    }
}

fn normalize_relative(path: &Path) -> Result<PathBuf, EditorError> {
    if path.as_os_str().is_empty() {
        return Err(EditorError::InvalidTarget("file path is empty".into()));
    }
    if path.is_absolute() {
        return Err(EditorError::InvalidTarget(
            "file path must be worktree-relative".into(),
        ));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(EditorError::InvalidTarget(
                        "file path escapes the worktree".into(),
                    ));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(EditorError::InvalidTarget(
                    "file path must be worktree-relative".into(),
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(EditorError::InvalidTarget("file path is empty".into()));
    }
    Ok(normalized)
}

/// Which optional operation produced a launch plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EditorOperation {
    OpenFile,
    OpenDirectory,
}

/// Where the launch should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Pane,
    External,
}

/// A substrate-free launch plan. `command` is retained temporarily for old
/// host call sites; it is derived from `argv` for logical providers. New code
/// executes `argv` directly and applies `cwd` at the host edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorLaunch {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub placement: Placement,
    pub provider: &'static str,
    pub operation: EditorOperation,
    pub command: String,
}

impl EditorLaunch {
    pub(super) fn direct(
        provider: &'static str,
        argv: Vec<String>,
        target: &EditorTarget,
        placement: Placement,
    ) -> Self {
        let command = argv_shell_line(&argv);
        Self {
            argv,
            cwd: target.worktree.clone(),
            placement,
            provider,
            operation: target.operation(),
            command,
        }
    }

    fn shell(
        provider: &'static str,
        command: String,
        target: &EditorTarget,
        placement: Placement,
    ) -> Self {
        Self {
            argv: crate::shellinv::run_argv(&util::shell(), &command),
            cwd: target.worktree.clone(),
            placement,
            provider,
            operation: target.operation(),
            command,
        }
    }
}

fn argv_shell_line(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| util::sh_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the resolved editor can do. Optional operation bits exactly match the
/// trait methods a provider implements.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct EditorCaps {
    pub open_file: bool,
    pub open_directory: bool,
    pub line: bool,
    pub column: bool,
    pub external: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorError {
    NotConfigured(&'static str),
    Unsupported(&'static str),
    InvalidTarget(String),
    Other(String),
}

impl std::fmt::Display for EditorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditorError::NotConfigured(w) => write!(f, "no editor: {w}"),
            EditorError::Unsupported(op) => write!(f, "editor does not support {op}"),
            EditorError::InvalidTarget(m) => write!(f, "invalid editor target: {m}"),
            EditorError::Other(m) => f.write_str(m),
        }
    }
}
impl std::error::Error for EditorError {}
impl SeamError for EditorError {
    fn class(&self) -> ErrorClass {
        match self {
            EditorError::NotConfigured(_) => ErrorClass::NotConfigured,
            EditorError::Unsupported(_) => ErrorClass::Unsupported,
            EditorError::InvalidTarget(_) | EditorError::Other(_) => ErrorClass::Other,
        }
    }
    fn unsupported(op: &'static str) -> Self {
        EditorError::Unsupported(op)
    }
}

/// The only editor/IDE seam. Planning is synchronous and performs no I/O.
pub trait Editor: Probe + Send + Sync {
    fn id(&self) -> &'static str;
    fn caps(&self) -> EditorCaps;

    fn open_file(&self, _target: &EditorTarget) -> Result<EditorLaunch, EditorError> {
        Err(EditorError::unsupported("open_file"))
    }

    fn open_directory(&self, _target: &EditorTarget) -> Result<EditorLaunch, EditorError> {
        Err(EditorError::unsupported("open_directory"))
    }

    fn open_target(&self, target: &EditorTarget) -> Result<EditorLaunch, EditorError> {
        let caps = self.caps();
        match target.operation() {
            EditorOperation::OpenFile if !caps.open_file => {
                return Err(EditorError::unsupported("open_file"));
            }
            EditorOperation::OpenDirectory if !caps.open_directory => {
                return Err(EditorError::unsupported("open_directory"));
            }
            _ => {}
        }
        if target.line().is_some() && !caps.line {
            return Err(EditorError::unsupported("line"));
        }
        if target.col().is_some() && !caps.column {
            return Err(EditorError::unsupported("column"));
        }
        match target.operation() {
            EditorOperation::OpenFile => self.open_file(target),
            EditorOperation::OpenDirectory => self.open_directory(target),
        }
    }

    /// Compatibility adapter for existing file-open call sites. New handoff
    /// surfaces must use [`Self::open_target`].
    fn open(&self, req: &OpenRequest<'_>) -> Result<EditorLaunch, EditorError> {
        self.open_file(&EditorTarget::legacy(req))
    }
}

// ---------------------------------------------------------------------------
// Custom-program compatibility
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpSyntax {
    Plus,
    Colon,
    GotoFlag,
    LineFlag,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramProfile {
    pub jump: JumpSyntax,
    pub column: bool,
    pub external: bool,
}

pub fn program_profile(program: &str) -> ProgramProfile {
    let base = util::basename(program);
    let base = base.strip_suffix(".exe").unwrap_or(base);
    if let Some(profile) = [
        vscode::program_profile(base),
        cursor::program_profile(base),
        zed::program_profile(base),
        jetbrains::program_profile(base),
        nvim_remote::program_profile(base),
        emacs::program_profile(base),
    ]
    .into_iter()
    .flatten()
    .next()
    {
        return profile;
    }
    let p = |jump, column, external| ProgramProfile {
        jump,
        column,
        external,
    };
    match base {
        "vi" | "nvi" | "vis" | "neovide" => p(JumpSyntax::Plus, false, false),
        "nano" | "micro" | "jed" | "mg" => p(JumpSyntax::Plus, false, false),
        "hx" | "helix" | "kak" => p(JumpSyntax::Colon, true, false),
        "subl" | "sublime_text" => p(JumpSyntax::Colon, true, true),
        "kate" | "gedit" => p(JumpSyntax::LineFlag, false, true),
        "gvim" | "mvim" => p(JumpSyntax::Plus, false, true),
        _ => p(JumpSyntax::None, false, false),
    }
}

pub fn is_gui_editor(cmd: &str) -> bool {
    let prog = cmd.split_whitespace().next().unwrap_or(cmd);
    program_profile(prog).external
}

pub fn launch_line(program: &str, req: &OpenRequest<'_>) -> EditorLaunch {
    let target = EditorTarget::legacy(req);
    custom_program_launch("program", program, &target, EditorOpenIn::Auto)
}

fn custom_program_launch(
    id: &'static str,
    program: &str,
    target: &EditorTarget,
    open_in: EditorOpenIn,
) -> EditorLaunch {
    let prog_word = program.split_whitespace().next().unwrap_or(program);
    let prof = program_profile(prog_word);
    let target_path = target.path();
    let path = target_path.to_string_lossy();
    let quoted = util::sh_quote(&path);
    let located = |sep: &str| match (target.line, target.col) {
        (Some(line), Some(col)) if prof.column => {
            util::sh_quote(&format!("{path}{sep}{line}{sep}{col}"))
        }
        (Some(line), _) => util::sh_quote(&format!("{path}{sep}{line}")),
        _ => quoted.clone(),
    };
    let command = match (prof.jump, target.line) {
        (JumpSyntax::Plus, Some(line)) => format!("{program} +{line} {quoted}"),
        (JumpSyntax::Colon, Some(_)) => format!("{program} {}", located(":")),
        (JumpSyntax::GotoFlag, Some(_)) => format!("{program} -g {}", located(":")),
        (JumpSyntax::LineFlag, Some(line)) => format!("{program} --line {line} {quoted}"),
        _ => format!("{program} {quoted}"),
    };
    let inferred = if prof.external {
        Placement::External
    } else {
        Placement::Pane
    };
    EditorLaunch::shell(id, command, target, forced_placement(open_in, inferred))
}

fn forced_placement(open_in: EditorOpenIn, inferred: Placement) -> Placement {
    match open_in {
        EditorOpenIn::Auto => inferred,
        EditorOpenIn::Pane => Placement::Pane,
        EditorOpenIn::External => Placement::External,
    }
}

pub struct TemplateEditor {
    template: String,
    open_in: EditorOpenIn,
}

impl Probe for TemplateEditor {
    fn probe(&self) -> ProbeReport {
        ProbeReport::new("editor", "template", Availability::Ready)
            .with_caps(&self.caps())
            .note(format!("[editor] command = {:?}", self.template))
    }
}

impl Editor for TemplateEditor {
    fn id(&self) -> &'static str {
        "template"
    }
    fn caps(&self) -> EditorCaps {
        EditorCaps {
            open_file: true,
            open_directory: true,
            line: self.template.contains("{line}"),
            column: self.template.contains("{col}"),
            external: self.placement() == Placement::External,
        }
    }
    fn open_file(&self, target: &EditorTarget) -> Result<EditorLaunch, EditorError> {
        Ok(self.plan(target))
    }
    fn open_directory(&self, target: &EditorTarget) -> Result<EditorLaunch, EditorError> {
        Ok(self.plan(target))
    }
}

impl TemplateEditor {
    fn plan(&self, target: &EditorTarget) -> EditorLaunch {
        let target_path = target.path();
        let path = target_path.to_string_lossy();
        let command = self
            .template
            .replace("{path}", &util::sh_quote(&path))
            .replace(
                "{line}",
                &target.line.map(|line| line.to_string()).unwrap_or_default(),
            )
            .replace(
                "{col}",
                &target.col.map(|col| col.to_string()).unwrap_or_default(),
            );
        EditorLaunch::shell("template", command, target, self.placement())
    }

    fn placement(&self) -> Placement {
        forced_placement(
            self.open_in,
            if is_gui_editor(&self.template) {
                Placement::External
            } else {
                Placement::Pane
            },
        )
    }
}

pub struct ProgramEditor {
    id: &'static str,
    program: String,
    open_in: EditorOpenIn,
}

pub(super) fn executable_availability(program: &str) -> Availability {
    if program.contains('/') {
        if Path::new(program).exists() {
            Availability::Ready
        } else {
            Availability::Unavailable(format!("{program} not found"))
        }
    } else if util::which_path(program).is_some() {
        Availability::Ready
    } else {
        Availability::Unavailable(format!("`{program}` not found on PATH"))
    }
}

impl Probe for ProgramEditor {
    fn probe(&self) -> ProbeReport {
        let program = self.program.split_whitespace().next().unwrap_or("");
        ProbeReport::new("editor", self.id, executable_availability(program))
            .with_caps(&self.caps())
            .note(format!("program: {}", self.program))
    }
}

impl Editor for ProgramEditor {
    fn id(&self) -> &'static str {
        self.id
    }
    fn caps(&self) -> EditorCaps {
        let profile = program_profile(self.program.split_whitespace().next().unwrap_or(""));
        EditorCaps {
            open_file: true,
            open_directory: true,
            line: profile.jump != JumpSyntax::None,
            column: profile.column,
            external: forced_placement(
                self.open_in,
                if profile.external {
                    Placement::External
                } else {
                    Placement::Pane
                },
            ) == Placement::External,
        }
    }
    fn open_file(&self, target: &EditorTarget) -> Result<EditorLaunch, EditorError> {
        Ok(custom_program_launch(
            self.id,
            &self.program,
            target,
            self.open_in,
        ))
    }
    fn open_directory(&self, target: &EditorTarget) -> Result<EditorLaunch, EditorError> {
        Ok(custom_program_launch(
            self.id,
            &self.program,
            target,
            self.open_in,
        ))
    }
}

pub fn editor_for(cfg: &Config) -> Box<dyn Editor> {
    editor_with_env(cfg, |key| std::env::var(key).ok())
}

pub fn editor_for_workspace(cfg: &Config, workspace_slug: &str) -> Box<dyn Editor> {
    editor_with_env_for_workspace(cfg, workspace_slug, |key| std::env::var(key).ok())
}

pub fn editor_with_env(cfg: &Config, env: impl Fn(&str) -> Option<String>) -> Box<dyn Editor> {
    resolve_editor(cfg, None, env)
}

pub fn editor_with_env_for_workspace(
    cfg: &Config,
    workspace_slug: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Box<dyn Editor> {
    resolve_editor(cfg, Some(workspace_slug), env)
}

fn resolve_editor(
    cfg: &Config,
    workspace_slug: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Box<dyn Editor> {
    let open_in = cfg.editor.open_in;
    let template = cfg.editor.command.trim();
    if !template.is_empty() {
        return Box::new(TemplateEditor {
            template: template.to_string(),
            open_in,
        });
    }

    let provider = workspace_slug
        .and_then(|slug| cfg.workspace.get(slug))
        .and_then(|workspace| workspace.editor)
        .unwrap_or(cfg.editor.provider);
    if let Some(editor) = providers::provider(provider, open_in) {
        return editor;
    }

    if let Some(tool) = cfg.tool_command("editor") {
        let tool = tool.trim();
        let tool = tool.strip_suffix(" .").unwrap_or(tool);
        if !tool.is_empty() && !tool.starts_with("${") {
            return Box::new(ProgramEditor {
                id: "tool",
                program: tool.to_string(),
                open_in,
            });
        }
    }
    for (id, key) in [("visual", "VISUAL"), ("env", "EDITOR")] {
        if let Some(program) = env(key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            return Box::new(ProgramEditor {
                id,
                program,
                open_in,
            });
        }
    }
    Box::new(ProgramEditor {
        id: "vi",
        program: "vi".into(),
        open_in,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(line: Option<usize>) -> OpenRequest<'static> {
        OpenRequest {
            path: "src/a'b.rs",
            line,
            col: None,
        }
    }

    #[test]
    fn target_normalizes_inside_worktree_without_io() {
        let target =
            EditorTarget::file("/work/tree", "src/./nested/../main.rs", Some(7), Some(3)).unwrap();
        assert_eq!(target.worktree(), Path::new("/work/tree"));
        assert_eq!(target.relative_file(), Some(Path::new("src/main.rs")));
        assert_eq!(target.path(), Path::new("/work/tree/src/main.rs"));
        assert_eq!(target.line(), Some(7));
        assert_eq!(target.col(), Some(3));
        assert_eq!(target.operation(), EditorOperation::OpenFile);

        let project = EditorTarget::project("/work/tree").unwrap();
        assert_eq!(project.path(), Path::new("/work/tree"));
        assert_eq!(project.operation(), EditorOperation::OpenDirectory);
    }

    #[test]
    fn target_rejects_escape_and_invalid_shapes() {
        for bad in ["../secret", "src/../../secret", "/etc/passwd", ""] {
            assert!(EditorTarget::file("/work/tree", bad, None, None).is_err());
        }
        assert!(EditorTarget::project("").is_err());
        assert!(EditorTarget::project("relative/root").is_err());
        assert!(EditorTarget::new("/work/tree", Option::<&Path>::None, Some(1), None).is_err());
        assert!(EditorTarget::file("/work/tree", "x", Some(0), None).is_err());
        assert!(EditorTarget::file("/work/tree", "x", None, Some(1)).is_err());
    }

    #[test]
    fn launch_lines_keep_custom_program_compatibility() {
        assert_eq!(
            launch_line("vim", &req(Some(42))).command,
            r"vim +42 'src/a'\''b.rs'"
        );
        assert_eq!(
            launch_line("code --wait", &req(Some(7))).command,
            r"code --wait -g 'src/a'\''b.rs:7'"
        );
        assert_eq!(
            launch_line("hx", &req(Some(3))).command,
            r"hx 'src/a'\''b.rs:3'"
        );
        assert_eq!(
            launch_line("kate", &req(Some(9))).command,
            r"kate --line 9 'src/a'\''b.rs'"
        );
        assert_eq!(
            launch_line("mystery", &req(Some(9))).command,
            r"mystery 'src/a'\''b.rs'"
        );
        assert_eq!(
            launch_line("code", &req(None)).placement,
            Placement::External
        );
    }

    #[test]
    fn precedence_is_template_workspace_global_then_auto_ladder() {
        let mut cfg = Config::default();
        cfg.editor.provider = EditorProvider::Vscode;
        cfg.workspace.insert(
            "repo".into(),
            crate::config::WorkspaceConfig {
                editor: Some(EditorProvider::Cursor),
                ..Default::default()
            },
        );
        assert_eq!(
            editor_with_env_for_workspace(&cfg, "repo", |_| None).id(),
            "cursor"
        );
        assert_eq!(
            editor_with_env_for_workspace(&cfg, "other", |_| None).id(),
            "vscode"
        );

        cfg.editor.command = "myed {path}".into();
        assert_eq!(
            editor_with_env_for_workspace(&cfg, "repo", |_| None).id(),
            "template"
        );

        cfg.editor.command.clear();
        cfg.workspace.get_mut("repo").unwrap().editor = Some(EditorProvider::Auto);
        assert_eq!(
            editor_with_env_for_workspace(&cfg, "repo", |key| (key == "EDITOR")
                .then(|| "nano".into()))
            .id(),
            "env"
        );
    }

    #[test]
    fn custom_ladder_and_placement_remain_compatible() {
        let mut cfg = Config::default();
        let editor = editor_with_env(&cfg, |key| (key == "EDITOR").then(|| "nano".into()));
        assert_eq!(editor.id(), "env");
        assert_eq!(
            editor.open(&req(Some(2))).unwrap().command,
            r"nano +2 'src/a'\''b.rs'"
        );

        let editor = editor_with_env(&cfg, |key| match key {
            "VISUAL" => Some("code".into()),
            "EDITOR" => Some("nano".into()),
            _ => None,
        });
        assert_eq!(editor.id(), "visual");
        assert!(editor.caps().external && editor.caps().column);

        cfg.tools.push(crate::config::NamedCommand {
            name: "editor".into(),
            command: "hx .".into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            resume: false,
            route_via_proxy: false,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
            drawer_scope: None,
            drawer_cwd: None,
        });
        assert_eq!(editor_with_env(&cfg, |_| Some("nano".into())).id(), "tool");

        cfg.editor.open_in = EditorOpenIn::Pane;
        let editor = editor_with_env(&cfg, |_| Some("code".into()));
        assert_eq!(editor.open(&req(None)).unwrap().placement, Placement::Pane);
        assert!(!editor.caps().external);
    }

    #[test]
    fn template_project_open_uses_root_and_empty_location() {
        let mut cfg = Config::default();
        cfg.editor.command = "myed --file {path} --at {line}:{col}".into();
        cfg.editor.open_in = EditorOpenIn::External;
        let launch = editor_with_env(&cfg, |_| None)
            .open_target(&EditorTarget::project("/work/tree").unwrap())
            .unwrap();
        assert_eq!(launch.command, "myed --file /work/tree --at :");
        assert_eq!(launch.cwd, Path::new("/work/tree"));
        assert_eq!(launch.operation, EditorOperation::OpenDirectory);
    }

    #[test]
    fn errors_classify() {
        assert_eq!(
            EditorError::NotConfigured("x").class(),
            ErrorClass::NotConfigured
        );
        assert_eq!(
            EditorError::unsupported("y").class(),
            ErrorClass::Unsupported
        );
        assert_eq!(
            EditorError::InvalidTarget("z".into()).class(),
            ErrorClass::Other
        );
    }
}
