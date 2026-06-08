//! `superzej pin` — configured pinned programs.
//!
//! A *pin* is a named program (config `[[pins]]`) summoned from anywhere with
//! `Alt-1..9` (the 1-based registration order) or the tabbar's pin chips. Pins
//! support two placements:
//!
//! - `location = "tab"` (default) — launch-or-focus a dedicated `pin:<name>`
//!   session tab using the `pin-tab` layout.
//! - `location = "layout"` — add a named tiled pane (`📌 <name>`) to the focused
//!   tab's active layout.
//!
//! `pin exec --in-place` runs only inside tab-placement pin tabs; layout pins run
//! their configured command directly in the newly-created pane. Pins are global
//! (reachable from every tab) and run on the host — no sandbox wrap, unlike
//! `tool.rs`. "Running" for tabbar chips is still tab-existence based; layout-pin
//! status tracking belongs to a later singleton/health slice.

use crate::cli::PinAction;
use crate::commands::resolve as resolve_cmd;
use crate::config::{Config, Pin, PinLocation};
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
    location: &'static str,
}

pub fn run(cfg: &Config, action: PinAction) -> Result<()> {
    match action {
        PinAction::Open { target, session } => open(cfg, &target, session),
        PinAction::Exec { .. } => exec(cfg),
        PinAction::Close { target, session } => {
            close(cfg, &target, session);
            Ok(())
        }
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

#[derive(Debug, PartialEq, Eq)]
enum OpenPlan {
    Tab {
        tab: String,
        cwd: PathBuf,
        layout: &'static str,
    },
    LayoutPane {
        pane_name: String,
        cwd: PathBuf,
        command: String,
    },
}

fn pane_name(name: &str) -> String {
    format!("\u{1f4cc} {name}")
}

fn open_plan(pin: &Pin, active_dir: Option<PathBuf>) -> OpenPlan {
    match pin.location {
        PinLocation::Tab => OpenPlan::Tab {
            tab: tab_name(&pin.name),
            cwd: pin
                .cwd
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(util::home),
            layout: "pin-tab",
        },
        PinLocation::Layout => OpenPlan::LayoutPane {
            pane_name: pane_name(&pin.name),
            cwd: pin
                .cwd
                .clone()
                .map(PathBuf::from)
                .or(active_dir)
                .unwrap_or_else(util::home),
            command: pin.command.clone(),
        },
    }
}

/// Launch-or-focus according to the pin's configured placement.
fn open(cfg: &Config, target: &str, session: Option<String>) -> Result<()> {
    let pin =
        resolve(cfg, target).unwrap_or_else(|| msg::die(&format!("pin: unknown pin '{target}'")));
    if let Some(s) = session.as_deref() {
        // Plugin-spawned commands target the active superzej session explicitly;
        // this also makes zellij::in_zellij() true for action wrappers.
        std::env::set_var("ZELLIJ_SESSION_NAME", s);
    }
    if !zellij::in_zellij() {
        match pin.location {
            PinLocation::Tab => {
                msg::info(&format!("(not in zellij) would open pin '{}'", pin.name))
            }
            PinLocation::Layout => msg::info(&format!(
                "(not in zellij) would add pin '{}' into active layout",
                pin.name
            )),
        }
        return Ok(());
    }
    match pin.location {
        PinLocation::Tab => open_tab(pin),
        PinLocation::Layout => open_layout(pin, session.as_deref()),
    }
    Ok(())
}

fn open_tab(pin: &Pin) {
    let OpenPlan::Tab { tab, cwd, layout } = open_plan(pin, None) else {
        unreachable!("tab pins produce tab plans")
    };
    if zellij::tab_names().iter().any(|t| t == &tab) {
        zellij::go_to_tab_name(&tab);
    } else {
        zellij::new_tab(&tab, &cwd, Some(layout));
    }
}

/// Close a running pin tab by name or index.
fn close(cfg: &Config, target: &str, session: Option<String>) {
    if let Some(s) = session.as_deref() {
        std::env::set_var("ZELLIJ_SESSION_NAME", s);
    }
    let pin = resolve(cfg, target)
        .unwrap_or_else(|| msg::die(&format!("pin: unknown pin '{target}'")));
    let tab = tab_name(&pin.name);
    if zellij::tab_names().iter().any(|t| t == &tab) {
        zellij::close_tab_name(&tab);
    } else {
        msg::info(&format!("pin '{}' is not running", pin.name));
    }
}

fn open_layout(pin: &Pin, session: Option<&str>) {
    let active_dir = active_tab_dir(session);
    if pin.cwd.is_none() && active_dir.is_none() {
        msg::warn(&format!(
            "pin '{}': could not resolve active tab directory; using $HOME",
            pin.name
        ));
    }
    let OpenPlan::LayoutPane {
        pane_name,
        cwd,
        command,
    } = open_plan(pin, active_dir)
    else {
        unreachable!("layout pins produce layout-pane plans")
    };
    let sh = util::shell();
    if !zellij::new_pane_cmd(&cwd, &pane_name, "Right", &[&sh, "-lc", &command]) {
        msg::warn(&format!(
            "pin '{}': zellij did not create layout pane",
            pin.name
        ));
    }
}

fn active_tab_dir(session: Option<&str>) -> Option<PathBuf> {
    let session = session
        .map(str::to_string)
        .unwrap_or_else(crate::db::session);
    let tab = zellij::focused_tab_name()?;
    resolve_cmd::resolve_tab_dir(&session, &tab).map(PathBuf::from)
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
                location: p.location.as_str(),
            })
            .collect();
        crate::outln!("{}", serde_json::to_string(&views)?);
    } else {
        for (i, p) in cfg.pins.iter().enumerate() {
            crate::outln!("{}\t{}\t{}\t{}", i + 1, p.name, p.location, p.command);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PinLocation, PinRestart, PinScope, PinStart};
    use std::path::Path;

    fn cfg_with(pins: &[(&str, &str)]) -> Config {
        Config {
            pins: pins
                .iter()
                .map(|(n, cmd)| Pin {
                    name: (*n).into(),
                    command: (*cmd).into(),
                    cwd: None,
                    location: PinLocation::Tab,
                    scope: PinScope::Global,
                    workspace: None,
                    start: PinStart::Lazy,
                    restart: PinRestart::Never,
                    singleton: true,
                })
                .collect(),
            ..Config::default()
        }
    }

    fn pin(name: &str, command: &str, cwd: Option<&str>, location: PinLocation) -> Pin {
        Pin {
            name: name.into(),
            command: command.into(),
            cwd: cwd.map(str::to_string),
            location,
            scope: PinScope::Global,
            workspace: None,
            start: PinStart::Lazy,
            restart: PinRestart::Never,
            singleton: true,
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
                location: p.location.as_str(),
            })
            .collect();
        let json = serde_json::to_string(&views).unwrap();
        assert_eq!(
            json,
            r#"[{"index":1,"name":"aerc","location":"tab"},{"index":2,"name":"logs","location":"tab"}]"#
        );
    }

    #[test]
    fn tab_pin_plan_uses_pin_tab_layout_and_home_default() {
        let p = pin("aerc", "aerc", None, PinLocation::Tab);
        assert_eq!(
            open_plan(&p, Some(Path::new("/repo/wt").to_path_buf())),
            OpenPlan::Tab {
                tab: "pin:aerc".into(),
                cwd: util::home(),
                layout: "pin-tab",
            }
        );
    }

    #[test]
    fn layout_pin_plan_uses_active_dir_when_cwd_missing() {
        let p = pin("logs", "journalctl -f", None, PinLocation::Layout);
        assert_eq!(
            open_plan(&p, Some(Path::new("/repo/wt").to_path_buf())),
            OpenPlan::LayoutPane {
                pane_name: "\u{1f4cc} logs".into(),
                cwd: Path::new("/repo/wt").to_path_buf(),
                command: "journalctl -f".into(),
            }
        );
    }

    #[test]
    fn layout_pin_plan_honors_cwd_override() {
        let p = pin("mail", "aerc", Some("/mail"), PinLocation::Layout);
        assert_eq!(
            open_plan(&p, Some(Path::new("/repo/wt").to_path_buf())),
            OpenPlan::LayoutPane {
                pane_name: "\u{1f4cc} mail".into(),
                cwd: Path::new("/mail").to_path_buf(),
                command: "aerc".into(),
            }
        );
    }

    #[test]
    fn layout_pin_plan_names_pane_with_pin_prefix() {
        assert_eq!(pane_name("logs"), "\u{1f4cc} logs");
    }
}
