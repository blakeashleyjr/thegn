use super::{
    Editor, EditorCaps, EditorError, EditorLaunch, EditorTarget, JumpSyntax, Placement,
    ProgramProfile, executable_availability, forced_placement,
};
use crate::config::EditorOpenIn;
use crate::seam::{Probe, ProbeReport};

const ID: &str = "jetbrains";
const EXECUTABLE: &str = "idea";

pub(super) struct Jetbrains {
    open_in: EditorOpenIn,
}
impl Jetbrains {
    pub(super) fn new(open_in: EditorOpenIn) -> Self {
        Self { open_in }
    }
}
pub(super) fn program_profile(program: &str) -> Option<ProgramProfile> {
    matches!(program, "idea" | "pycharm" | "webstorm" | "rider").then_some(ProgramProfile {
        jump: JumpSyntax::LineFlag,
        column: false,
        external: true,
    })
}
impl Probe for Jetbrains {
    fn probe(&self) -> ProbeReport {
        ProbeReport::new("editor", ID, executable_availability(EXECUTABLE)).with_caps(&self.caps())
    }
}
impl Editor for Jetbrains {
    fn id(&self) -> &'static str {
        ID
    }
    fn caps(&self) -> EditorCaps {
        caps(self.open_in)
    }
    fn open_file(&self, target: &EditorTarget) -> Result<EditorLaunch, EditorError> {
        let mut argv = vec![EXECUTABLE.into()];
        if let Some(line) = target.line() {
            argv.extend(["--line".into(), line.to_string()]);
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
        column: false,
        external: placement(open_in) == Placement::External,
    }
}
