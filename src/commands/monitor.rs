//! `superzej monitor <kind>` — open a resource monitor for a top-bar stat as a
//! floating pane. Dispatched by the tabbar plugin when a stat segment is
//! selected and Enter is pressed: `cpu`/`mem` open the system monitor (`btm` by
//! default), `gpu` opens `nvtop`. Both commands are configurable under
//! `[monitor]` in config.toml. The pane floats (not tiled) so it overlays the
//! center column instead of reflowing the chrome layout, and closes on exit.

use crate::config::Config;
use crate::{commands, msg, util, zellij};
use anyhow::Result;

/// The (pane name, shell command) to embed for a stat `kind`, or `None` for an
/// unknown kind. `cpu`/`mem` share the system monitor (named `system`); `gpu`
/// gets its own (named `gpu`). The pane name is stable so the UI/tests can find
/// it. Pure — the side effects live in `run`.
fn plan(cfg: &Config, kind: &str) -> Option<(&'static str, String)> {
    let cmd = cfg.monitor_command(kind)?.to_string();
    let name = if kind == "gpu" { "gpu" } else { "system" };
    Some((name, cmd))
}

pub fn run(cfg: &Config, kind: &str) -> Result<()> {
    let Some((name, cmd)) = plan(cfg, kind) else {
        msg::die(&format!(
            "monitor: unknown stat '{kind}' (want cpu|mem|gpu)"
        ));
    };
    let cwd = commands::resolve_worktree(None);
    if zellij::in_zellij() {
        let sh = util::shell();
        zellij::new_float(&cwd, name, &[&sh, "-lc", &cmd]);
    } else {
        msg::info(&format!(
            "(not in zellij) would run: {cmd}  [cwd={}]",
            cwd.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MonitorConfig;

    #[test]
    fn plan_maps_cpu_and_mem_to_the_system_monitor() {
        let cfg = Config::default();
        assert_eq!(plan(&cfg, "cpu"), Some(("system", "btm".into())));
        assert_eq!(plan(&cfg, "mem"), Some(("system", "btm".into())));
    }

    #[test]
    fn plan_maps_gpu_to_the_gpu_monitor() {
        let cfg = Config::default();
        assert_eq!(plan(&cfg, "gpu"), Some(("gpu", "nvtop".into())));
    }

    #[test]
    fn plan_rejects_unknown_kinds() {
        let cfg = Config::default();
        assert_eq!(plan(&cfg, "disk"), None);
        assert_eq!(plan(&cfg, ""), None);
    }

    #[test]
    fn plan_honors_configured_overrides() {
        let cfg = Config {
            monitor: MonitorConfig {
                system: "htop".into(),
                gpu: "nvitop".into(),
            },
            ..Config::default()
        };
        assert_eq!(plan(&cfg, "cpu"), Some(("system", "htop".into())));
        assert_eq!(plan(&cfg, "mem"), Some(("system", "htop".into())));
        assert_eq!(plan(&cfg, "gpu"), Some(("gpu", "nvitop".into())));
    }

    #[test]
    fn run_outside_zellij_reports_instead_of_spawning() {
        // Clear the inherited session vars (process-local — this never touches a
        // live session) so `in_zellij()` is false and `run` takes the safe
        // "(not in zellij) would run" branch rather than spawning a pane.
        // SAFETY: edition-2021 env API; no other test in this crate reads these.
        std::env::remove_var("ZELLIJ");
        std::env::remove_var("ZELLIJ_SESSION_NAME");
        assert!(!zellij::in_zellij());
        // A known kind resolves + returns Ok without spawning.
        assert!(run(&Config::default(), "cpu").is_ok());
        assert!(run(&Config::default(), "gpu").is_ok());
    }
}
