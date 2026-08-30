use super::{
    Editor, EditorCaps, EditorError, EditorLaunch, EditorTarget, JumpSyntax, Placement,
    ProgramProfile, executable_availability, forced_placement,
};
use crate::config::EditorOpenIn;
use crate::seam::{Probe, ProbeReport};

const ID: &str = "emacs";
const EXECUTABLE: &str = "emacsclient";

pub(super) struct Emacs {
    open_in: EditorOpenIn,
}
impl Emacs {
    pub(super) fn new(open_in: EditorOpenIn) -> Self {
        Self { open_in }
    }
}
pub(super) fn program_profile(program: &str) -> Option<ProgramProfile> {
    matches!(program, "emacs" | "emacsclient").then_some(ProgramProfile {
        jump: JumpSyntax::Plus,
        column: false,
        external: program == EXECUTABLE,
    })
}
impl Probe for Emacs {
    fn probe(&self) -> ProbeReport {
        ProbeReport::new("editor", ID, executable_availability(EXECUTABLE)).with_caps(&self.caps())
    }
}
impl Editor for Emacs {
    fn id(&self) -> &'static str {
        ID
    }
    fn caps(&self) -> EditorCaps {
        caps(self.open_in)
    }
    fn open_file(&self, target: &EditorTarget) -> Result<EditorLaunch, EditorError> {
        let mut argv = vec![EXECUTABLE.into(), "-n".into()];
        if let Some(line) = target.line() {
            argv.push(match target.col() {
                // The handoff seam is 1-based, while Emacs action-argument
                // columns are zero-based (column 0 is the leftmost column).
                Some(col) => format!("+{line}:{}", col.saturating_sub(1)),
                None => format!("+{line}"),
            });
        }
        argv.push(target.path().to_string_lossy().into_owned());
        Ok(EditorLaunch::direct(
            ID,
            argv,
            target,
            placement(self.open_in),
        ))
    }
    fn open_directory(&self, target: &EditorTarget) -> Result<EditorLaunch, EditorError> {
        Ok(EditorLaunch::direct(
            ID,
            vec![
                EXECUTABLE.into(),
                "-n".into(),
                target.path().to_string_lossy().into_owned(),
            ],
            target,
            placement(self.open_in),
        ))
    }
}
fn placement(open_in: EditorOpenIn) -> Placement {
    forced_placement(open_in, Placement::External)
}
fn caps(open_in: EditorOpenIn) -> EditorCaps {
    EditorCaps {
        open_file: true,
        open_directory: true,
        line: true,
        column: true,
        external: placement(open_in) == Placement::External,
    }
}
