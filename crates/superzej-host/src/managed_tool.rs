//! Host-side acquisition for [`superzej_core::managed_tool`] specs.
//!
//! `superzej-core` decides *which* tier resolves a tool, *which* release asset
//! matches the platform, and *whether* an install is needed — but it carries no
//! HTTP client. This module performs the side effect: an `npm install` for
//! `Npm` sources, or a GitHub-release download + `chmod +x` for `GithubRelease`.
//! It runs off the event loop (the CLI path, or `spawn_blocking` when the
//! compositor provisions a tool) exactly as the managed pi install does — never
//! on the loop — and surfaces failures rather than degrading silently.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use superzej_core::managed_tool::{Arch, ManagedTool, Os, Source};
use superzej_core::{msg, util};

/// Run a setup subprocess, capturing output when the TUI is active (so npm
/// progress never paints over the alt-screen frame) and inheriting stdio on the
/// CLI. Shared by the pi setup and generic tool installs. `fail` is the message
/// when the child exits non-zero.
// CLI path or off-loop (sprite provisioning runs it from spawn_blocking); the
// blocking wait never happens on the event loop.
#[expect(clippy::disallowed_methods)]
pub fn run_setup_cmd(mut cmd: Command, ctx: &str, fail: &str) -> Result<()> {
    if msg::tui_active() {
        let out = cmd.output().with_context(|| ctx.to_string())?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stdout.trim().is_empty() || !stderr.trim().is_empty() {
            tracing::debug!(
                target: "szhost::provision",
                cmd = ctx,
                stdout = %stdout.trim(),
                stderr = %stderr.trim(),
                "managed-tool setup subprocess output (captured; not painted on the frame)"
            );
        }
        anyhow::ensure!(out.status.success(), "{fail}: {}", stderr.trim());
    } else {
        let status = cmd.status().with_context(|| ctx.to_string())?;
        anyhow::ensure!(status.success(), "{fail}");
    }
    Ok(())
}

/// The managed tools superzej knows about, for `doctor` reporting and (later)
/// pre-provisioning. Today just the managed pi.
pub fn known() -> Vec<ManagedTool> {
    vec![
        crate::cmd::agent::pi_tool(),
        superzej_core::debug::bs_tool(),
    ]
}

/// Acquire a tool's binary into its managed dir — the raw fetch, without the
/// `needs_install` gate or version-marker write (callers own those, so the pi
/// setup can preserve its exact ordering). `Npm` shells out to `npm install
/// --prefix`; `GithubRelease` downloads the platform asset and marks it
/// executable.
pub fn acquire(tool: &ManagedTool) -> Result<()> {
    match &tool.source {
        Source::Npm { package } => {
            anyhow::ensure!(
                util::have("npm"),
                "npm not found — needed to install {package}@{}. \
                 Install Node/npm, or put the tool on PATH.",
                tool.version
            );
            let mut cmd = Command::new("npm");
            cmd.args(["install", "--prefix"])
                .arg(tool.managed_dir())
                .arg(format!("{package}@{}", tool.version));
            run_setup_cmd(
                cmd,
                &format!("npm install {package}@{}", tool.version),
                &format!("npm install {package}@{} failed", tool.version),
            )
        }
        Source::Cargo { crate_name } => {
            anyhow::ensure!(
                util::have("cargo"),
                "cargo not found — needed to install {crate_name} {}. \
                 Install the Rust toolchain, or put the tool on PATH.",
                tool.version
            );
            let mut cmd = Command::new("cargo");
            cmd.args(["install", crate_name, "--version", &tool.version, "--root"])
                .arg(tool.managed_dir())
                .arg("--locked");
            run_setup_cmd(
                cmd,
                &format!("cargo install {crate_name} --version {}", tool.version),
                &format!("cargo install {crate_name} {} failed", tool.version),
            )
        }
        Source::GithubRelease { repo, .. } => {
            let os = Os::current().context("unsupported OS for a managed download")?;
            let arch =
                Arch::current().context("unsupported architecture for a managed download")?;
            let asset = tool.asset_for(os, arch).with_context(|| {
                format!(
                    "{}: no release asset for this platform/architecture",
                    tool.name
                )
            })?;
            let url = format!(
                "https://github.com/{repo}/releases/download/{}/{asset}",
                tool.version
            );
            let bin = tool.bin_path();
            if let Some(parent) = bin.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            download_to(&url, &bin)?;
            make_executable(&bin)?;
            Ok(())
        }
    }
}

/// Ensure a tool is installed and its version marker recorded: gate on
/// [`ManagedTool::needs_install`], [`acquire`], then mark. The generic one-call
/// path for tools without a bespoke setup (the pi setup drives [`acquire`]
/// directly to preserve its seed/register ordering; the debugger uses this).
pub fn install(tool: &ManagedTool, force: bool) -> Result<()> {
    if !tool.needs_install(force) {
        return Ok(());
    }
    acquire(tool)?;
    mark_installed(tool);
    Ok(())
}

/// Record the pinned version in the tool's marker file. Best-effort: the marker
/// is a cache (a missed write just triggers a reinstall next time), so its
/// failure must never fail the install.
pub fn mark_installed(tool: &ManagedTool) {
    if let Err(e) = std::fs::write(tool.version_marker(), &tool.version) {
        tracing::debug!(
            target: "szhost::provision",
            tool = %tool.name,
            error = %e,
            "best-effort: failed to write managed-tool version marker"
        );
    }
}

fn download_to(url: &str, dest: &Path) -> Result<()> {
    let resp = reqwest::blocking::get(url).with_context(|| format!("GET {url}"))?;
    anyhow::ensure!(
        resp.status().is_success(),
        "download {url} failed: HTTP {}",
        resp.status()
    );
    let bytes = resp
        .bytes()
        .with_context(|| format!("read body of {url}"))?;
    std::fs::write(dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod +x {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}
