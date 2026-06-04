//! `superzej launch` (bare `sj`) — the "new" state. Outside zellij it starts a
//! session; inside, a repo launcher: pick a recent repo or add a new one. Loops
//! so the home tab stays a launcher; "shell here" drops to a prompt.

use crate::config::Config;
use crate::db::Db;
use crate::{commands, picker, util, zellij};
use anyhow::Result;

const ADD_NEW: &str = "+ add a new repo…";
const SHELL_HERE: &str = "· shell here";

pub fn run(cfg: &Config) -> Result<()> {
    if !zellij::in_zellij() {
        util::exec_command("superzej", &["attach"]);
    }

    loop {
        let recents = Db::open()?.recent_repos(20)?;
        let mut options = recents;
        options.push(ADD_NEW.to_string());
        options.push(SHELL_HERE.to_string());

        match picker::pick("superzej — open a repo", &options, &cfg.picker) {
            None => util::exec_shell(),
            Some(c) if c == SHELL_HERE => util::exec_shell(),
            Some(c) if c == ADD_NEW => {
                if let Some(t) = picker::pick_repo(cfg) {
                    commands::new_workspace::run(cfg, Some(t), None)?;
                }
            }
            Some(c) => commands::new_workspace::run(cfg, Some(c), None)?,
        }
    }
}
