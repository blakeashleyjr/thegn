use super::{
    Editor, EditorCaps, EditorError, EditorLaunch, EditorTarget, JumpSyntax, Placement,
    ProgramProfile, forced_placement,
};
use crate::config::EditorOpenIn;
use crate::seam::{Availability, Probe, ProbeReport};

const ID: &str = "zed";
const EXECUTABLE: &str = "zed";

pub(super) struct Zed {
    open_in: EditorOpenIn,
}
impl Zed {
    pub(super) fn new(open_in: EditorOpenIn) -> Self {
        Self { open_in }
    }
}
pub(super) fn program_profile(program: &str) -> Option<ProgramProfile> {
    matches!(program, "zed" | "zeditor").then_some(ProgramProfile {
        jump: JumpSyntax::Colon,
        column: true,
        external: true,
    })
}
impl Probe for Zed {
    fn probe(&self) -> ProbeReport {
        ProbeReport::new("editor", ID, Availability::Ready)
            .with_caps(&self.caps())
            .note("registered; executable availability is checked by the launch edge")
    }
}
impl Editor for Zed {
    fn id(&self) -> &'static str {
        ID
    }
    fn caps(&self) -> EditorCaps {
        caps(self.open_in)
    }
    fn open_file(&self, target: &EditorTarget) -> Result<EditorLaunch, EditorError> {
        let mut location = target.path().to_string_lossy().into_owned();
        if let Some(line) = target.line() {
            location.push_str(&format!(":{line}"));
            if let Some(col) = target.col() {
                location.push_str(&format!(":{col}"));
            }
        }
        Ok(EditorLaunch::direct(
            ID,
            vec![EXECUTABLE.into(), location],
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
        column: true,
        external: placement(open_in) == Placement::External,
    }
}
