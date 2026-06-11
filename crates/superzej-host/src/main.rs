//! superzej-host — the native terminal compositor.
//!
//! Phase 1 (the GO/NO-GO spike): a tokio event loop driving a single
//! portable-pty pane through a `PaneEmulator` grid, composited into a termwiz
//! `Surface` that diff-flushes to the outer terminal (the "no-flash" mechanism).

mod center;
mod chrome;
mod compositor;
mod copymode;
mod emulator;
mod keymap;
mod layout;
mod palette;
mod pane;
mod panel;
mod run;
mod sequence;
mod session;
mod task;
mod testkit;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Clone)]
#[command(name = "szhost", version, about = "superzej native terminal host")]
pub struct Cli {
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[arg(long, global = true)]
    pub log_level: Option<String>,

    /// Override a config value (e.g. `--set theme.accent=cyan --set drawer.height=15`)
    #[arg(long = "set", global = true, value_name = "KEY=VALUE")]
    pub overrides: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();
    if let Some(lvl) = cli.log_level.as_deref() {
        cli.overrides.push(format!("log.level={lvl}"));
    }
    run::main(cli).await
}
