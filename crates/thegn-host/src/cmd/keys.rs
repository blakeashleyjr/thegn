//! `thegn keys` — inspect and check the resolved keybindings without launching
//! the compositor.
//!
//! `config/config.toml.example` has pointed users at `thegn keys list` /
//! `thegn keys validate` since the registry was written, and the startup
//! conflict banner (`run.rs`) tells them to run `tg keys validate` — but the
//! subcommand did not exist. This is it.
//!
//! Three views, all reading the same sources the running UI does, so what this
//! prints is what the UI will do:
//!
//! - `list` — every binding: the core registry (after `[keybinds]` overrides
//!   and `[[actions]]`), the host `ACTION_SPECS`, and the zone-local key tables
//!   that live outside both (today: the sidebar).
//! - `validate` — `detect_collisions` over the effective registry; exits
//!   non-zero when anything clashes, so it is usable in a pre-commit hook.
//! - `hints` — exactly the `(chord, label)` pairs a zone's hint strip renders,
//!   for checking that a key actually surfaces.

use anyhow::Result;
use clap::Subcommand;
use thegn_core::config::Config;
use thegn_core::keymap::{self, Collision};
use thegn_core::outln;

#[derive(Subcommand, Clone)]
pub enum Action {
    /// List the effective keybindings, grouped by where they apply.
    List {
        /// Only bindings for this zone (`global`, `center`, `sidebar`,
        /// `panel`, `masthead`, `statusbar`).
        #[arg(long)]
        zone: Option<String>,
        /// Emit machine-readable JSON instead of the text table.
        #[arg(long)]
        json: bool,
    },
    /// Check for chord collisions and reserved-chord overrides. Exits non-zero
    /// if any are found.
    Validate,
    /// Print the hint strip a zone shows, as the UI would render it.
    Hints {
        /// Which zone's hints to print (default: `sidebar`).
        #[arg(long, default_value = "sidebar")]
        zone: String,
    },
}

pub fn run(cfg: &Config, action: &Action) -> Result<()> {
    match action {
        Action::List { zone, json } => list(cfg, zone.as_deref(), *json),
        Action::Validate => validate(cfg),
        Action::Hints { zone } => hints(cfg, zone),
    }
}

/// One printable binding. A flattened [`crate::keymap_merge::Binding`]: the
/// table shows a single chord per row, with `—` for palette-only actions.
struct Row {
    chord: String,
    id: String,
    label: String,
    zone: String,
    source: &'static str,
}

/// Every binding, from the shared fold in [`crate::keymap_merge`] — the same
/// set the generated Keybindings help page renders, so the CLI and the in-app
/// page can never disagree.
fn collect(cfg: &Config) -> Vec<Row> {
    crate::keymap_merge::collect(cfg)
        .into_iter()
        .map(|b| Row {
            // Palette-only actions have no chord but are still bindable — list
            // them so users can see the id they'd put in `[keybinds]`.
            chord: b.chords.first().cloned().unwrap_or_else(|| "—".to_string()),
            id: b.id,
            label: b.label,
            zone: b.zone,
            source: b.source.as_str(),
        })
        .collect()
}

fn list(cfg: &Config, zone: Option<&str>, json: bool) -> Result<()> {
    let rows: Vec<Row> = collect(cfg)
        .into_iter()
        .filter(|r| zone.is_none_or(|z| r.zone == z))
        .collect();

    if json {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "chord": r.chord,
                    "id": r.id,
                    "label": r.label,
                    "zone": r.zone,
                    "source": r.source,
                })
            })
            .collect();
        outln!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    if rows.is_empty() {
        outln!("no bindings match");
        return Ok(());
    }
    let chord_w = rows
        .iter()
        .map(|r| r.chord.chars().count())
        .max()
        .unwrap_or(0);
    let id_w = rows.iter().map(|r| r.id.chars().count()).max().unwrap_or(0);
    let mut current = String::new();
    for r in &rows {
        if r.zone != current {
            outln!("");
            outln!("── {} ──", r.zone);
            current = r.zone.clone();
        }
        outln!(
            "  {:<chord_w$}  {:<id_w$}  {}  [{}]",
            r.chord,
            r.id,
            r.label,
            r.source
        );
    }
    outln!("");
    outln!("{} binding(s). Rebind by id in [keybinds].", rows.len());
    outln!("zone-table entries are handled by the focused zone and are not rebindable.");
    Ok(())
}

/// Warn about `Super`/`Cmd` chords, which parse and validate cleanly but can
/// never fire on macOS.
///
/// Terminal emulators there reserve Cmd for the application menu and do not
/// forward it as a modifier, so a user who binds `cmd-k` gets a chord that
/// parses, survives `keys validate`, shows up in `keys list`, and then silently
/// does nothing — the worst kind of "unbound", because every signal says it
/// worked. Nothing in thegn binds Super by default; this is purely about user
/// rebinds.
fn super_chords(actions: &[thegn_core::keymap::Resolved]) -> Vec<(String, String)> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    actions
        .iter()
        .flat_map(|a| {
            a.chords
                .iter()
                .filter(|c| c.to_kdl().contains("Super"))
                .map(move |c| (c.to_kdl().to_string(), a.id.clone()))
        })
        .collect()
}

fn validate(cfg: &Config) -> Result<()> {
    let actions = keymap::effective(cfg);
    for (chord, id) in super_chords(&actions) {
        outln!(
            "! {chord} ({id}) uses Super/Cmd, which macOS terminals do not \
             forward — this binding will never fire"
        );
    }
    let collisions = keymap::detect_collisions(&actions);
    if collisions.is_empty() {
        outln!("✓ no keybind conflicts");
        return Ok(());
    }
    for c in &collisions {
        match c {
            Collision::Duplicate { chord, ids } => {
                outln!(
                    "✗ {chord} is bound to {} actions: {}",
                    ids.len(),
                    ids.join(", ")
                );
            }
            Collision::Reserved { chord, id } => {
                outln!("✗ {id} is bound to {chord}, which is reserved for the terminal");
            }
        }
    }
    outln!("");
    outln!("{} conflict(s) — fix them in [keybinds].", collisions.len());
    // Non-zero exit so this is usable as a check in a hook or CI.
    anyhow::bail!("keybind validation failed");
}

fn hints(cfg: &Config, zone: &str) -> Result<()> {
    let pairs = match zone {
        "sidebar" => crate::sidebar_keytable::footer_hints(cfg),
        "splash" => crate::logotype::splash_hints(cfg)
            .into_iter()
            .map(|h| (h.chord, h.label.to_string()))
            .collect(),
        other => anyhow::bail!("unknown hint zone {other:?}; expected `sidebar` or `splash`"),
    };
    if pairs.is_empty() {
        outln!("(no hints)");
        return Ok(());
    }
    let w = pairs
        .iter()
        .map(|(c, _)| c.chars().count())
        .max()
        .unwrap_or(0);
    for (chord, label) in &pairs {
        outln!("  {chord:<w$}  {label}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default config is conflict-free — the invariant the startup banner
    /// and this command both check.
    #[test]
    fn validate_passes_on_defaults() {
        assert!(validate(&Config::default()).is_ok());
    }

    /// A forced duplicate fails, and fails loudly (non-zero exit).
    #[test]
    fn validate_fails_on_a_duplicate() {
        let mut cfg = Config::default();
        // Put two actions on one chord.
        cfg.keybinds.insert("toggle-sidebar".into(), "Alt w".into());
        let err = validate(&cfg).expect_err("duplicate must fail validation");
        assert!(err.to_string().contains("validation failed"), "{err}");
    }

    /// `list` covers all three sources, so a key that isn't in the registry
    /// (the sidebar's) is still discoverable from the CLI.
    #[test]
    fn list_includes_registry_and_zone_tables() {
        let rows = collect(&Config::default());
        assert!(
            rows.iter().any(|r| r.source == "registry"),
            "registry rows present"
        );
        assert!(
            rows.iter()
                .any(|r| r.source == "zone-table" && r.zone == "sidebar"),
            "sidebar zone-table rows present"
        );
        // The keys this audit surfaced are listed.
        for chord in ["e", "g", "i"] {
            assert!(
                rows.iter().any(|r| r.chord == chord && r.zone == "sidebar"),
                "`{chord}` listed for the sidebar"
            );
        }
    }

    #[test]
    fn list_rows_follow_rebinds() {
        let mut cfg = Config::default();
        cfg.keybinds
            .insert("toggle-sidebar".into(), "Ctrl Alt d".into());
        let rows = collect(&cfg);
        let row = rows
            .iter()
            .find(|r| r.id == "toggle-sidebar")
            .expect("toggle-sidebar listed");
        assert_eq!(row.chord, "Ctrl-Alt-d");
    }

    #[test]
    fn hints_rejects_an_unknown_zone() {
        assert!(hints(&Config::default(), "nope").is_err());
    }

    #[test]
    fn super_chords_are_flagged_on_macos_only() {
        // Nothing binds Super by default, so a clean config must stay silent —
        // otherwise every macOS user sees a warning about a binding they never
        // made.
        let clean = keymap::effective(&Config::default());
        assert!(super_chords(&clean).is_empty());

        // A user rebind onto Cmd parses, validates, and lists — and then never
        // fires, because macOS terminals keep Cmd for themselves. That silent
        // dead-end is the whole reason this warning exists.
        let mut cfg = Config::default();
        cfg.keybinds.insert("palette".into(), "cmd k".into());
        let rebound = keymap::effective(&cfg);
        let found = super_chords(&rebound);
        if cfg!(target_os = "macos") {
            assert_eq!(found.len(), 1, "{found:?}");
            assert_eq!(found[0].1, "palette");
            assert!(found[0].0.contains("Super"), "{found:?}");
        } else {
            // Elsewhere Super is a perfectly ordinary modifier; warning about it
            // would be wrong.
            assert!(found.is_empty(), "{found:?}");
        }
    }
}
