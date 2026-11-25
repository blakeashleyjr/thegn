# Dynamic key discovery

Linear: THE-2704. Roadmap: public launch 2026-11-30.

## Why

Someone who doesn't know thegn's keys has to learn them from the `F1` help, the
Keys panel section (9), or the statusbar strip. The strip holds one line, and
it cuts whole bindings to fit. Meanwhile the right panel is mostly blank in
the usual states: a clean tree, few changes, no PRs. The draw tests show it.
One change fits within 18 rows of a 30-row panel, even with the 11-row Layout
keys block (THE-2703) below it, so a 50-row terminal leaves a dozen or more
rows unused. That space is always visible and it sits beside the work. It
should teach the keys for whatever the user is doing.

Today the hints are scattered. The statusbar strip follows focus. The panel's
NAVIGATE footer appears only while the panel has focus. The Layout keys block
is a fixed list. The sidebar has its own footer, and the which-key popup
appears only after a prefix key. Nothing adapts to what the user already
knows.

## What Changes

- **Context keys follow focus (slice 1, shipped with this change).** The
  panel's blank rows show the keys for the focused zone: PANE KEYS (terminal
  pane), DRAWER KEYS, BAR KEYS (masthead/statusbar), and the existing
  NAVIGATE while the panel has focus. The rows come from the same live,
  focus-aware list as the statusbar strip. The block grows into blank rows up
  to 12 tips and shrinks as content needs room. It skips chords the Layout
  keys block already shows. New key: `[panel] context_hints` (default `true`).
- **Finer contexts (later slices).** Hints for the open panel section, the
  merge queue, wizards and modal prompts, through one `KeyContext` value the
  focus code produces.
- **Progressive disclosure (later).** Count chord uses per action in the
  state DB. After `[keys] learn_after` uses (default 5), an action's hint
  drops down the list and then off it. A `reset-key-hints` palette action
  brings them all back. `[keys] hint_level = "beginner" | "learned" | "off"`
  sets the policy.
- **Which-key overlay (later).** `?` in a non-PTY zone, or a configurable
  leader, opens a grouped popup with every key for the current context. It
  reuses the existing post-prefix which-key layer, and it is also offered
  from the panel block's title row.
- **One source of truth.** The statusbar strip, the panel blocks, the Layout
  keys block, the which-key overlay and the cheatsheet all read the effective
  keymap. No draw site writes a chord literal.

## Impact

- Code: `crates/thegn-host/src/panel/context_hints.rs` (new),
  `chrome.rs::draw_panel`, `FrameModel` (`context_hints_off`,
  `drawer_focused`), `run.rs` focus mirror, `hydrate.rs`.
  `thegn-core` `PanelConfig.context_hints`.
- Docs: `docs/help/panel.md`, `config/config.toml.example`.
- Later slices touch `keyhint.rs`, `focus.rs`, the state DB (a schema bump
  for chord-use counts), and `docs/help/keybindings`.
