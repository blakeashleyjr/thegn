//! The thegn pi package, baked into the thegn binary.
//!
//! The repo ships a pi package at `extensions/` — the ACP bridge extension
//! (`thegn-acp.ts`: routes the model through thegn's LLM proxy, exposes the
//! house tools over MCP-over-ACP, and under `THEGN_BOUNCER=1` gates
//! bash/read/edit/write through thegn) plus the `thegn-house` skill. We
//! `include_str!` it here so `thegn agent setup` can seed the MANAGED pi
//! (`~/.thegn/pi/agent`) anywhere — no repo checkout on PATH — and so the SAME
//! bytes seed a sprite. A thegn rebuild ships extension updates; `agent setup`
//! re-seeds on a [`PI_PIN`] bump.

use std::io::Result;
use std::path::Path;

/// Pinned `@earendil-works/pi-coding-agent` version installed under the managed
/// dir. Tracks the repo extension's dependency (`extensions/package.json`,
/// `^0.80`); bump in lockstep when the extension needs a newer pi.
pub const PI_PIN: &str = "0.80.2";

const THEGN_ACP_TS: &str = include_str!("../../../extensions/thegn-acp.ts");
const PACKAGE_JSON: &str = include_str!("../../../extensions/package.json");
const HOUSE_SKILL_MD: &str = include_str!("../../../extensions/skills/thegn-house/SKILL.md");

/// Write the embedded thegn-acp package into `pkg_dir` (the
/// `…/agent/packages/thegn-acp` directory): the extension `.ts`, its
/// `package.json`, and the `thegn-house` skill. Overwrites in place so an
/// extension update from a newer thegn build lands on re-seed.
pub fn seed_package(pkg_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(pkg_dir)?;
    std::fs::write(pkg_dir.join("thegn-acp.ts"), THEGN_ACP_TS)?;
    std::fs::write(pkg_dir.join("package.json"), PACKAGE_JSON)?;
    let skill = pkg_dir.join("skills").join("thegn-house");
    std::fs::create_dir_all(&skill)?;
    std::fs::write(skill.join("SKILL.md"), HOUSE_SKILL_MD)?;
    Ok(())
}
