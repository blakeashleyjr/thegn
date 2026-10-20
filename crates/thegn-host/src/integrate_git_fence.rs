//! Shared command fence for host-automatic Git in merge-queue fold paths.

use std::path::Path;
use std::process::Command;

/// THE-225: host-automatic Git in candidate-derived worktrees never runs
/// repository hooks or fsmonitor; user-invoked Git remains the user-owned exception.
pub(super) const HOST_GIT_FENCE: [&str; 4] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
];

pub(super) fn cmd(dir: &Path) -> Command {
    let mut command = thegn_core::util::git_cmd(dir);
    command.args(HOST_GIT_FENCE);
    command
}

#[expect(
    clippy::disallowed_methods,
    reason = "fold paths run off the UI loop (CLI or spawn_blocking)"
)]
pub(super) fn ok(dir: &Path, args: &[&str]) -> bool {
    thegn_core::util::note_core_git_spawn();
    cmd(dir)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub(super) fn out(dir: &Path, args: &[&str]) -> Option<String> {
    out_allow_empty(dir, args).filter(|output| !output.is_empty())
}

#[expect(
    clippy::disallowed_methods,
    reason = "fold paths run off the UI loop (CLI or spawn_blocking)"
)]
pub(super) fn out_allow_empty(dir: &Path, args: &[&str]) -> Option<String> {
    thegn_core::util::note_core_git_spawn();
    let output = cmd(dir).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
#[path = "integrate_git_fence_tests.rs"]
mod tests;
