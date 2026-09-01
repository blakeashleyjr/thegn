# THE-7 — theme builder popup and previews

## Decision

Turn the existing theme cycle into a boxed, modal `ThemeBuilder` overlay. The
builder owns only transient selection/edit state and a resolved candidate
palette. A host `ThemeStore` owns bounded filesystem work on a background
worker; `thegn-core` owns the user-theme file model, Gogh conversion, palette
resolution, and contrast audit. The render path remains a pure consumer of
palette roles.

The implementation is three serial chunks because the host overlay needs the
core contracts, and the CLI/help work needs both the core names and host store.
The chunks are file-disjoint; their dependency order is recorded in each
chunk.

## Current-code invariants verified

- The resolved palette is the central render input: `Palette` contains the
  surface, text, focus, accent, status, hue, and heat roles at
  `crates/thegn-core/src/theme.rs:149-193`; `Config::palette_with_preset` applies
  `[theme.colors]`/`[theme.hues]` and extends derived tokens at
  `crates/thegn-core/src/config.rs:6748-6797`. The builder must resolve through
  this seam, never maintain a second host palette.
- `extend_palette` derives light-safe structural tokens from the active
  surfaces and applies the visibility rule at `crates/thegn-core/src/theme.rs:262-334`.
  Every candidate, including an import, calls this before preview or audit.
- Built-in cycle order is the single `PRESETS` catalog at
  `crates/thegn-core/src/theme.rs:387-410`; the builder extends that catalog
  with user names rather than creating a second built-in list.
- A boxed popup must use `layer::open_layer`: its placement/hit-test rectangle
  is `layer::box_rect` and it covers the caret at
  `crates/thegn-host/src/layer.rs:295-331`. This automatically participates in
  the compositor's `layers`/Full rule at
  `crates/thegn-host/src/render_plan.rs:73-107` and
  `crates/thegn-host/src/run.rs:11796-11814`.
- Config reload already replaces the live palette at
  `crates/thegn-host/src/run.rs:10431-10504`, including
  `chrome::set_palette(new_cfg.palette())` at lines 10455-10459. A reload must
  then reapply the builder candidate, or a live preview will be silently
  clobbered.
- `Palette` has `focus` but no cursor field (`theme.rs:163-190`). Gogh's
  cursor therefore maps to `focus`, the existing active/caret affordance; no
  cursor config key or parallel slot is introduced.
- The existing contrast contract is already in this branch: the
  `theme_contrast` module is exported by `crates/thegn-core/src/lib.rs:271`, and
  the palette derivation comments identify its structural rule at
  `theme.rs:262-275`. The contrast openspec's implementation is an ancestor of
  this branch, so THE-7 reuses `theme_contrast::audit`; it does not add a second
  contrast subsystem. The remaining human visual/e2e work is not silently
  claimed here.
- Hex normalization already exists at
  `crates/thegn-core/src/config.rs:7023-7032`. The new file parser may share its
  semantics, but invalid input remains a warning/default rather than a startup
  failure.

These facts supersede stale parts of the draft openspec. In particular, the
draft's “small config writes on the loop” (`openspec/changes/add-theme-builder-overlay/design.md:15-26`)
violates the 0%-idle rule in `CLAUDE.md` and `docs/ARCHITECTURE.md:54-84`.
All THE-7 scans, imports, watcher work, and writes go through a background
provider seam. The draft's base16 and export paths are cut: the issue requires
Gogh-style import plus save/apply, and those extra formats/commands would add
parser, completion, help, and test surface without serving the requested flow.
The overlay/import/user-theme shape is the draft proposal at
`openspec/changes/add-theme-builder-overlay/proposal.md:35-66`, while the
Gogh/base16/export expansion is specified at
`openspec/changes/add-theme-builder-overlay/specs/theming/spec.md:103-137`;
only the Gogh portion survives this design. The contrast openspec's
implementation is already an ancestor; its remaining visual/e2e acceptance
work is not silently folded into THE-7.

## User-visible behavior

`Ctrl+Alt+Shift+t` opens a centered, dimmed popup. The left column is an ordered
catalog of built-ins followed by valid user themes; a built-in name wins a
same-named user file. The right column contains editable color tokens and a
preview strip. The strip renders representative sidebar row, tab, statusbar,
diff hunk, pane sample, selected row, activity dots, and structural text. It
must visibly exercise `bg0/bg1/panel`, `text/dim/faint/ghost`, `border/focus`,
`accent`/selection, semantic hues, and status roles.

Navigation and editing are a small explicit state machine in a new module:
Up/Down select a catalog item, Tab/Shift-Tab move editor focus, arrows move
within token values, Enter selects/edits, `#rgb`/`#rrggbb` accepts a value,
`Esc` cancels/reverts, and Enter on the action row applies. `Ctrl+S` opens
“Save as” with a slug/name field; `i` opens a local-path import field. A
bracketed paste is data for the active path/name field, never a shell command.
Mouse hit-testing uses the same `layer::box_rect` as painting; outside clicks
follow the existing modal policy and do not bypass unsaved edits.

Opening snapshots the effective config palette. Preview changes immediately
call `chrome::set_palette(candidate)` and mark chrome dirty. Cancel restores the
snapshot. Apply queues a single persisted update for `[theme].preset` and the
existing `[theme.colors]`/`[theme.hues]` overrides, then closes only after a
successful provider response; failures stay visible in the popup. Selecting a
user theme is still a named `[theme].preset`, not a new config key. Config
reloads update the base config, then resolve the active candidate over the new
base so an open preview survives reload and never clobbers a newer config.

## Core contracts and Gogh mapping

Add substrate-free `theme_user` and `theme_import` modules. `UserTheme` is a
closed, versioned TOML model, not a dynamic map: metadata/name, editable base
surface/text/status colors including `accent` and `focus`, and the eight hue
roles. Derived `ghost2/ghost3`, shadow, chip, activity, and heat values are
recomputed by `extend_palette`; this avoids the draft's inconsistent claim that
the user file has exactly the current `[theme.colors]` keys while omitting
focus/accent (`openspec/changes/add-theme-builder-overlay/design.md:62-103`).
The config schema remains unchanged. The only config documentation change is a
comment explaining that `preset` accepts a built-in or a local theme name under
the existing XDG config directory.

`theme_import` accepts a bounded local Gogh YAML or JSON object. It performs no
network access and exposes pure parse/convert functions plus unit tests. The
accepted Gogh fields are `name`, optional `variant`, `background`,
`foreground`, `cursor`, and `color_01` through `color_16`; malformed, oversized,
non-regular, or missing-color input produces a structured error. The host
chooses the path and performs the read; core receives bytes/text only.

The mapper keeps all sixteen ANSI inputs in an `Ansi16` value and applies one
constant role table. Neutral slots seed the surface/text ramp (`01/09` dark
anchors and `08/16` light anchors); normal/bright pairs seed the six chromatic
roles (`02/10` red, `03/11` green, `04/12` amber, `05/13` blue, `06/14`
purple, `07/15` teal). The selected member of each pair is the one with the
better contrast against the imported background. Orange and magenta are pure
blends of the resolved amber/red and purple/red representatives. Thus every
ANSI input is consumed deterministically, while light variants derive their
surfaces relative to imported background/foreground instead of assuming dark
terminal geometry. `foreground → text`, `background → bg0`, and
`cursor → focus`; accent is the highest-contrast chromatic representative and
selection continues to use `Palette::sel_accent`/`sel`.

Tests must assert the 16 ANSI values, fg/bg/cursor mappings, variant handling,
bad hex, missing fields, size limits, and that the resulting palette is
extended and audit-able. No `serde_yaml` dependency is needed: JSON uses the
existing JSON path and YAML is the deliberately narrow Gogh scalar/object
grammar, with strict field names and no anchors, tags, aliases, or code.

## Host seams and rendering

`ThemeStore` is a provider seam with two channels: catalog/import/save results
into the loop and a `TerminalWaker` pulse out of the worker. It scans only
`$XDG_CONFIG_HOME/thegn/themes`, watches that directory non-recursively with
debounce, ignores corrupt files with a status warning, caps file size, and
writes only validated slugs beneath that directory using an atomic temp-file
rename. CLI invocation may use the same provider synchronously; the TUI never
does filesystem I/O in the event loop. The existing config watcher remains the
config watcher; the theme directory needs its own bounded watcher rather than
an accidental recursive watch.

`ThemeBuilder` is pure state/reducer/render code. `handlers/theme_builder.rs`
owns event routing and drains store results. `run.rs` gets only thin wiring:
state, action dispatch, config-reload reapply, render call, and mouse/paste
routing. Do not grow `run.rs` with parsing, TOML, path, watcher, or palette
mapping logic. All preview draw calls use `seg::Tok::Slot`, `Hue`, `Heat`,
`SelAccent`, or `Sel`; no `Tok::Rgb` and no color literals. The preview is
composed at truecolor and naturally passes through `wire::color_spec`, the
single color-depth quantization chokepoint required by
`docs/ARCHITECTURE.md:86-99`. Glyphs use the existing capability-selected
glyphs, not new draw-site literals.

The overlay is painted at the existing modal layer order, after ordinary
workspace/media overlays and before help/which-key. Opening, editing, import
completion, cancel, and save completion mark chrome/full damage. Since the
popup is boxed and calls `open_layer`, `render_plan::Full` is derived by the
existing cover fact; explicit fast-path guards must also treat the builder as
modal so a pane-only path can never paint over it.

The popup displays a non-blocking contrast badge from
`thegn_core::theme_contrast::audit(candidate)`. It is warning-only: invalid
input is rejected at the editor boundary, but a low-contrast intentional theme
can be applied. The audit is not duplicated in host code.

## Actions, config, help, and ratchets

Keep `CycleTheme` (`Ctrl+Alt+t`) for one-step cycling, but make it cycle the
merged built-in/user catalog and remove the stale “set [theme] preset” status
instruction once `theme set <name>` is fixed. Add `ThemeBuilderOpen` through
the complete action path: enum, key name, parser, `ActionSpec`, default chord,
dispatch, help claim, and any key conflict tests. This follows the action
recipe at `docs/extending/action.md:1-25`.

`thegn theme set <name>` becomes deterministic and headless; `list` includes
valid local names; `import <file> [--name]` reads a local path and saves a
validated user theme. There is intentionally no export subcommand. Add every
new value-taking CLI argument to the one completion catalog at
`crates/thegn-core/src/completion/catalog.rs`; the completion-slot ratchet is
kept unchanged because catalog classification is the required path. No remote
capability is added, so `docs/api/control-v1.json` and its control-schema
snapshot remain unchanged; run their drift test to prove that.

Add the overlay keyboard/help page to the registered help sources in
`crates/thegn-host/src/help/pages.rs`, link it from the help index, and update
the theme/config documentation. The help ratchets remain empty/shrink-only;
the new page must make the action and all modal keys discoverable rather than
adding an allowlist debt. Update the existing `[theme]` comment in
`config/config.toml.example`; do not add a theme-directory config key. Run the
env-overlay ratchet unchanged because there are no new config keys. The
capability catalog, control schema, completion-slot, env-overlay, and help
ratchets are all explicitly checked in the chunks.

## Verification and deferred e2e snapshots

Run only the scoped commands in the chunk files: `just quick <crate>` and
filtered `cargo nextest run -p <crate> <filter>`. Do not run `just test`, `just
ci`, a full-workspace compile, or e2e. Add/list (but do not record) these e2e
snapshot cases for the eventual test pass: `theme-builder-open`,
`theme-builder-edit-preview`, `theme-builder-cancel-reverts`,
`theme-builder-gogh-import`, `theme-builder-save-reload`, and
`theme-builder-small-terminal`. Any manual `thegn` invocation must set
`XDG_STATE_HOME` to a fresh temporary directory; never migrate or exercise the
live state DB from this worktree.
