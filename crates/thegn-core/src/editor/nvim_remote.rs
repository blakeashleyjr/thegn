use super::{
    Editor, EditorCaps, EditorError, EditorLaunch, EditorTarget, JumpSyntax, Placement,
    ProgramProfile, forced_placement,
};
use crate::config::EditorOpenIn;
use crate::seam::{Availability, Probe, ProbeReport};

const ID: &str = "nvim_remote";
const EXECUTABLE: &str = "nvr";

pub(super) struct NvimRemote {
    open_in: EditorOpenIn,
}
impl NvimRemote {
    pub(super) fn new(open_in: EditorOpenIn) -> Self {
        Self { open_in }
    }
}
pub(super) fn program_profile(program: &str) -> Option<ProgramProfile> {
    matches!(program, "vim" | "nvim" | "nvr").then_some(ProgramProfile {
        jump: JumpSyntax::Plus,
        column: false,
        external: program == EXECUTABLE,
    })
}
impl Probe for NvimRemote {
    fn probe(&self) -> ProbeReport {
        ProbeReport::new("editor", ID, Availability::Ready)
            .with_caps(&self.caps())
            .note("registered; executable availability is checked by the launch edge")
    }
}
impl Editor for NvimRemote {
    fn id(&self) -> &'static str {
        ID
    }
    fn caps(&self) -> EditorCaps {
        caps(self.open_in)
    }
    fn open_file(&self, target: &EditorTarget) -> Result<EditorLaunch, EditorError> {
        let mut argv = vec![EXECUTABLE.into(), "--remote-silent".into()];
        if let Some(line) = target.line() {
            argv.push(format!("+{line}"));
        }
        argv.push(target.path().to_string_lossy().into_owned());
        Ok(EditorLaunch::direct(
            ID,
            argv,
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
        open_directory: false,
        line: true,
        column: false,
        external: placement(open_in) == Placement::External,
    }
}
