//! The contrast contract: one machine-checked table of (foreground role ×
//! background role × minimum WCAG-2.x ratio) over every token pair the chrome
//! composes, plus the pure [`audit`] that evaluates a *resolved* [`Palette`]
//! against it. A shipped preset that drops a pair below its floor fails the
//! sweep in [`crate::theme`]'s tests; the same finding shape drives the theme
//! builder's live per-token badges (THE-7).
//!
//! Substrate-free and I/O-free: it reuses [`crate::theme::contrast_ratio`]
//! (WCAG 2.x relative luminance) over the "R;G;B" fragments already in the
//! palette tables. Floors are adapted to terminal cells — cell-sized glyphs
//! sit near WCAG's "large text" boundary, so the metadata tiers use 3.0 rather
//! than 4.5, and the structural floor is a visibility bound (1.5) rather than
//! the non-text 3.0: a full-cell box-drawing rule is far heavier than a 1-px
//! web border, so the floor catches *collapse*, not web-AA.
//!
//! The contract binds only what thegn ships. User `[theme.colors]` /
//! `[theme.hues]` overrides are deliberately **not** gated here — warning on a
//! risky override is the builder's job (THE-7), not a config error.

use crate::theme::{Hue, Palette, contrast_ratio};

/// Which bar a palette is held to. The shipped default (prism) carries a
/// stricter `text` floor (AAA 7.0); every other preset uses the AA 4.5 floor.
/// Every *other* rule's floor is identical across presets — the point of THE-6
/// is that light mode meets the same bar as dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bar {
    /// The shipped default preset: `text` ≥ 7.0 (AAA) on every standard surface.
    Default,
    /// Every other shipped preset: `text` ≥ 4.5 (AA).
    Preset,
}

impl Bar {
    /// The `text`-tier floor for this bar.
    fn text_floor(self) -> f32 {
        match self {
            Bar::Default => 7.0,
            Bar::Preset => 4.5,
        }
    }
}

/// One failing token pair from [`audit`]: the rule that owns it, the two role
/// names, the measured ratio, and the floor it missed. Emitted in a
/// deterministic order (rule declaration order, then surface order) so the
/// sweep prints stable output and the builder can diff findings.
#[derive(Debug, Clone, PartialEq)]
pub struct ContrastFinding {
    /// The rule id, e.g. `"faint-on-surface"`.
    pub rule: &'static str,
    /// Foreground role name, e.g. `"faint"`, `"ghost2"`, `"amber"`.
    pub fg: &'static str,
    /// Background role name, e.g. `"panel2"`, `"accent"`, `"sel_accent"`.
    pub bg: &'static str,
    /// Measured WCAG 2.x contrast ratio.
    pub ratio: f32,
    /// The floor this pair failed to clear.
    pub min: f32,
}

/// Evaluate a resolved palette against the contrast contract. Returns every
/// failing pair (empty = pass). Pure: no I/O, no allocation beyond the result.
///
/// The palette must already be extended ([`crate::theme::extend_palette`]) —
/// the derived `ghost2`/`ghost3` and the `sel_accent()` tint are what chrome
/// actually draws, so derivation bugs are in scope, not just table literals.
pub fn audit(p: &Palette, bar: Bar) -> Vec<ContrastFinding> {
    let mut out = Vec::new();

    // Named surfaces, in a fixed order so findings are deterministic.
    let bg0 = ("bg0", p.bg0.as_str());
    let bg1 = ("bg1", p.bg1.as_str());
    let panel = ("panel", p.panel.as_str());
    // Half the sidebar's rows sit on the alternate block tint, so it carries
    // exactly the same text as `panel` and is gated exactly as hard.
    let panel_alt = ("panel_alt", p.panel_alt.as_str());
    let panel2 = ("panel2", p.panel2.as_str());
    let raise = ("raise", p.raise.as_str());

    // Readable text sits on any surface, including selection (panel2) and hover
    // (raise) — a row stays readable when selected or hovered.
    let all_surfaces = [bg0, bg1, panel, panel_alt, panel2, raise];
    // Recessive metadata and structural glyphs land on the base surfaces only;
    // panel2/raise re-tier the text riding a selection/hover.
    let metadata_surfaces = [bg0, bg1, panel, panel_alt];
    // UI affordances (focus frame, accent marks, activity dots) frame the tree
    // and bars, drawn on the two base backgrounds.
    let base_surfaces = [bg0, bg1];
    // Hue-as-text also has to survive a selected row, so it includes panel2.
    let hue_surfaces = [bg0, bg1, panel, panel_alt, panel2];

    // text: body copy — AA (AAA on the default).
    check_set(
        &mut out,
        "text-on-surface",
        ("text", &p.text),
        &all_surfaces,
        bar.text_floor(),
    );
    // dim: secondary copy — AA.
    check_set(
        &mut out,
        "dim-on-surface",
        ("dim", &p.dim),
        &all_surfaces,
        4.5,
    );
    // faint: muted labels — AA-large.
    check_set(
        &mut out,
        "faint-on-surface",
        ("faint", &p.faint),
        &all_surfaces,
        3.0,
    );

    // ghost: faintest *readable* tier (timestamps, counts, key hints).
    check_set(
        &mut out,
        "ghost-on-surface",
        ("ghost", &p.ghost),
        &metadata_surfaces,
        3.0,
    );

    // Structural floor: rules/fills/tracks/scaffolding must not vanish.
    for (fg_name, fg) in [
        ("ghost2", &p.ghost2),
        ("ghost3", &p.ghost3),
        ("border", &p.border),
    ] {
        check_set(
            &mut out,
            "structural-floor",
            (fg_name, fg),
            &metadata_surfaces,
            1.5,
        );
    }

    // Filled chips: `chip_fg` on the accent and every semantic hue.
    check(
        &mut out,
        "chip-on-fill",
        ("chip_fg", &p.chip_fg),
        ("accent", &p.accent),
        3.5,
    );
    for h in Hue::ALL {
        check(
            &mut out,
            "chip-on-fill",
            ("chip_fg", &p.chip_fg),
            (hue_name(h), p.hue(h)),
            3.5,
        );
    }

    // Hues drawn as status/identity *text* (diff ±, CI states, agent names),
    // on the base surfaces and a selected row (panel2).
    for h in Hue::ALL {
        check_set(
            &mut out,
            "hue-as-text",
            (hue_name(h), p.hue(h)),
            &hue_surfaces,
            3.0,
        );
    }

    // Focus/accent affordances on the two base backgrounds.
    check_set(
        &mut out,
        "affordance",
        ("focus", &p.focus),
        &base_surfaces,
        3.0,
    );
    check_set(
        &mut out,
        "affordance",
        ("accent", &p.accent),
        &base_surfaces,
        3.0,
    );

    // Sidebar activity dots must be tellable apart from the tree background.
    for (fg_name, fg) in [
        ("activity_active", &p.activity_active),
        ("activity_waiting", &p.activity_waiting),
        ("activity_done", &p.activity_done),
    ] {
        check_set(&mut out, "activity-dot", (fg_name, fg), &base_surfaces, 3.0);
    }

    // Selected-row copy on the derived accent tint.
    let sel = p.sel_accent();
    check(
        &mut out,
        "text-on-selection",
        ("text", &p.text),
        ("sel_accent", &sel),
        4.5,
    );

    out
}

/// Push a finding if `fg` on `bg` clears less than `min`.
fn check(
    out: &mut Vec<ContrastFinding>,
    rule: &'static str,
    fg: (&'static str, &str),
    bg: (&'static str, &str),
    min: f32,
) {
    let ratio = contrast_ratio(fg.1, bg.1);
    if ratio < min {
        out.push(ContrastFinding {
            rule,
            fg: fg.0,
            bg: bg.0,
            ratio,
            min,
        });
    }
}

/// Check one foreground against a set of surfaces, in surface order.
fn check_set(
    out: &mut Vec<ContrastFinding>,
    rule: &'static str,
    fg: (&'static str, &str),
    surfaces: &[(&'static str, &str)],
    min: f32,
) {
    for &bg in surfaces {
        check(out, rule, fg, bg, min);
    }
}

/// The static role name for a hue (matches the serde lowercase form).
fn hue_name(h: Hue) -> &'static str {
    match h {
        Hue::Teal => "teal",
        Hue::Magenta => "magenta",
        Hue::Purple => "purple",
        Hue::Green => "green",
        Hue::Amber => "amber",
        Hue::Red => "red",
        Hue::Blue => "blue",
        Hue::Orange => "orange",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Hues, PRESETS, extend_palette, preset};

    /// Resolve a preset the way chrome does: table values + `extend_palette`.
    fn resolved(name: &str) -> Palette {
        let mut p = preset(name).unwrap();
        extend_palette(&mut p);
        p
    }

    /// The bar a preset is held to: the shipped default is stricter.
    fn bar_for(name: &str) -> Bar {
        if name == "prism" || name.is_empty() {
            Bar::Default
        } else {
            Bar::Preset
        }
    }

    /// A synthetic palette that passes the whole contract: near-white surfaces
    /// (raise a mid grey), black readable/structural tokens, white chip text on
    /// black fills. Used as the "clean" baseline the anchor tests perturb.
    fn passing_palette() -> Palette {
        let white = "255;255;255".to_string();
        let black = "0;0;0".to_string();
        let hues = Hues {
            teal: black.clone(),
            magenta: black.clone(),
            purple: black.clone(),
            green: black.clone(),
            amber: black.clone(),
            red: black.clone(),
            blue: black.clone(),
            orange: black.clone(),
        };
        Palette {
            bg0: white.clone(),
            bg1: white.clone(),
            panel: white.clone(),
            panel_alt: white.clone(),
            panel2: white.clone(),
            // raise mid-grey: black fg still clears every floor on it, and it
            // gives the anchor test a surface to isolate a single failure on.
            raise: "120;120;120".to_string(),
            border: black.clone(),
            focus: black.clone(),
            text: black.clone(),
            dim: black.clone(),
            faint: black.clone(),
            ghost: black.clone(),
            accent: black.clone(),
            ghost2: black.clone(),
            ghost3: black.clone(),
            shadow_bg: black.clone(),
            shadow_fg: black.clone(),
            chip_fg: white,
            activity_active: black.clone(),
            activity_waiting: black.clone(),
            activity_done: black,
            hues,
            heat: Default::default(),
        }
    }

    #[test]
    fn audit_of_a_clean_palette_is_empty() {
        assert!(audit(&passing_palette(), Bar::Preset).is_empty());
    }

    #[test]
    fn audit_reports_exactly_the_violated_pair() {
        // Perturb one token so exactly one pair drops below its floor:
        // `faint` = mid-grey (140) still clears 3.0 on the white surfaces but
        // collapses against the mid-grey `raise` — an isolated single finding.
        let mut p = passing_palette();
        p.faint = "140;140;140".to_string();
        let findings = audit(&p, Bar::Preset);
        assert_eq!(
            findings.len(),
            1,
            "expected exactly one finding: {findings:?}"
        );
        let f = &findings[0];
        assert_eq!(f.rule, "faint-on-surface");
        assert_eq!(f.fg, "faint");
        assert_eq!(f.bg, "raise");
        assert_eq!(f.min, 3.0);
        assert!(
            f.ratio < 3.0,
            "measured ratio {} should be below floor",
            f.ratio
        );
    }

    #[test]
    fn audit_reports_derived_tokens_not_just_table_values() {
        // Nothing in the table changed, but a `ghost` that only just clears the
        // structural floor drags the *derived* ghost2/ghost3 under it. The
        // audit runs on the extended palette, so it catches the derived pair.
        let mut p = preset("light").unwrap();
        // A ghost close to bg0 so blend-toward-bg0 derivations collapse.
        p.ghost = "225;227;235".to_string();
        p.ghost2 = String::new();
        p.ghost3 = String::new();
        extend_palette(&mut p);
        let findings = audit(&p, Bar::Preset);
        assert!(
            findings
                .iter()
                .any(|f| f.rule == "structural-floor" && (f.fg == "ghost2" || f.fg == "ghost3")),
            "derived ghost2/ghost3 collapse must be reported: {findings:?}"
        );
    }

    #[test]
    fn audit_is_deterministic() {
        let p = resolved("light");
        assert_eq!(audit(&p, Bar::Preset), audit(&p, Bar::Preset));
    }

    /// The whole point of THE-6: every shipped preset clears the contract, with
    /// the default held to its stricter `text` bar. A regressed value fails
    /// here, naming the preset, the pair, the ratio, and the floor.
    #[test]
    fn every_shipped_preset_satisfies_the_contrast_contract() {
        let mut failures = String::new();
        for name in PRESETS {
            let findings = audit(&resolved(name), bar_for(name));
            for f in &findings {
                failures.push_str(&format!(
                    "\n  {name}: [{}] {} on {} = {:.2}:1 (want >= {})",
                    f.rule, f.fg, f.bg, f.ratio, f.min
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "contrast contract violations:{failures}"
        );
    }
}
