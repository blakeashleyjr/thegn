//! `thegn agent <action>` — install + configure thegn's OWN pi under
//! `~/.thegn/pi`: a pinned `@earendil-works/pi-coding-agent` binary plus a
//! managed agent dir (`PI_CODING_AGENT_DIR`) seeded with the repo's `thegn-acp`
//! package. Self-contained + reproducible, used by the "Agent" picker entry on the
//! host and (carried) inside sprites — instead of the host's global `pi`/`~/.pi`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use thegn_core::managed_tool::{ManagedTool, UpdatePolicy};
use thegn_core::{msg, outln, util};

/// The managed pi as a [`ManagedTool`] spec. Pi keeps its legacy layout
/// (`~/.thegn/pi`, `node_modules/.bin/pi`, `.thegn-pi-version`) for
/// byte-for-byte compatibility with existing installs and carried sprites; the
/// `pi`-on-PATH tier-2 fallback covers the npm-absent case. See
/// [`thegn_core::managed_tool`] for the shared resolver.
pub fn pi_tool() -> ManagedTool {
    ManagedTool::npm(
        "pi",
        "@earendil-works/pi-coding-agent",
        "pi",
        crate::pi_assets::PI_PIN,
    )
    .with_policy(UpdatePolicy::Once)
    .with_path_fallbacks(&["pi"])
    .with_layout(
        util::managed_pi_dir(),
        "node_modules/.bin/pi",
        ".thegn-pi-version",
    )
}

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Install/refresh the managed pi (pinned binary + thegn-acp extension + config).
    Setup {
        /// Reinstall the pinned binary even if the pinned version is already present.
        #[arg(long)]
        force: bool,
    },
    /// Print the managed pi paths (binary + PI_CODING_AGENT_DIR).
    Path,
}

pub fn run(cfg: &thegn_core::config::Config, action: Action) -> Result<()> {
    match action {
        Action::Setup { force } => {
            setup(force)?;
            inject_mcp_servers(cfg);
            Ok(())
        }
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
    pi_tool().bin_path()
}

/// Merge user-declared MCP servers (`[mcp_servers.<name>]`) into the managed
/// pi's `settings.json` under the de-facto `mcpServers` key — additive, so
/// `pi install`'s own keys (packages, …) are preserved. Best-effort: settings
/// are agent config (a cache-like artifact), so a read/parse/write failure logs
/// and never fails `agent setup`. No-op when no servers are declared.
pub fn inject_mcp_servers(cfg: &thegn_core::config::Config) {
    if cfg.mcp_servers.is_empty() {
        return;
    }
    let path = util::managed_pi_agent_dir().join("settings.json");
    let mut root = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = root.as_object_mut() {
        obj.insert(
            "mcpServers".to_string(),
            thegn_core::mcp::config::settings_block(&cfg.mcp_servers),
        );
        match serde_json::to_string_pretty(&root) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&path, s) {
                    tracing::debug!(target: "thegn::provision", error = %e, "best-effort: write pi settings.json (mcpServers)");
                } else {
                    msg::info(&format!(
                        "injected {} MCP server(s) into pi settings",
                        cfg.mcp_servers.len()
                    ));
                }
            }
            Err(e) => {
                tracing::debug!(target: "thegn::provision", error = %e, "best-effort: serialize pi settings.json")
            }
        }
    }
}

/// `true` when the pinned binary is present at the current `PI_PIN` — used to
/// skip the (slow) npm install on re-runs and by the launch-time ensure check.
pub fn is_current() -> bool {
    pi_tool().is_current()
}

/// Idempotent install + configure of the managed pi. Safe to re-run: the binary
/// install is skipped when already at the pin (unless `force`), but the extension
/// package + registration are always re-seeded so an extension update shipped with
/// a new thegn build lands.
pub fn setup(force: bool) -> Result<()> {
    let dir = util::managed_pi_dir();
    let agent = util::managed_pi_agent_dir();
    let pin = crate::pi_assets::PI_PIN;
    let tool = pi_tool();
    std::fs::create_dir_all(&agent).with_context(|| format!("create {}", agent.display()))?;

    // 1. Pinned binary (npm --prefix → <dir>/node_modules/.bin/pi), through the
    //    shared managed-tool resolver. The install gate is the tool's update
    //    policy (Once ⇒ install unless force or not-current).
    if tool.needs_install(force) {
        msg::info(&format!(
            "installing pinned pi {pin} into {}",
            dir.display()
        ));
        crate::managed_tool::acquire(&tool)?;
    } else {
        msg::info(&format!("pinned pi {pin} already installed"));
    }

    // 2. Seed the thegn-acp package INSIDE the agent dir (so settings.json's
    //    relative package path stays valid when the dir is carried to a sprite).
    let pkg = agent.join("packages").join("thegn-acp");
    crate::pi_assets::seed_package(&pkg).context("seed thegn-acp package")?;

    // 3. Register it via the pinned pi (`pi install <relative path>` writes
    //    settings.json `{ "packages": ["packages/thegn-acp"] }`).
    register(&agent)?;

    // Marker written last (after registration), so `is_current()` — and the
    // launch-time auto-ensure gate that keys on it — means "fully set up",
    // not merely "binary present".
    crate::managed_tool::mark_installed(&tool);
    msg::info(&format!(
        "managed pi ready — binary {}, PI_CODING_AGENT_DIR={}",
        managed_pi_bin().display(),
        agent.display()
    ));
    Ok(())
}

/// `pi install packages/thegn-acp` against the managed agent dir. Uses the
/// pinned binary when present, else a `pi` on PATH (the `npm`-absent fallback).
fn register(agent: &Path) -> Result<()> {
    let bin = managed_pi_bin();
    let pi = if bin.exists() {
        bin
    } else {
        PathBuf::from("pi")
    };
    let mut cmd = Command::new(&pi);
    cmd.arg("install")
        .arg("packages/thegn-acp")
        .current_dir(agent)
        .env("PI_CODING_AGENT_DIR", agent);
    crate::managed_tool::run_setup_cmd(
        cmd,
        &format!("{} install packages/thegn-acp", pi.display()),
        "registering thegn-acp (pi install) failed",
    )
}
