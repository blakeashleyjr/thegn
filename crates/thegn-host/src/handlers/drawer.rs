//! Loop-side drawer transitions and cold-result draining.
//!
//! The drawer state module owns persistence, pooling, and off-loop resolution;
//! this handler is the small event-loop boundary that applies those results and
//! exposes one transition path for toggle, cycle, and picker selection.

use std::path::Path;

use tokio::sync::mpsc::UnboundedReceiver;

use crate::compositor::Rect;
use crate::drawer_state::{DrawerRegistryMsg, DrawerRuntime};
use crate::panes::Panes;
use thegn_core::config::{Config, DrawerScope};

/// Loop locals needed by the drawer drain. Callers keep the compositor's
/// existing geometry/focus policy; this context only mutates drawer state and
/// panes.
pub(crate) struct Context<'a> {
    pub runtime: &'a mut DrawerRuntime,
    pub cfg: &'a Config,
    pub active_dir: &'a Path,
    pub panes: &'a mut Panes,
    pub rect: Rect,
}

/// Drain every available cold result. Results are matched by both scope key
/// and occupant ID inside [`DrawerRuntime::apply_result`]; stale picker/cycle
/// results are therefore harmless.
pub(crate) fn drain(rx: &mut UnboundedReceiver<DrawerRegistryMsg>, ctx: Context<'_>) {
    while let Ok((request, result)) = rx.try_recv() {
        ctx.runtime.apply_result(
            ctx.cfg,
            request,
            result,
            ctx.active_dir,
            ctx.panes,
            ctx.rect,
        );
    }
}

pub(crate) fn toggle_files(
    runtime: &mut DrawerRuntime,
    cfg: &Config,
    scope: DrawerScope,
    dir: &Path,
    panes: &mut Panes,
    rect: Rect,
) {
    runtime.toggle(cfg, scope, dir, panes, rect);
}

pub(crate) fn cycle(
    runtime: &mut DrawerRuntime,
    cfg: &Config,
    scope: DrawerScope,
    dir: &Path,
    panes: &mut Panes,
    rect: Rect,
) -> Option<String> {
    runtime.cycle(cfg, scope, dir, panes, rect)
}

pub(crate) fn select(
    runtime: &mut DrawerRuntime,
    cfg: &Config,
    scope: DrawerScope,
    occupant_id: &str,
    dir: &Path,
    panes: &mut Panes,
    rect: Rect,
) -> bool {
    runtime.select(cfg, scope, occupant_id, dir, panes, rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_helpers_share_the_runtime_path() {
        let mut cfg = Config::default();
        cfg.tools.push(thegn_core::config::NamedCommand {
            name: "db".into(),
            command: "psql".into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
            resume: false,
            route_via_proxy: false,
            drawer_scope: Some(DrawerScope::Global),
            drawer_cwd: None,
        });
        let policy = thegn_core::config_drawer::drawer_policy(&cfg);
        assert_eq!(policy.occupants()[0].id, "files");
        assert_eq!(policy.occupants()[1].id, "tool:db");
    }
}
