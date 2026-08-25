# Theme builder — a real overlay with live preview, token editing, and import

Linear: THE-7

## Why

Theme selection today is three disconnected fragments, none of them a real UI:

- `Ctrl+Alt+t` (`Action::CycleTheme`) blind-cycles 21 presets with no way to
  jump, compare, or persist — the statusbar literally says "set [theme] preset
  to keep".
- `thegn theme set` shells out to **fzf/gum** (external tools, no preview of
  anything but the name) and then writes `theme.name` — a key the config does
  not read (`ThemeConfig` deserializes `preset`), so the command has silently
  never worked.
- Per-token customization exists only as raw `[theme.colors]` /
  `[theme.hues]` TOML editing, blind, with no feedback on whether the result
  is legible.

Meanwhile every ingredient for a first-class builder already exists in-process:
boxed layers (`layer::open_layer`), a live palette swap
(`chrome::set_palette(cfg.palette_with_preset(name))`) proven by both
CycleTheme and the onboarding wizard's preview-with-revert, comment-preserving
config writes (`toml_edit` in `cmd/theme.rs`), and — once
`add-theme-contrast-contract` lands — a pure contrast audit to badge illegible
choices as the user makes them.

THE-7 links Gogh (Gogh-Co/Gogh): ~400 MIT-licensed terminal schemes as flat
YAML (`name`/`variant`/`color_01..16`/`background`/`foreground`/`cursor`).
Users should be able to bring those (and base16 schemes) into thegn's
token-palette world instead of hand-porting hex values.

## What Changes

- **A theme-builder overlay** (new action `theme-builder-open`), a boxed layer
  with three panes of state: a preset browser (built-in + user themes,
  filterable), a token editor (every `[theme.colors]`/`[theme.hues]` slot),
  and a preview strip rendering sample chrome rows (text tiers on each
  surface, hues, filled chips, a selection row, diff ±, activity dots) inside
  the popup. Selection live-applies via `chrome::set_palette` — the whole
  screen behind the overlay is the real preview — with Esc reverting to the
  saved theme (the onboarding-wizard semantics) and Enter persisting.
- **Per-token editing with contrast feedback**: pick a token, enter a hex
  value (inline input, `menu::InputOverlay` pattern) or pick a hue; the
  palette re-resolves and re-applies live, and the row shows any failing
  contract pairs (ratio + floor) from `theme_contrast::audit`. Warn, never
  block.
- **User themes on disk**: save the current palette as a named theme —
  `$XDG_CONFIG_HOME/thegn/themes/<name>.toml`, the same `[colors]`/`[hues]`
  shape as the config override tables plus minimal metadata. User themes join
  the preset namespace (`[theme] preset = "<name>"`, `thegn theme list`, the
  cycle, the builder); built-in names win collisions with a warning. Theme
  files ride the existing config fs-watch for live reload.
- **Import**: `thegn theme import <file> [--name <n>]` and an in-overlay
  import path accepting **Gogh YAML** and **base16 YAML** (classic flat and
  `palette:`-nested). A pure `thegn-core` mapper converts 16-color +
  bg/fg schemes into the token palette (surfaces blended from
  background toward foreground mirroring `extend_palette`, ANSI/base08–0F
  slots to the eight hues, `variant: light` flipping derivation), then runs
  the contrast audit and reports warnings. Export writes the user-theme TOML
  (`thegn theme export <name>`).
- **Fix the persist bug**: theme selection writes `[theme] preset` (not the
  dead `theme.name`), via `toml_edit` so user comments survive; `thegn theme
set` is repointed at the same write and its fzf/gum dependency dropped in
  favor of pointing at the builder (kept as a headless fallback:
  `thegn theme set <name>` non-interactive).

## Impact

- **Linear**: THE-7.
- **Roadmap**: group **N** — delivers **N 182** (Theme import/export/share)
  and turns **N 172/173**'s config-only story into a UI; adjacent to
  **M 170** (palette preview/themes) but deliberately not inside the command
  palette.
- **Depends on**: `add-theme-contrast-contract` (THE-6) for
  `theme_contrast::audit` — the builder's badges and import warnings consume
  it. The overlay itself could land without badges, but the two changes are
  scoped as a pair.
- **Specs**: `theming` — MODIFIED "Named presets with per-color overrides"
  (user themes join the preset namespace); ADDED requirements (builder
  overlay, token editing with feedback, user themes on disk, import/export,
  correct persistence).
- **Code**: `thegn-core/src/{theme_import.rs,theme_user.rs}` (pure parse/map +
  user-theme model; unit-tested, 95 % gate), `thegn-host/src/theme_builder.rs`
  (+ `handlers/theme_builder.rs`, a `run.rs` dispatch arm, `keymap.rs`
  action + `keymap_specs.rs` spec), `cmd/theme.rs` (import/export/save, set
  fix). Host reads the themes dir off the loop and hands parsed themes to core
  — core stays substrate-free.
- **Help/actions gates**: new action id(s) claimed by a new
  `docs/help/theming.md` page (help ratchet + prose ratchet); action recipe
  gates per `docs/extending/action.md`; no new zone or panel section — the
  overlay is a boxed layer, F1 context stays the underlying zone.
- **Config**: no new keys. `[theme] preset` documentation extended to user
  theme names in `config/config.toml.example`.
- **Capability catalog**: none. `thegn theme …` verbs are local config-file
  utilities (the same class as `thegn config`), not control-plane doors — no
  HTTP/gRPC/MCP/plugin surface is added. Exposing theme ops remotely later
  would require CATALOG rows first (stated in design).
- **DB**: none — themes are files; git/config stay the source of truth.
- **Render/event loop**: overlay is a boxed layer (`Overlays.layers` ⇒ Full
  frames while open); live preview re-applies the palette (full recompose per
  edit, input-rate-bounded). Themes-dir scan and file import run off-loop with
  waker delivery. Detailed in design.
- **e2e**: a new overlay means new muse baselines for its specs; existing
  baselines are unaffected (the default palette does not change). e2e is
  currently a local-only gate with stale baselines — record locally, note in
  tasks.
- **In-flight overlap**: none of the 42 in-flight changes touch theming.
  `add-profile-reordering`/profiles work may later want per-profile themes
  (N 174) — user themes on disk are the substrate that unblocks it, not a
  competitor.
- **Non-goals**: fetching schemes over the network (import is local files
  only — no new network surface); a full color-picker widget (v1 is hex entry
  - hue swatches); exporting _to_ Gogh/base16 formats; per-profile theme
    switching (N 174, blocked on profiles); theme plugins (N 204).
