//! Debugger integration — pure domain logic for launching a debugger session.
//!
//! superzej's debugger is BugStalker (`bs`): a self-contained Rust/Linux TUI
//! debugger installed from crates.io. It is acquired and pinned through the
//! shared [`crate::managed_tool`] resolver (a `Cargo` source), and launched as an
//! ordinary interactive program. Because a debug session is started inside a
//! superzej pane, it inherits that pane's sandbox and remote placement for free —
//! no DAP client and no extra wrapping needed.
//!
//! Everything here is pure and unit-tested: the pinned tool spec, the
//! platform gate (BugStalker is Linux-x86-64 only), and the session-argv
//! builders. The `cargo install` fetch and the process exec live in the host.

use crate::managed_tool::{Arch, ManagedTool, Os, UpdatePolicy};

/// Pinned BugStalker version (crates.io `bugstalker`). Bump to adopt a newer
/// debugger release.
pub const BS_PIN: &str = "0.4.6";

/// The BugStalker managed-tool spec: `cargo install bugstalker` (binary `bs`),
/// resolved override → PATH (`bs`) → managed `~/.superzej/tools/bugstalker/bin/bs`.
pub fn bs_tool() -> ManagedTool {
    ManagedTool::cargo("bugstalker", "bugstalker", "bs", BS_PIN)
        .with_policy(UpdatePolicy::Once)
        .with_path_fallbacks(&["bs"])
}

/// Whether BugStalker can run on `(os, arch)`. BugStalker supports **Linux
/// x86-64 only**; everywhere else superzej refuses rather than attempting an
/// install/launch that cannot work.
pub fn bs_supported(os: Os, arch: Arch) -> bool {
    os == Os::Linux && arch == Arch::X64
}

/// [`bs_supported`] for the host superzej is running on. `false` when the
/// platform can't be determined.
pub fn platform_supported() -> bool {
    match (Os::current(), Arch::current()) {
        (Some(os), Some(arch)) => bs_supported(os, arch),
        _ => false,
    }
}

/// A short, user-facing reason a debug session can't start here (or `None` when
/// it can). Used by the CLI/doctor to explain the platform gate.
pub fn unsupported_reason() -> Option<&'static str> {
    if platform_supported() {
        None
    } else {
        Some(
            "BugStalker supports only Linux on x86-64; install it there (or via your distro / nix) to debug on this host",
        )
    }
}

/// argv to launch `program` (with `args`) under the debugger: `bs <program>
/// [args…]` — the debugee is a positional argument and the rest are passed
/// through to it.
pub fn launch_argv(bin: &str, program: &str, args: &[String]) -> Vec<String> {
    let mut argv = vec![bin.to_string(), program.to_string()];
    argv.extend(args.iter().cloned());
    argv
}

/// argv to attach the debugger to a running process: `bs -p <pid>`.
pub fn attach_argv(bin: &str, pid: i64) -> Vec<String> {
    vec![bin.to_string(), "-p".to_string(), pid.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bs_tool_is_cargo_sourced_and_pinned() {
        let t = bs_tool();
        assert_eq!(t.name, "bugstalker");
        assert_eq!(t.version, BS_PIN);
        // Cargo source ⇒ no release asset; resolves to a managed bin named `bs`.
        assert_eq!(t.asset_for(Os::Linux, Arch::X64), None);
        assert!(t.bin_path().ends_with("bin/bs"));
        assert!(t.path_fallbacks.contains(&"bs".to_string()));
    }

    #[test]
    fn supported_only_on_linux_x64() {
        assert!(bs_supported(Os::Linux, Arch::X64));
        assert!(!bs_supported(Os::Linux, Arch::Arm64));
        assert!(!bs_supported(Os::Macos, Arch::X64));
        assert!(!bs_supported(Os::Macos, Arch::Arm64));
        assert!(!bs_supported(Os::Windows, Arch::X64));
        // The reason string is present iff the host is unsupported.
        assert_eq!(unsupported_reason().is_none(), platform_supported());
    }

    #[test]
    fn launch_argv_builds_program_and_args() {
        assert_eq!(
            launch_argv("bs", "./target/debug/app", &["--flag".into(), "x".into()]),
            vec![
                "bs".to_string(),
                "./target/debug/app".into(),
                "--flag".into(),
                "x".into()
            ]
        );
        // No extra args → just the debugee.
        assert_eq!(
            launch_argv("/opt/bs", "prog", &[]),
            vec!["/opt/bs".to_string(), "prog".into()]
        );
    }

    #[test]
    fn attach_argv_builds_pid_form() {
        assert_eq!(
            attach_argv("bs", 4242),
            vec!["bs".to_string(), "-p".into(), "4242".into()]
        );
    }
}
