//! `superzej pin` — pinned programs as their own session tabs.
//!
//! A *pin* is a named program (config `[[pins]]`) that lives as its own zellij
//! tab named `pin:<name>`, alongside the `{slug}/home` / `{slug}/{branch}`
//! workspace/worktree tabs. It's summoned from anywhere with `Alt-1..9` (the
//! 1-based registration order) or the tabbar's pin chips:
//!
//! - `pin open <name|index>` — launch-or-focus: `go-to-tab-name` if the pin's
//!   tab exists, else open it with the `pin-tab` layout. The keybind/chip entry.
//! - `pin exec --in-place` — runs *inside* the pin tab's center pane (the layout
//!   invokes it); resolves the pin from the focused tab name and execs it, so
//!   the pane *becomes* the program (mirrors `pick-agent --in-place`).
//! - `pin list --json` — the tabbar's pin-chip feed.
//!
//! Pins are global (reachable from every tab) and run on the host — no sandbox
//! wrap, unlike `tool.rs`. "Running" is just "a `pin:<name>` tab exists", which
//! the tabbar derives from its `TabUpdate`; no DB row or state file is kept.

use crate::cli::PinAction;
use crate::config::{Config, Pin};
use crate::{msg, util, zellij};
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

/// Tab-name prefix marking a pin tab. Slash-free so the chrome plugins can tell
/// pins apart from `{slug}/…` worktree tabs at a glance.
const PIN_PREFIX: &str = "pin:";

/// A pin in the tabbar's JSON feed (1-based `index` = the `Alt-N` mapping).
#[derive(Serialize)]
struct PinView<'a> {
    index: usize,
    name: &'a str,
}

pub fn run(cfg: &Config, action: PinAction) -> Result<()> {
    match action {
        PinAction::Open { target } => open(cfg, &target),
        PinAction::Exec { .. } => exec(cfg),
        PinAction::List { json } => list(cfg, json),
    }
}

/// The tab name for a pin (`pin:<name>`).
fn tab_name(name: &str) -> String {
    format!("{PIN_PREFIX}{name}")
}

/// The pin name carried by a `pin:<name>` tab (`None` for non-pin tabs).
fn pin_name_of_tab(tab: &str) -> Option<&str> {
    tab.strip_prefix(PIN_PREFIX)
}

/// Resolve a pin by 1-based index (all-digit target) or by name.
fn resolve<'a>(cfg: &'a Config, target: &str) -> Option<&'a Pin> {
    match target.parse::<usize>() {
        Ok(idx) => cfg.pin_by_index(idx),
        Err(_) => cfg.pin(target),
    }
}

/// Launch-or-focus: focus the pin's tab if present, else open it.
fn open(cfg: &Config, target: &str) -> Result<()> {
    let pin =
        resolve(cfg, target).unwrap_or_else(|| msg::die(&format!("pin: unknown pin '{target}'")));
    if !zellij::in_zellij() {
        msg::info(&format!("(not in zellij) would open pin '{}'", pin.name));
        return Ok(());
    }
    let tab = tab_name(&pin.name);
    if zellij::tab_names().iter().any(|t| t == &tab) {
        zellij::go_to_tab_name(&tab);
    } else {
        let cwd = pin
            .cwd
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(util::home);
        zellij::new_tab(&tab, &cwd, Some("pin-tab"));
    }
    Ok(())
}

/// The pin-tab center pane: resolve the pin from the focused tab name and exec
/// its command in place — this process *becomes* the program.
fn exec(cfg: &Config) -> Result<()> {
    let name = focused_pin_name().unwrap_or_else(|| msg::die("pin exec: not in a pin tab"));
    let cmd = cfg
        .pin(&name)
        .map(|p| p.command.clone())
        .unwrap_or_else(|| msg::die(&format!("pin exec: unknown pin '{name}'")));
    zellij::rename_pane(&format!("\u{1f4cc} {name}")); // 📌
    util::exec_shell_cmd(&cmd); // diverges
}

/// The pin name from the focused tab, retry-polling briefly to dodge the
/// post-`new-tab` focus race that `new_tab.rs` already relies on.
fn focused_pin_name() -> Option<String> {
    for _ in 0..10 {
        if let Some(name) = zellij::focused_tab_name()
            .as_deref()
            .and_then(pin_name_of_tab)
        {
            return Some(name.to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Print configured pins (JSON for the tabbar, else a TSV).
fn list(cfg: &Config, json: bool) -> Result<()> {
    if json {
        let views: Vec<PinView> = cfg
            .pins
            .iter()
            .enumerate()
            .map(|(i, p)| PinView {
                index: i + 1,
                name: &p.name,
            })
            .collect();
        crate::outln!("{}", serde_json::to_string(&views)?);
    } else {
        for (i, p) in cfg.pins.iter().enumerate() {
            crate::outln!("{}\t{}\t{}", i + 1, p.name, p.command);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(pins: &[(&str, &str)]) -> Config {
        Config {
            pins: pins
                .iter()
                .map(|(n, cmd)| Pin {
                    name: (*n).into(),
                    command: (*cmd).into(),
                    cwd: None,
                })
                .collect(),
            ..Config::default()
        }
    }

    #[test]
    fn tab_name_round_trips() {
        assert_eq!(tab_name("aerc"), "pin:aerc");
        assert_eq!(pin_name_of_tab("pin:aerc"), Some("aerc"));
        assert_eq!(pin_name_of_tab("repo/home"), None);
        assert_eq!(pin_name_of_tab("repo/feat \u{b7}2"), None);
    }

    #[test]
    fn resolves_by_index_and_name() {
        let cfg = cfg_with(&[("aerc", "aerc"), ("logs", "journalctl -f")]);
        assert_eq!(resolve(&cfg, "1").map(|p| p.name.as_str()), Some("aerc"));
        assert_eq!(resolve(&cfg, "2").map(|p| p.name.as_str()), Some("logs"));
        assert_eq!(resolve(&cfg, "logs").map(|p| p.name.as_str()), Some("logs"));
        // Out-of-range index and unknown name both miss.
        assert!(resolve(&cfg, "3").is_none());
        assert!(resolve(&cfg, "0").is_none());
        assert!(resolve(&cfg, "nope").is_none());
    }

    #[test]
    fn list_json_is_indexed_array() {
        let cfg = cfg_with(&[("aerc", "aerc"), ("logs", "journalctl -f")]);
        let views: Vec<PinView> = cfg
            .pins
            .iter()
            .enumerate()
            .map(|(i, p)| PinView {
                index: i + 1,
                name: &p.name,
            })
            .collect();
        let json = serde_json::to_string(&views).unwrap();
        assert_eq!(
            json,
            r#"[{"index":1,"name":"aerc"},{"index":2,"name":"logs"}]"#
        );
    }
}
