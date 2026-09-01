# Design — theme builder overlay

## Event loop, rendering, damage channel

The builder is a **boxed layer** (`layer::open_layer` + `LayerSpec`, the
onboarding-wizard shape), so while open it rides the `Overlays.layers` bit and
every frame is `Full` — correct, since a live palette swap recomposes all
chrome anyway. Preview applies through the existing
`chrome::set_palette(current_config.palette_with_preset(name))` chokepoint
(pre-resolved termwiz colors, quantized once in `wire.rs::color_spec` — the
builder introduces **zero** color literals at draw sites; its swatches render
palette _roles_, which is also what makes the preview honest under 256/16-color
degradation).

Wake paths: all interaction is key/mouse input (already on the loop). Two
pieces of I/O run **off-loop with waker delivery**:

- the themes-dir scan (startup + config-watch events → background read →
  channel + `TerminalWaker` pulse), like every other off-thread producer;
- file import (read + parse handed back as a parsed `UserTheme` or error).

Config writes (persist preset, save theme, import) are small user-invoked
writes following the existing `cmd/theme.rs` / onboarding `leave_writes`
precedent: `toml_edit` read-modify-write of the live config file, best-effort
with the failure surfaced in `model.status` (never a silent `let _ =` on the
primary path of a user action).

## Preview and revert semantics

Same contract the onboarding wizard already implements: preview applies to the
**runtime palette only**; nothing is written until the user confirms.

- Cursor motion over presets/tokens → `set_palette` with the candidate.
- Esc → re-apply `current_config.palette()` (the saved theme) and close.
- Enter/save → persist, then the config fs-watch reload re-derives the same
  palette (idempotent).
- Config-watch interplay: if config.toml changes on disk while the builder is
  open, the reload must not clobber the live preview — the builder re-applies
  its candidate on top after the reload (preview wins until closed), and the
  eventual save is a fresh `toml_edit` read-modify-write of the _current_ disk
  state, so an external edit to an unrelated key is never lost.

The in-popup preview strip exists because the popup itself covers part of the
screen: it renders one row per concern (text/dim/faint/ghost on each surface,
the eight hues, a filled chip, a `sel_accent()` selection row, diff ±, the
three activity dots) so a candidate can be judged without closing anything.
Every swatch is a palette role — no literals (the color-literal ratchet
applies).

## Token editing and contrast badges

The editor lists every `ThemeColors` + `ThemeHues` slot (the config's own
override vocabulary — one schema, no second token list to drift). Editing a
token builds a candidate `Config`-shaped override set, re-resolves via
`palette_with_preset` + `extend_palette`, applies, and runs
`theme_contrast::audit` (from `add-theme-contrast-contract`). Failing pairs
show inline as `⚠ faint/panel2 2.3 < 3.0` — **warn, never block**: the
contract binds shipped presets; users may do as they please with open eyes.
Hex parsing reuses the config's `parse_hex_rgb`; invalid input keeps the last
value (the malformed-hex-falls-back requirement already in the theming spec).

## User themes on disk

`$XDG_CONFIG_HOME/thegn/themes/<name>.toml`:

```toml
[meta]
name = "paperback"        # display name; file stem is the id
variant = "light"          # advisory
# origin = "gogh:Dracula"  # provenance when imported

[colors]                   # exactly the [theme.colors] key set
bg0 = "#f5f6fa"
# …
[hues]                     # exactly the [theme.hues] key set
teal = "#007a6d"
```

Decisions and why:

- **Files, not `[themes.<name>]` config tables.** A theme is a shareable
  artifact (N 182 is import/**share**); a file can be copied, mailed,
  or dropped in a dotfiles repo without touching config.toml. It also keeps
  config.toml's schema closed (the "Rust structs are the schema" requirement —
  a dynamic map of theme tables would weaken unknown-key linting).
- **Same key vocabulary as the override tables.** Import produces the
  `[colors]`/`[hues]` struct; the config overlay code path is reused, not
  duplicated. Saved themes are intentionally not exported to another format.
- **Resolution seam**: the **host** owns the directory read (core is
  substrate-free); parsed themes land in a `UserThemes` map handed to
  resolution. Pure core function
  `theme_user::to_palette(&UserTheme) -> Palette` (+ `extend_palette`), with
  `[theme.colors]`/`[theme.hues]` config overrides still applied on top —
  user themes behave exactly like presets under the existing override
  requirement.
- **Precedence**: built-in preset names win; a colliding user theme warns and
  is shadowed (predictability over cleverness). Unknown `preset` values fall
  back to the default exactly as today.
- **Live reload**: the themes dir is added to the existing config fs-watch
  registration; a changed theme file re-resolves the palette without restart
  (extending the "theme reloads live" requirement's WHEN to theme files).
- **Cycle/list**: `PRESETS` stays the built-in table; the cycle order and
  `thegn theme list` append user themes (list marks them `user`).

## Import: Gogh

Formats (verified against Gogh-Co/Gogh master):

- **Gogh**: flat YAML — `name`, `author`, `variant: dark|light`,
  `color_01`…`color_16` (ANSI 0–7 then bright 8–15), `background`,
  `foreground`, `cursor`. ~400 schemes, MIT.
  Gogh is the only v1 import format. Base16 support and export are deliberately
  deferred so this change stays focused on the requested local Gogh flow.

Parsing: a minimal line-oriented `key: value` subset parser in
`thegn-core/src/theme_import.rs` — **no serde_yaml dependency**. A full YAML
engine is unjustified weight for the deps-audit gate and the substrate-free
core. The parser is pure, panic-free, and exhaustively unit-tested (including
hostile input — see Security).

Mapping (pure, unit-tested):

- **Gogh**: `background`→bg0; bg1/panel/panel2/raise derived by blending
  background toward foreground in fixed steps (the same
  relative-to-own-surfaces philosophy as `extend_palette`, so light schemes
  stay light). `variant` is a checked contract: `light` requires background to
  be lighter than foreground and `dark` requires the inverse; contradictory
  documents are rejected rather than silently violating the declared variant.
  `foreground`→text;
  dim/faint/ghost blended between fg and bg; hues from color_02..07 (or the
  bright row when the normal row is too close to the background); accent =
  the highest-contrast non-grey hue; focus = accent.
- After mapping: `extend_palette`, then `theme_contrast::audit` — findings are
  presented as warnings with the import result, tying THE-6 and THE-7
  together: an imported washed-out light scheme announces exactly which pairs
  are below floor.

Import surfaces: `thegn theme import <file> [--name <n>]` (writes the
user-theme file, prints the audit summary; `--json` follows the CLI
machine-readable-list convention where applicable) and the overlay's import
action (prompts for a path via the inline input; no file browser in v1).

## Persistence fix

`cmd/theme.rs::set` writes `theme_table["name"]` but `ThemeConfig` reads
`preset` — the write has never taken effect. The fix routes every persist
(builder Enter, `thegn theme set <name>`) through one helper that writes
`[theme] preset` via `toml_edit`. `thegn theme set` drops its hard fzf/gum
requirement: with an argument it is non-interactive; without, it prints a hint
to use the builder (fzf fallback kept only if trivially cheap).

## Actions, keymap, help

- `theme-builder-open` — palette row + `ActionSpec` (keywords: theme, colors,
  appearance), default chord `Ctrl+Alt+Shift+t` (sibling of the cycle chord;
  subject to the collision check in `default_keymap`). `CycleTheme` is kept.
- In-overlay keys are overlay-local (list nav, `/` filter, `e` edit token,
  `i` import, `s` save-as, Enter apply+persist, Esc revert) — not global
  actions, so no per-key ActionSpecs; they surface in the overlay's own footer
  kbd strip.
- Help: new `docs/help/theming.md` claims `theme-builder-open` (and takes over
  `cycle-theme` if currently claimed elsewhere — the ratchet will arbitrate),
  documents the overlay keys, user themes, and Gogh import. The overlay is
  a layer, not a zone/panel: F1 context remains the underlying zone; the page
  is reachable from the help index and the action's palette entry. Gates:
  action-recipe tests, help ratchet + prose ratchet
  (`just help-ratchet-update` never adds entries for new work).

## Security

- **Theme files are untrusted input** (imported from the internet, shared
  between users). Mitigations: a hard size cap on files read for import/scan
  (e.g. 256 KiB); the parser is panic-free on arbitrary bytes (fuzz-shaped
  unit tables: truncation, BOM, huge lines, non-UTF-8 rejected cleanly); every
  color value funnels through hex parsing to `u8` triples before it can reach
  the wire — a malicious theme **cannot inject escape sequences**, because
  palettes store numeric `R;G;B` fragments and chrome emits them through
  `wire.rs`, never raw strings from the file.
- **Path handling**: `--name`/save-as names are slugified to
  `[a-z0-9-_]`; the write target is always
  `$XDG_CONFIG_HOME/thegn/themes/<slug>.toml` — no traversal out of the
  themes dir. Import reads the file the user named (their authority), but
  never writes outside the themes dir and config.toml.
- **No network surface**: no in-app scheme browsing/fetching (explicit
  non-goal); import is local files only.
- **No credentials, no sandbox change, no new external door**: theme verbs
  are local config utilities; nothing is exposed over HTTP/gRPC/MCP/plugin,
  so no capability-catalog row and no scope policy — a future remote surface
  must add CATALOG rows first.
- **Blast radius of the write surface**: config.toml (one key via
  `toml_edit`, comment-preserving) and the themes dir. Worst case is a bad
  theme, which the malformed-hex fallback and the contrast warnings already
  contain; a corrupt theme file is skipped with a warning, never fatal to
  startup.

## Alternatives considered

- **Extend `thegn theme set` (fzf/gum) instead of an overlay.** Rejected:
  external-tool dependency, no live preview, no token editing, and it is the
  path that shipped the `theme.name` bug — the product is its own multiplexer
  and can render its own picker.
- **`[themes.<name>]` tables in config.toml.** Rejected above (schema
  openness, shareability).
- **base16 import/export.** Deferred: the issue's requested Gogh flow is
  complete without another parser or output format.
- **A network "browse Gogh" mode.** Rejected: a new network + trust surface
  for marginal value; the corpus is one `git clone`/download away.

## Open questions

- Mouse: layers already hit-test; v1 should at least support click-to-select
  and wheel scroll in the lists — confirm against `layer.rs` hit-test scope
  during implementation.
- Whether the builder should offer "fix it for me" (auto-nudge a failing
  token to the nearest passing value) — deferred; the audit API makes it
  possible later.
- Whether `thegn theme list` should print user themes' audit status (a
  `warn` column) — cheap, decide in review.
- The cursor color from Gogh (`cursor`) has no palette slot today — dropped
  on import (documented), unless a `cursor` token is added by a future
  change.
