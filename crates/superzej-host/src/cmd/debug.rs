//! `szhost debug <action>` — the BugStalker debugger integration.
//!
//! BugStalker (`bs`) is acquired + pinned via the shared managed-tool resolver
//! (a `Cargo` source) and launched as an ordinary interactive program. Started
//! inside a superzej pane, a session inherits that pane's sandbox and remote
//! placement for free — so this verb performs no sandbox/placement wrapping
//! itself. Gated to BugStalker's supported platform (Linux x86-64).

use anyhow::{Result, bail};
use superzej_core::config::Config;
use superzej_core::debug::{self, attach_argv, bs_tool, launch_argv};
use superzej_core::{msg, outln};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Install/refresh the pinned BugStalker (`cargo install bugstalker`).
    Setup {
        /// Reinstall even if the pinned version is already present.
        #[arg(long)]
        force: bool,
    },
    /// Print the resolved `bs` binary path and which tier resolved it.
    Path,
    /// Debug a program under BugStalker: `bs <program> [args…]`. Run inside a
    /// superzej pane to debug within that pane's sandbox/placement.
    Run {
        /// The program (debugee) to launch under the debugger.
        program: String,
        /// Arguments passed through to the debugee.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Attach BugStalker to a running process: `bs -p <pid>`.
    Attach {
        /// PID of the process to attach to.
        pid: i64,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Setup { force } => setup(force),
        Action::Path => path(cfg),
        Action::Run { program, args } => {
            let bin = ensure_bin(cfg)?;
            exec_replace(launch_argv(&bin, &program, &args))
        }
        Action::Attach { pid } => {
            let bin = ensure_bin(cfg)?;
            exec_replace(attach_argv(&bin, pid))
        }
    }
}

/// Refuse early on an unsupported platform with a clear, actionable message.
fn require_supported() -> Result<()> {
    if let Some(reason) = debug::unsupported_reason() {
        bail!("{reason}");
    }
    Ok(())
}

fn setup(force: bool) -> Result<()> {
    require_supported()?;
    let tool = bs_tool();
    if tool.needs_install(force) {
        msg::info(&format!("installing BugStalker {} via cargo", tool.version));
    }
    crate::managed_tool::install(&tool, force)?;
    msg::info(&format!("BugStalker ready — {}", tool.bin_path().display()));
    Ok(())
}

fn path(cfg: &Config) -> Result<()> {
    let tool = bs_tool();
    let res = tool.resolve(
        cfg.managed_tools.get(&tool.name),
        superzej_core::util::which_path,
    );
    outln!("bs: {} ({})", res.path(), res.tier());
    if let Some(reason) = debug::unsupported_reason() {
        outln!("note: {reason}");
    }
    Ok(())
}

/// Resolve `bs`, installing the managed copy if that's the selected tier and it
/// isn't current. Returns the binary path to exec.
fn ensure_bin(cfg: &Config) -> Result<String> {
    require_supported()?;
    let tool = bs_tool();
    use superzej_core::managed_tool::Resolution;
    match tool.resolve(
        cfg.managed_tools.get(&tool.name),
        superzej_core::util::which_path,
    ) {
        Resolution::Override { path, .. } | Resolution::OnPath { path } => Ok(path),
        Resolution::Managed { path, current } => {
            if !current {
                msg::info(&format!("installing BugStalker {} via cargo", tool.version));
                crate::managed_tool::install(&tool, false)?;
            }
            Ok(path)
        }
    }
}

/// Replace this process with the debugger so it owns the terminal. The CLI verb
/// never runs on the compositor event loop, so exec-replacing is safe here.
#[cfg(unix)]
fn exec_replace(argv: Vec<String>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    // `exec` only returns on failure.
    Err(cmd.exec().into())
}

#[cfg(not(unix))]
fn exec_replace(_argv: Vec<String>) -> Result<()> {
    bail!("debug sessions require a Unix host")
}
