//! The per-OS leaf of the SDK: which program opens a URL. Nothing else in this
//! crate may use a platform cfg (`platform_ratchet_tests::platform_cfgs_are_confined_to_platform_rs`).
use std::process::Command;

#[cfg(target_os = "linux")]
pub(crate) fn opener() -> Option<Command> {
    Some(Command::new("xdg-open"))
}

#[cfg(target_os = "macos")]
pub(crate) fn opener() -> Option<Command> {
    Some(Command::new("open"))
}

#[cfg(target_os = "windows")]
pub(crate) fn opener() -> Option<Command> {
    let mut command = Command::new("rundll32");
    command.arg("url.dll,FileProtocolHandler");
    Some(command)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(crate) fn opener() -> Option<Command> {
    None
}
