//! `superzej theme` (internal) — print theme values for the WASM plugins.
//!
//! Plugins can't read config.toml (sandboxed fs), so each one runs this at
//! load and parses the single line: the accent as `R;G;B`.

use crate::config::Config;
use anyhow::Result;

pub fn run(cfg: &Config) -> Result<()> {
    println!("{}", cfg.accent_rgb());
    Ok(())
}
