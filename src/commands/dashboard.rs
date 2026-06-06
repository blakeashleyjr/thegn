//! `superzej dashboard` — worktree dashboard with two surfaces:
//!   (default) floating fuzzy quick-switcher (Alt-d)
//!   --watch   persistent auto-refreshing table (pinnable pane)
//!   --inner   internal: the fzf UI itself (spawned floating)

use crate::commands::list;
use crate::config::Config;
use crate::db::Db;
use crate::models::WorktreeView;
use crate::{repo, util, worktree, zellij};
use anyhow::Result;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

pub fn run(cfg: &Config, watch: bool, inner: bool) -> Result<()> {
    if watch {
        return watch_loop(cfg);
    }
    if inner {
        return inner_ui(cfg);
    }
    if zellij::in_zellij() {
        let cwd = std::env::current_dir()?;
        zellij::new_float(&cwd, "dashboard", &["superzej", "dashboard", "--inner"]);
        zellij::close_pane();
    } else {
        inner_ui(cfg)?;
    }
    Ok(())
}

fn watch_loop(cfg: &Config) -> Result<()> {
    let interval: u64 = std::env::var("SZ_DASH_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    loop {
        print!("\x1b[2J\x1b[H"); // clear + home
        // "✦ superzej" wordmark (magenta star, accent name) + dim subtitle.
        println!(
            "\x1b[38;2;{}m\u{2726}\x1b[0m \x1b[1m\x1b[38;2;{}msuperzej\x1b[0m \
\x1b[38;2;{}mworktrees · refresh {interval}s\x1b[0m\n",
            crate::theme::MAGENTA,
            cfg.accent_rgb(),
            crate::theme::FAINT,
        );
        list::run(cfg, false)?;
        std::io::stdout().flush().ok();
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

fn inner_ui(cfg: &Config) -> Result<()> {
    let rows = list::collect(cfg)?;
    if rows.is_empty() || !util::have("fzf") {
        list::run(cfg, false)?;
        eprint!("enter to close… ");
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        return Ok(());
    }

    // One line per worktree: visible columns, then a hidden tab-delimited path.
    let lines: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "{:<14.14}  {:<26.26}  +{}/-{}  {}●\t{}",
                r.workspace, r.branch, r.ahead, r.behind, r.dirty, r.path
            )
        })
        .collect();

    let mut child = Command::new("fzf")
        .args([
            "--reverse",
            "--height=100%",
            "--delimiter=\t",
            "--with-nth=1",
            "--pointer=▌",
            "--prompt=worktrees ❯ ",
            "--header=enter=go to tab  ^e=editor  ^g=lazygit  ^x=remove worktree",
            "--expect=enter,ctrl-e,ctrl-g,ctrl-x",
            "--preview=git -C {2} diff --stat 2>/dev/null | head -40",
            &crate::picker::fzf_color(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(lines.join("\n").as_bytes());
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.lines();
    let key = it.next().unwrap_or("").trim();
    let sel = it.next().unwrap_or("");
    let path = match sel.split('\t').nth(1) {
        Some(p) => p,
        None => return Ok(()),
    };
    let row = match rows.iter().find(|r| r.path == path) {
        Some(r) => r,
        None => return Ok(()),
    };

    act(cfg, key, row);
    Ok(())
}

fn act(cfg: &Config, key: &str, row: &WorktreeView) {
    if !zellij::in_zellij() {
        return;
    }
    let wt = Path::new(&row.path);
    match key {
        "ctrl-e" => {
            crate::commands::tool::open_editor(cfg, wt, None);
        }
        "ctrl-g" => {
            zellij::new_float(wt, "lazygit", &["lazygit"]);
        }
        "ctrl-x" => {
            worktree::remove(Path::new(&row.repo), wt, &row.branch, false);
            if let Ok(db) = Db::open() {
                let _ = db.del_worktree(&row.path);
            }
        }
        _ => {
            // Everything is one session now — just jump to the worktree's tab
            // (named `{repo_slug}/{branch}`). No session switch, no teleport.
            let slug = repo::repo_slug(Path::new(&row.repo));
            zellij::go_to_tab_name(&repo::branch_tab(&slug, &row.branch));
        }
    }
}
