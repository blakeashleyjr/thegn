//! The superzej pi package, baked into the szhost binary.
//!
//! The repo ships a pi package at `extensions/` — the ACP bridge extension
//! (`superzej-acp.ts`: routes the model through superzej's LLM proxy, exposes the
//! house tools over MCP-over-ACP, and under `SUPERZEJ_BOUNCER=1` gates
//! bash/read/edit/write through superzej) plus the `superzej-house` skill. We
//! `include_str!` it here so `szhost agent setup` can seed the MANAGED pi
//! (`~/.superzej/pi/agent`) anywhere — no repo checkout on PATH — and so the SAME
//! bytes seed a sprite. A szhost rebuild ships extension updates; `agent setup`
//! re-seeds on a [`PI_PIN`] bump.

use std::io::Result;
use std::path::Path;

/// Pinned `@earendil-works/pi-coding-agent` version installed under the managed
/// dir. Tracks the repo extension's dependency (`extensions/package.json`,
/// `^0.80`); bump in lockstep when the extension needs a newer pi.
pub const PI_PIN: &str = "0.80.2";

const SUPERZEJ_ACP_TS: &str = include_str!("../../../extensions/superzej-acp.ts");
const PACKAGE_JSON: &str = include_str!("../../../extensions/package.json");
const HOUSE_SKILL_MD: &str = include_str!("../../../extensions/skills/superzej-house/SKILL.md");

/// Write the embedded superzej-acp package into `pkg_dir` (the
/// `…/agent/packages/superzej-acp` directory): the extension `.ts`, its
/// `package.json`, and the `superzej-house` skill. Overwrites in place so an
/// extension update from a newer szhost build lands on re-seed.
pub fn seed_package(pkg_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(pkg_dir)?;
    std::fs::write(pkg_dir.join("superzej-acp.ts"), SUPERZEJ_ACP_TS)?;
    std::fs::write(pkg_dir.join("package.json"), PACKAGE_JSON)?;
    let skill = pkg_dir.join("skills").join("superzej-house");
    std::fs::create_dir_all(&skill)?;
    std::fs::write(skill.join("SKILL.md"), HOUSE_SKILL_MD)?;
    Ok(())
}
