//! `superzej keys <action>` — inspect and regenerate keybindings.
//!
//! The registry (`src/keymap.rs`) is the single source of truth; this surface
//! exposes it to users, scripts, and (via `hints`) the statusbar plugin.

use crate::cli::KeysAction;
use crate::commands::attach;
use crate::config::Config;
use crate::keymap::{self, Collision, Context, Scope};
use crate::msg;
use anyhow::Result;

pub fn run(cfg: &Config, action: KeysAction) -> Result<()> {
    match action {
        KeysAction::List => list(cfg),
        KeysAction::Get { id } => get(cfg, &id),
        KeysAction::Show { json } => show(cfg, json),
        KeysAction::Validate => validate(cfg),
        KeysAction::Sync => sync(cfg),
        KeysAction::Hints { mode, context } => hints(cfg, &mode, &context),
    }
}

fn primary_chord(a: &keymap::Resolved) -> String {
    a.chords
        .first()
        .map(|c| c.to_kdl().to_string())
        .unwrap_or_default()
}

fn ctx_str(c: Context) -> &'static str {
    match c {
        Context::Always => "always",
        Context::WorktreeOnly => "worktree",
        Context::NonWorktree => "non-worktree",
    }
}

fn list(cfg: &Config) -> Result<()> {
    for a in keymap::effective(cfg) {
        let keys = a
            .chords
            .iter()
            .map(|c| c.to_kdl().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        crate::outln!(
            "{:<18} {:<22} {:<28} {}{}",
            a.id,
            keys,
            a.menu_label,
            ctx_str(a.context),
            if a.custom { "  (custom)" } else { "" }
        );
    }
    Ok(())
}

fn get(cfg: &Config, id: &str) -> Result<()> {
    match keymap::effective(cfg).iter().find(|a| a.id == id) {
        Some(a) => {
            crate::outln!("{}", primary_chord(a));
            Ok(())
        }
        None => anyhow::bail!("unknown action: {id}"),
    }
}

fn show(cfg: &Config, json: bool) -> Result<()> {
    let acts = keymap::effective(cfg);
    if json {
        let arr: Vec<serde_json::Value> = acts
            .iter()
            .map(|a| {
                serde_json::json!({
                    "id": a.id,
                    "chords": a.chords.iter().map(|c| c.to_kdl()).collect::<Vec<_>>(),
                    "label": a.menu_label,
                    "hint": a.hint,
                    "context": ctx_str(a.context),
                    "scope": if a.scope == Scope::Tab { "tab" } else { "shared" },
                    "menu": a.menu,
                    "custom": a.custom,
                })
            })
            .collect();
        crate::outln!("{}", serde_json::to_string_pretty(&arr)?);
    } else {
        list(cfg)?;
    }
    Ok(())
}

fn validate(cfg: &Config) -> Result<()> {
    let acts = keymap::effective(cfg);
    let cols = keymap::detect_collisions(&acts);
    if cols.is_empty() {
        crate::outln!("keybinds ok ({} actions)", acts.len());
        return Ok(());
    }
    for c in &cols {
        match c {
            Collision::Duplicate { chord, ids } => msg::error(&format!(
                "chord {chord:?} bound to multiple actions: {}",
                ids.join(", ")
            )),
            Collision::Reserved { chord, id } => msg::error(&format!(
                "action {id:?} uses reserved zellij chord {chord:?}"
            )),
        }
    }
    anyhow::bail!("{} keybind problem(s)", cols.len())
}

fn sync(cfg: &Config) -> Result<()> {
    if attach::sync_managed_config(cfg)? {
        msg::info("regenerated managed keybinds");
    } else {
        msg::info("keybinds already up to date");
    }
    Ok(())
}

/// Statusbar feed: `key<TAB>label` lines for the given mode/context. The plugin
/// caches these per (mode, context) and renders them as chips.
fn hints(cfg: &Config, _mode: &str, context: &str) -> Result<()> {
    let want = |c: Context| match context {
        "worktree" => c == Context::Always || c == Context::WorktreeOnly,
        "non-worktree" => c == Context::Always || c == Context::NonWorktree,
        _ => true,
    };
    let mut seen = std::collections::BTreeSet::new();
    for a in keymap::effective(cfg) {
        if a.scope != Scope::Shared || a.chords.is_empty() || !want(a.context) {
            continue;
        }
        // One chip per hint label (the first/primary chord).
        if !seen.insert(a.hint.clone()) {
            continue;
        }
        crate::outln!("{}\t{}", a.chords[0].to_hint(), a.hint);
    }
    Ok(())
}
