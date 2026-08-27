//! The runtime **launch menu** and **preset application**.
//!
//! Selecting a launch-menu row (a preset, an agent, a tool, or `shell`) — or a
//! `launch_preset` intent arriving from `open --preset` — launches into the
//! active worktree. Because resolving a launch spec opens the DB and settles the
//! worktree's sandbox (slow for a provider/remote env), and a preset resolves
//! *several* of them, the resolution runs **off the event loop**: a
//! `spawn_blocking` task composes the specs, sends a [`LaunchApply`] over a
//! channel, and pulses the `TerminalWaker`; the loop drain then spawns the panes
//! (fast — openpty + exec). This is the same off-loop shape the drawer/materialize
//! cold-spawn uses, so a slow sandbox resolve never freezes the UI.
//!
//! Each command resolves through the same launch-spec pipeline as the wizard's
//! picker (`agent::launch_spec_full`): agent/tool names resolve to their
//! configured command + provider/sandbox semantics; anything else runs raw via
//! the sandbox-wrapped login shell. A preset's `env`/`cwd` overlay is applied
//! last. A preset's panes deliberately do **not** claim the worktree's
//! remembered agent (`suppress_agent_record`) — a preset only launches panes.

use termwiz::terminal::TerminalWaker;

use crate::agent::{LaunchExtras, LaunchSpec, launch_spec_full};
use crate::center::CenterTree;
use crate::compositor::Rect;
use crate::panes::Panes;
use crate::session::Session;
use thegn_core::config::{Config, Preset, PresetCommand, PresetMode};
use thegn_core::config_presets::{preset_pane_cwd, resolve_env};

/// A resolved launch, delivered from the off-loop resolver to the loop drain.
pub(crate) struct LaunchApply {
    /// Worktree group to launch into (routing key: the group's unique name).
    pub group: String,
    /// Human label for the status line ("dev" / "claude").
    pub label: String,
    /// One new tab holding an even split of every spec (`true`), or one new tab
    /// per spec (`false`).
    pub split: bool,
    /// Pre-composed launch specs (sandbox-wrapped; env/cwd assembled off-loop).
    pub specs: Vec<LaunchSpec>,
    /// Fallback/degradation warnings surfaced during resolution.
    pub warnings: Vec<String>,
}

pub(crate) type LaunchTx = tokio::sync::mpsc::UnboundedSender<LaunchApply>;

/// What the launch menu / an intent asked to launch into the active worktree.
pub(crate) enum LaunchRequest {
    /// A single picker choice (an agent/tool name or `shell`) → one new tab.
    /// Records the remembered agent exactly like the wizard picker.
    Choice(String),
    /// A named preset → its `mode` decides split vs one-tab-per-command. Never
    /// records a remembered agent.
    Preset(Preset),
}

/// Whether a preset's `mode` opens an even split (one tab) rather than one tab
/// per command.
pub(crate) fn split_for_mode(mode: PresetMode) -> bool {
    matches!(mode, PresetMode::Split)
}

/// Resolve a launch request off the event loop and deliver it to the loop.
///
/// `cfg`/`group`/`worktree` are owned clones so the task is `'static`. On
/// completion it sends one [`LaunchApply`] and wakes the loop; a resolve error
/// (e.g. a sandbox halt) is surfaced as a warning-only apply with no specs.
pub(crate) fn spawn_resolve(
    tx: LaunchTx,
    waker: TerminalWaker,
    cfg: Config,
    group: String,
    worktree: String,
    req: LaunchRequest,
) {
    tokio::task::spawn_blocking(move || {
        let apply = resolve(&cfg, group, &worktree, req);
        // best-effort: the loop may be gone (shutdown); either send failing is
        // fine, but if the send lands we must wake so the drain runs.
        if tx.send(apply).is_ok() {
            let _ = waker.wake();
        }
    });
}

/// The pure-ish off-loop body: compose the launch specs for `req`.
fn resolve(cfg: &Config, group: String, worktree: &str, req: LaunchRequest) -> LaunchApply {
    match req {
        LaunchRequest::Choice(choice) => {
            let mut specs = Vec::new();
            let mut warnings = Vec::new();
            match compose_choice(cfg, worktree, &choice) {
                Ok(spec) => {
                    warnings.extend(spec.warnings.iter().cloned());
                    specs.push(spec);
                }
                Err(e) => warnings.push(format!("{choice}: {e}")),
            }
            LaunchApply {
                group,
                label: choice,
                split: false,
                specs,
                warnings,
            }
        }
        LaunchRequest::Preset(preset) => {
            let split = split_for_mode(preset.mode);
            let cmds = preset.resolved_commands(&cfg.agents, &cfg.tools);
            let mut specs = Vec::new();
            let mut warnings = Vec::new();
            for pc in &cmds {
                match compose_preset_command(cfg, worktree, &preset, pc) {
                    Ok(spec) => {
                        warnings.extend(spec.warnings.iter().cloned());
                        specs.push(spec);
                    }
                    Err(e) => warnings.push(format!("{}: {e}", preset.name)),
                }
            }
            LaunchApply {
                group,
                label: preset.name.clone(),
                split,
                specs,
                warnings,
            }
        }
    }
}

/// Compose one launch spec for a single picker choice (records the remembered
/// agent for agent choices, exactly like the wizard picker).
fn compose_choice(cfg: &Config, worktree: &str, choice: &str) -> anyhow::Result<LaunchSpec> {
    let daemon_persistent = crate::handlers::startup::daemon_active(cfg);
    launch_spec_full(
        cfg,
        worktree,
        None,
        choice,
        true,
        daemon_persistent,
        LaunchExtras::default(),
    )
}

/// Compose one launch spec for a preset command: resolve an agent/tool name to
/// its full launch semantics, or run a raw command via the login shell; then
/// overlay the preset's worktree-relative `cwd` and its `env` (applied last,
/// expanded through the secret indirection). Never records a remembered agent.
fn compose_preset_command(
    cfg: &Config,
    worktree: &str,
    preset: &Preset,
    pc: &PresetCommand,
) -> anyhow::Result<LaunchSpec> {
    let daemon_persistent = crate::handlers::startup::daemon_active(cfg);
    let (choice, over): (&str, Option<String>) = match pc {
        PresetCommand::Named(n) => (n.as_str(), None),
        PresetCommand::Shell => ("shell", None),
        PresetCommand::ShellCommand(c) => ("shell", Some(c.clone())),
    };
    let mut spec = launch_spec_full(
        cfg,
        worktree,
        None,
        choice,
        true,
        daemon_persistent,
        LaunchExtras {
            cmd_override: over.as_deref(),
            prompt: None,
            suppress_agent_record: true,
            stage: None,
        },
    )?;
    // Preset `cwd` is worktree-relative; overlay only a local (Some) cwd — a
    // remote/provider pane cd's on its target and keeps `cwd == None`.
    if let Some(base) = spec.cwd.clone() {
        spec.cwd = Some(preset_pane_cwd(&base, preset.cwd.as_deref()));
    }
    // Preset `env` overlaid last (last wins over the pane's base env).
    spec.env.extend(resolve_env(&preset.env));
    Ok(spec)
}

/// The name of the first preset command that resolves to a configured agent
/// (not a tool/shell/raw) — the remembered agent a template's preset claims
/// when the template sets no explicit `agent`.
pub(crate) fn first_agent_of_preset(cfg: &Config, preset: &Preset) -> Option<String> {
    preset
        .resolved_commands(&cfg.agents, &cfg.tools)
        .into_iter()
        .find_map(|pc| match pc {
            PresetCommand::Named(n) if cfg.agents.iter().any(|a| a.name == n) => Some(n),
            _ => None,
        })
}

/// Spawn a resolved launch into its worktree group (loop-side; openpty + exec
/// only — the slow work already ran off-loop). Returns the focused pane id, or
/// `None` if the group is gone or nothing spawned. `split` ⇒ one new tab with
/// an even split of every spec; else one new tab per spec.
pub(crate) fn apply_launch(
    apply: LaunchApply,
    session: &mut Session,
    panes: &mut Panes,
    center: Rect,
) -> Option<u32> {
    for w in &apply.warnings {
        thegn_core::msg::warn(w);
    }
    let gi = session
        .worktrees
        .iter()
        .position(|g| g.name == apply.group)?;
    if apply.specs.is_empty() {
        return None;
    }
    // Make the launched group the active one so the new tab is what the user
    // sees (relevant for an `open --preset` into a background workspace).
    session.active = gi;

    if apply.split && apply.specs.len() > 1 {
        let ti = session.worktrees[gi].add_tab();
        // Reuse the even-split tree builder with a spawn closure that pops the
        // pre-composed specs in order (the leaf command strings are placeholders).
        let placeholders = vec![String::new(); apply.specs.len()];
        let layout = crate::layout_spec::LayoutSpec::even_split(&placeholders);
        let built = {
            let mut it = apply.specs.into_iter();
            let mut spawn = |_cmd: Option<&str>| -> Option<u32> {
                let spec = it.next()?;
                panes
                    .spawn_argv_env(&spec.argv, spec.cwd.as_deref(), &spec.env, center)
                    .ok()
            };
            layout.apply(&mut spawn)
        };
        let (tree, focus) = built?;
        let g = &mut session.worktrees[gi];
        if let Some(tab) = g.tabs.get_mut(ti) {
            for old in tab.center.pane_ids() {
                panes.table.remove(&old);
            }
            tab.center = tree;
            tab.focused_pane = focus;
        }
        Some(focus)
    } else {
        // One new tab per spec (also the single-choice / single-command path).
        let mut last = None;
        for spec in apply.specs {
            let ti = session.worktrees[gi].add_tab();
            match panes.spawn_argv_env(&spec.argv, spec.cwd.as_deref(), &spec.env, center) {
                Ok(id) => {
                    let g = &mut session.worktrees[gi];
                    if let Some(tab) = g.tabs.get_mut(ti) {
                        for old in tab.center.pane_ids() {
                            panes.table.remove(&old);
                        }
                        tab.center = CenterTree::Leaf(id);
                        tab.focused_pane = id;
                    }
                    last = Some(id);
                }
                Err(e) => thegn_core::msg::warn(&format!("launch failed: {e}")),
            }
        }
        last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_mode_maps_to_split_flag() {
        assert!(split_for_mode(PresetMode::Split));
        assert!(!split_for_mode(PresetMode::Tabs));
    }

    #[test]
    fn first_agent_finds_first_agent_command_only() {
        let mut cfg = Config::default();
        cfg.agents.push(thegn_core::config::NamedCommand {
            name: "claude".into(),
            command: "claude".into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            resume: false,
            route_via_proxy: false,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
        });
        cfg.tools.push(thegn_core::config::NamedCommand {
            name: "lazygit".into(),
            command: "lazygit".into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            resume: false,
            route_via_proxy: false,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
        });
        // tool first, then agent → the agent is claimed, not the tool.
        let preset = Preset {
            name: "dev".into(),
            commands: vec!["lazygit".into(), "just dev".into(), "claude".into()],
            ..Default::default()
        };
        assert_eq!(first_agent_of_preset(&cfg, &preset), Some("claude".into()));
        // No agent command → None.
        let preset = Preset {
            name: "tools".into(),
            commands: vec!["lazygit".into(), "just dev".into()],
            ..Default::default()
        };
        assert_eq!(first_agent_of_preset(&cfg, &preset), None);
    }
}
