//! The editor seam: "open this file (at this line) in the user's editor".
//!
//! thegn opens files from a dozen places — the files accordion, diff hunks,
//! test failures, problems, search hits, `thegn config edit` — and each used
//! to re-derive the command string. This is the one resolver:
//!
//! 1. `[editor] command` (a template with `{path}`, `{line}`, `{col}`), else
//! 2. the `[[tools]]` entry named `editor`, else
//! 3. `$VISUAL` / `$EDITOR`, else
//! 4. `vi`.
//!
//! For 2–4 the program's **basename** picks the line-jump syntax (`vim +N
//! file`, `code -g file:N`, `hx file:N`, …) and whether the editor is a
//! windowed app to spawn detached rather than inside a pane. It is a *sync*
//! seam (`openspec/specs/provider-seams`): [`Editor::open`] only plans a
//! launch — the caller spawns it in a pane or detached.

use crate::config::{Config, EditorOpenIn};
use crate::seam::{Availability, ErrorClass, Probe, ProbeReport, SeamError};
use crate::util;
use std::path::Path;

/// What a file-open needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenRequest<'a> {
    pub path: &'a str,
    pub line: Option<usize>,
    pub col: Option<usize>,
}

/// Where the launch should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// A shell command for a terminal pane (`vim +12 'src/a.rs'`).
    Pane,
    /// A windowed editor: spawn detached (`code -g 'src/a.rs:12'`).
    External,
}

/// A planned launch: a shell line plus where to run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorLaunch {
    pub command: String,
    pub placement: Placement,
}

/// What the resolved editor can do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct EditorCaps {
    /// Can jump to a line.
    pub line: bool,
    /// Can jump to a column.
    pub column: bool,
    /// Is a windowed app (opens outside the terminal).
    pub external: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorError {
    /// No editor resolvable at this layer (ladder falls through).
    NotConfigured(&'static str),
    Unsupported(&'static str),
    Other(String),
}

impl std::fmt::Display for EditorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditorError::NotConfigured(w) => write!(f, "no editor: {w}"),
            EditorError::Unsupported(op) => write!(f, "editor does not support {op}"),
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
            EditorError::Other(_) => ErrorClass::Other,
        }
    }
    fn unsupported(op: &'static str) -> Self {
        EditorError::Unsupported(op)
    }
}

/// The seam. Blocking by contract (pure planning — it never spawns).
pub trait Editor: Probe + Send + Sync {
    fn id(&self) -> &'static str;
    fn caps(&self) -> EditorCaps;
    /// Plan a launch for `req`. `NotConfigured` lets the next layer try.
    fn open(&self, req: &OpenRequest<'_>) -> Result<EditorLaunch, EditorError>;
}

// ---------------------------------------------------------------------------
// Per-program knowledge (pure)
// ---------------------------------------------------------------------------

/// How a program spells "open `file` at line N (col M)".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpSyntax {
    /// `prog +N file` (vi family, nano, micro, emacs, helix also accepts `file:N`).
    Plus,
    /// `prog file:N[:M]` (helix, zed, subl, kak).
    Colon,
    /// `prog -g file:N[:M]` (VS Code family).
    GotoFlag,
    /// `prog --line N file` (kate, gedit).
    LineFlag,
    /// Unknown program: open the file, no jump.
    None,
}

/// Static facts about an editor program, keyed by basename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramProfile {
    pub jump: JumpSyntax,
    pub column: bool,
    pub external: bool,
}

/// The table every layer consults. Basename only (`code --wait` and
/// `/usr/bin/code` both resolve to `code`; `.exe` stripped).
pub fn program_profile(program: &str) -> ProgramProfile {
    let base = util::basename(program);
    let base = base.strip_suffix(".exe").unwrap_or(base);
    let p = |jump, column, external| ProgramProfile {
        jump,
        column,
        external,
    };
    match base {
        "vi" | "vim" | "nvim" | "nvi" | "vis" | "neovide" => p(JumpSyntax::Plus, false, false),
        "nano" | "micro" | "emacs" | "emacsclient" | "jed" | "mg" => {
            p(JumpSyntax::Plus, false, false)
        }
        "hx" | "helix" | "kak" => p(JumpSyntax::Colon, true, false),
        "zed" | "zeditor" | "subl" | "sublime_text" => p(JumpSyntax::Colon, true, true),
        "code" | "code-insiders" | "codium" | "vscodium" | "cursor" | "windsurf" => {
            p(JumpSyntax::GotoFlag, true, true)
        }
        "kate" | "gedit" => p(JumpSyntax::LineFlag, false, true),
        "gvim" | "mvim" => p(JumpSyntax::Plus, false, true),
        "idea" | "pycharm" | "webstorm" | "rider" => p(JumpSyntax::LineFlag, false, true),
        _ => p(JumpSyntax::None, false, false),
    }
}

/// Whether an editor command launches a graphical (windowed) editor that
/// should be spawned detached rather than run inside a terminal pane.
pub fn is_gui_editor(cmd: &str) -> bool {
    let prog = cmd.split_whitespace().next().unwrap_or(cmd);
    program_profile(prog).external
}

/// Compose the shell line for `program` (which may carry its own flags, e.g.
/// `code --wait`) opening `req`, using the program's jump syntax.
pub fn launch_line(program: &str, req: &OpenRequest<'_>) -> EditorLaunch {
    let prog_word = program.split_whitespace().next().unwrap_or(program);
    let prof = program_profile(prog_word);
    let quoted = util::sh_quote(req.path);
    let target = |sep: &str| match (req.line, req.col) {
        (Some(l), Some(c)) if prof.column => {
            util::sh_quote(&format!("{}{sep}{l}{sep}{c}", req.path))
        }
        (Some(l), _) => util::sh_quote(&format!("{}{sep}{l}", req.path)),
        _ => quoted.clone(),
    };
    let command = match (prof.jump, req.line) {
        (JumpSyntax::Plus, Some(l)) => format!("{program} +{l} {quoted}"),
        (JumpSyntax::Colon, Some(_)) => format!("{program} {}", target(":")),
        (JumpSyntax::GotoFlag, Some(_)) => format!("{program} -g {}", target(":")),
        (JumpSyntax::LineFlag, Some(l)) => format!("{program} --line {l} {quoted}"),
        _ => format!("{program} {quoted}"),
    };
    EditorLaunch {
        command,
        placement: if prof.external {
            Placement::External
        } else {
            Placement::Pane
        },
    }
}

// ---------------------------------------------------------------------------
// Layers
// ---------------------------------------------------------------------------

/// `[editor] command` — a user template with `{path}` / `{line}` / `{col}`.
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
            line: self.template.contains("{line}"),
            column: self.template.contains("{col}"),
            external: self.placement() == Placement::External,
        }
    }
    fn open(&self, req: &OpenRequest<'_>) -> Result<EditorLaunch, EditorError> {
        let command = self
            .template
            .replace("{path}", &util::sh_quote(req.path))
            .replace(
                "{line}",
                &req.line.map(|l| l.to_string()).unwrap_or_default(),
            )
            .replace("{col}", &req.col.map(|c| c.to_string()).unwrap_or_default());
        Ok(EditorLaunch {
            command,
            placement: self.placement(),
        })
    }
}

impl TemplateEditor {
    fn placement(&self) -> Placement {
        match self.open_in {
            EditorOpenIn::External => Placement::External,
            EditorOpenIn::Pane => Placement::Pane,
            EditorOpenIn::Auto => {
                if is_gui_editor(&self.template) {
                    Placement::External
                } else {
                    Placement::Pane
                }
            }
        }
    }
}

/// A plain program (+ flags): the `[[tools]] editor` entry, `$VISUAL`,
/// `$EDITOR`, or the `vi` fallback.
pub struct ProgramEditor {
    id: &'static str,
    program: String,
    open_in: EditorOpenIn,
}

impl Probe for ProgramEditor {
    fn probe(&self) -> ProbeReport {
        let prog = self.program.split_whitespace().next().unwrap_or("");
        let availability = if prog.contains('/') {
            if Path::new(prog).exists() {
                Availability::Ready
            } else {
                Availability::Unavailable(format!("{prog} not found"))
            }
        } else if util::which_path(prog).is_some() {
            Availability::Ready
        } else {
            Availability::Unavailable(format!("`{prog}` not found on PATH"))
        };
        ProbeReport::new("editor", self.id, availability)
            .with_caps(&self.caps())
            .note(format!("program: {}", self.program))
    }
}

impl Editor for ProgramEditor {
    fn id(&self) -> &'static str {
        self.id
    }
    fn caps(&self) -> EditorCaps {
        let prof = program_profile(self.program.split_whitespace().next().unwrap_or(""));
        EditorCaps {
            line: prof.jump != JumpSyntax::None,
            column: prof.column,
            external: match self.open_in {
                EditorOpenIn::External => true,
                EditorOpenIn::Pane => false,
                EditorOpenIn::Auto => prof.external,
            },
        }
    }
    fn open(&self, req: &OpenRequest<'_>) -> Result<EditorLaunch, EditorError> {
        let mut l = launch_line(&self.program, req);
        l.placement = match self.open_in {
            EditorOpenIn::External => Placement::External,
            EditorOpenIn::Pane => Placement::Pane,
            EditorOpenIn::Auto => l.placement,
        };
        Ok(l)
    }
}

/// Resolve the editor for this config + environment: the first layer with
/// something configured. Pure apart from reading `$VISUAL`/`$EDITOR`.
pub fn editor_for(cfg: &Config) -> Box<dyn Editor> {
    editor_with_env(cfg, |k| std::env::var(k).ok())
}

/// [`editor_for`] with the environment injected (tests).
pub fn editor_with_env(cfg: &Config, env: impl Fn(&str) -> Option<String>) -> Box<dyn Editor> {
    let open_in = cfg.editor.open_in;
    let t = cfg.editor.command.trim();
    if !t.is_empty() {
        return Box::new(TemplateEditor {
            template: t.to_string(),
            open_in,
        });
    }
    if let Some(tool) = cfg.tool_command("editor") {
        let tool = tool.trim();
        // The legacy `[[tools]] editor` default is `${EDITOR:-vi} .`: the
        // trailing ` .` opens the cwd — strip it so the file is the target.
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
        if let Some(v) = env(key)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            return Box::new(ProgramEditor {
                id,
                program: v,
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
    fn profiles_cover_the_usual_suspects() {
        assert_eq!(program_profile("vim").jump, JumpSyntax::Plus);
        assert_eq!(program_profile("/usr/bin/nvim").jump, JumpSyntax::Plus);
        assert_eq!(program_profile("code.exe").jump, JumpSyntax::GotoFlag);
        assert!(program_profile("code").external && program_profile("code").column);
        assert_eq!(program_profile("hx").jump, JumpSyntax::Colon);
        assert!(!program_profile("hx").external);
        assert_eq!(program_profile("kate").jump, JumpSyntax::LineFlag);
        assert_eq!(program_profile("weird-editor").jump, JumpSyntax::None);
        assert!(is_gui_editor("code --wait"));
        assert!(!is_gui_editor("vim"));
    }

    #[test]
    fn launch_lines_use_each_syntax_and_quote_paths() {
        assert_eq!(
            launch_line("vim", &req(Some(42))).command,
            r"vim +42 'src/a'\''b.rs'"
        );
        assert_eq!(
            launch_line("vim", &req(None)).command,
            r"vim 'src/a'\''b.rs'"
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
        let col = OpenRequest {
            path: "f.rs",
            line: Some(4),
            col: Some(8),
        };
        // `sh_quote` leaves shell-safe targets bare.
        assert_eq!(launch_line("zed", &col).command, "zed f.rs:4:8");
        assert_eq!(launch_line("vim", &col).command, "vim +4 f.rs");
        assert_eq!(launch_line("code", &col).placement, Placement::External);
        assert_eq!(launch_line("vim", &col).placement, Placement::Pane);
    }

    #[test]
    fn ladder_template_then_tool_then_env_then_vi() {
        let mut cfg = Config::default();
        // Default config: `[[tools]] editor` is `${EDITOR:-vi} .` → env layer.
        let e = editor_with_env(&cfg, |k| (k == "EDITOR").then(|| "nano".to_string()));
        assert_eq!(e.id(), "env");
        assert_eq!(
            e.open(&req(Some(2))).unwrap().command,
            r"nano +2 'src/a'\''b.rs'"
        );
        // $VISUAL beats $EDITOR.
        let e = editor_with_env(&cfg, |k| match k {
            "VISUAL" => Some("code".into()),
            "EDITOR" => Some("nano".into()),
            _ => None,
        });
        assert_eq!(e.id(), "visual");
        assert!(e.caps().external && e.caps().column);
        // Nothing set → vi.
        let e = editor_with_env(&cfg, |_| None);
        assert_eq!(e.id(), "vi");
        assert_eq!(e.probe().seam, "editor");
        // A concrete [[tools]] editor wins over env. (The default config has no
        // stored `[[tools]]` rows — push one, as a user config would.)
        cfg.tools.push(crate::config::NamedCommand {
            name: "editor".into(),
            command: "hx .".into(),
            hints: Vec::new(),
            provider: None,
        });
        let e = editor_with_env(&cfg, |_| Some("nano".into()));
        assert_eq!(e.id(), "tool");
        assert_eq!(
            e.open(&req(Some(5))).unwrap().command,
            r"hx 'src/a'\''b.rs:5'"
        );
        // [editor] command template wins over everything; open_in overrides.
        cfg.editor.command = "myed --file {path} --at {line}:{col}".into();
        cfg.editor.open_in = EditorOpenIn::External;
        let e = editor_with_env(&cfg, |_| Some("nano".into()));
        assert_eq!(e.id(), "template");
        let l = e
            .open(&OpenRequest {
                path: "x.rs",
                line: Some(1),
                col: Some(2),
            })
            .unwrap();
        assert_eq!(l.command, "myed --file x.rs --at 1:2");
        assert_eq!(l.placement, Placement::External);
        assert!(e.caps().line && e.caps().column && e.caps().external);
        assert!(e.probe().notes[0].contains("myed"));
    }

    #[test]
    fn open_in_pane_forces_a_gui_editor_into_the_terminal() {
        let mut cfg = Config::default();
        cfg.editor.open_in = EditorOpenIn::Pane;
        let e = editor_with_env(&cfg, |_| Some("code".into()));
        assert_eq!(e.open(&req(None)).unwrap().placement, Placement::Pane);
        assert!(!e.caps().external);
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
        assert_eq!(EditorError::Other("z".into()).to_string(), "z");
        assert!(
            EditorError::NotConfigured("x")
                .to_string()
                .contains("no editor")
        );
        assert!(EditorError::Unsupported("y").to_string().contains('y'));
    }
}
