//! `superzej agent <action>` — install + configure superzej's OWN pi under
//! `~/.superzej/pi`: a pinned `@earendil-works/pi-coding-agent` binary plus a
//! managed agent dir (`PI_CODING_AGENT_DIR`) seeded with the repo's `superzej-acp`
//! package. Self-contained + reproducible, used by the "Agent" picker entry on the
//! host and (carried) inside sprites — instead of the host's global `pi`/`~/.pi`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use superzej_core::{msg, outln, util};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Install/refresh the managed pi (pinned binary + superzej-acp extension + config).
    Setup {
        /// Reinstall the pinned binary even if the pinned version is already present.
        #[arg(long)]
        force: bool,
    },
    /// Print the managed pi paths (binary + PI_CODING_AGENT_DIR).
    Path,
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::Setup { force } => setup(force),
        Action::Path => {
            outln!("binary: {}", managed_pi_bin().display());
            outln!(
                "PI_CODING_AGENT_DIR: {}",
                util::managed_pi_agent_dir().display()
            );
            Ok(())
        }
    }
}

/// The pinned pi binary npm drops under the managed dir.
pub fn managed_pi_bin() -> PathBuf {
    util::managed_pi_dir().join("node_modules/.bin/pi")
}

fn version_marker() -> PathBuf {
    util::managed_pi_dir().join(".superzej-pi-version")
}

/// `true` when the pinned binary is present at the current [`PI_PIN`] — used to
/// skip the (slow) npm install on re-runs and by the launch-time ensure check.
pub fn is_current() -> bool {
    managed_pi_bin().exists()
        && std::fs::read_to_string(version_marker())
            .map(|s| s.trim() == crate::pi_assets::PI_PIN)
            .unwrap_or(false)
}

/// Idempotent install + configure of the managed pi. Safe to re-run: the binary
/// install is skipped when already at the pin (unless `force`), but the extension
/// package + registration are always re-seeded so an extension update shipped with
/// a new szhost build lands.
pub fn setup(force: bool) -> Result<()> {
    let dir = util::managed_pi_dir();
    let agent = util::managed_pi_agent_dir();
    let pin = crate::pi_assets::PI_PIN;
    std::fs::create_dir_all(&agent).with_context(|| format!("create {}", agent.display()))?;

    // 1. Pinned binary (npm --prefix → <dir>/node_modules/.bin/pi).
    if force || !is_current() {
        anyhow::ensure!(
            util::have("npm"),
            "npm not found — needed to install the pinned pi (@earendil-works/pi-coding-agent@{pin}). \
             Install Node/npm, or add a `pi` to PATH and re-run."
        );
        msg::info(&format!(
            "installing pinned pi {pin} into {}",
            dir.display()
        ));
        let status = Command::new("npm")
            .args(["install", "--prefix"])
            .arg(&dir)
            .arg(format!("@earendil-works/pi-coding-agent@{pin}"))
            .status()
            .context("npm install pinned pi")?;
        anyhow::ensure!(
            status.success(),
            "npm install @earendil-works/pi-coding-agent@{pin} failed"
        );
    } else {
        msg::info(&format!("pinned pi {pin} already installed"));
    }

    // 2. Seed the superzej-acp package INSIDE the agent dir (so settings.json's
    //    relative package path stays valid when the dir is carried to a sprite).
    let pkg = agent.join("packages").join("superzej-acp");
    crate::pi_assets::seed_package(&pkg).context("seed superzej-acp package")?;

    // 3. Register it via the pinned pi (`pi install <relative path>` writes
    //    settings.json `{ "packages": ["packages/superzej-acp"] }`).
    register(&agent)?;

    std::fs::write(version_marker(), pin).ok();
    msg::info(&format!(
        "managed pi ready — binary {}, PI_CODING_AGENT_DIR={}",
        managed_pi_bin().display(),
        agent.display()
    ));
    Ok(())
}

/// `pi install packages/superzej-acp` against the managed agent dir. Uses the
/// pinned binary when present, else a `pi` on PATH (the `npm`-absent fallback).
fn register(agent: &Path) -> Result<()> {
    let bin = managed_pi_bin();
    let pi = if bin.exists() {
        bin
    } else {
        PathBuf::from("pi")
    };
    let status = Command::new(&pi)
        .arg("install")
        .arg("packages/superzej-acp")
        .current_dir(agent)
        .env("PI_CODING_AGENT_DIR", agent)
        .status()
        .with_context(|| format!("{} install packages/superzej-acp", pi.display()))?;
    anyhow::ensure!(
        status.success(),
        "registering superzej-acp (pi install) failed"
    );
    Ok(())
}
