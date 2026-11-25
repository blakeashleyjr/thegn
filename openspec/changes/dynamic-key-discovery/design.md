# Design: dynamic key discovery

## Current surfaces (verified 2026-11-25)

| Surface                 | Source                                                                  | When                                       |
| ----------------------- | ----------------------------------------------------------------------- | ------------------------------------------ |
| Statusbar strip         | `keyhint::context_hints(focus, panel_ui, cfg)` → `FrameModel::keyhints` | always, one row, trimmed at whole bindings |
| Panel NAVIGATE footer   | `model.keyhints`, first 6                                               | panel focused and blank tail rows          |
| Panel Layout keys       | `panel::layout_hints::ROWS` via `FrameModel::chord_hints`               | `[panel] layout_hints`, blank tail rows    |
| Sidebar NAVIGATE footer | `sidebar_keytable::footer_hints`                                        | sidebar blank tail rows                    |
| Which-key popup         | `keyhint` popup after a pending prefix                                  | only mid-sequence                          |
| Cheatsheet              | `keyhint::cheatsheet_groups` (Keys section 9, Full view)                | on demand                                  |

Every one of these reads the effective keymap (`thegn_core::keymap::effective`,
`keymap::chord_hint_for_mode`) or a key table that the dispatch tests check
(`panel::section_keys`, `sidebar_keytable`). That is the single source of
truth this design keeps. No new surface may write a chord literal.

## 1. Context that follows focus and mode

`keyhint::context_hints` already maps the focus zone to a keymap `Context`.
It puts zone-local keys first (panel section keys, sidebar keys), then the
registry actions for that context in priority order, and returns only the
`lock` hint while keys are locked. The mode (vim normal/insert, emacs) changes
the chords through `chord_hint_for_mode`.

**Slice 1** reuses that list. The panel block's title says which zone it
describes (PANE / DRAWER / BAR KEYS, or NAVIGATE when the panel itself has
focus). The sidebar keeps its own footer, so the panel adds no sidebar block.
The corner overlay is transient, so it gets no block either.

**Later:** a `KeyContext` enum (`Pane`, `Sidebar`, `PanelSection(Section)`,
`Drawer`, `Bar`, `Wizard(kind)`, `MergeQueue`, `Prompt`) computed once per
frame from focus plus the overlay stack (wizard open, merge-queue view,
confirm prompt). `context_hints` takes it instead of `FocusState`, and each
`KeyContext` names a key table. Wizards and modal prompts get their keys from
their own footers' tables. The panel block then shows the open section's row
keys when the panel has focus (from `section_keys`, already drift-tested).

## 2. Using the empty panel space (adaptive)

Order from the bottom of the panel body: Lead footer (pinned, untouched),
then Layout keys, then context keys, then a one-row gap, then panel content.
Both blocks paint only rows that `build_panel` did not produce, so they never
cover content and need no hit-table entries.

- Layout keys keep priority (THE-2703 behaviour unchanged).
- Context keys take `free - layout - 1` rows: the title plus up to
  `MAX_TIPS` (12) tips in priority order, fewer when there is less room, and
  nothing below two rows.
- Content always wins. As the section list grows, the context block shrinks
  tip by tip, then the Layout block drops its lowest rows, then both go.
- Dedup: a context tip whose chord the Layout block shows is skipped. The
  comparison ignores case and `+`/`-`, so `Ctrl+x` matches `Ctrl-x`.

**Later:** when content is short and the zone's hints run out, the block can
add "next to learn" tips: the highest-priority actions that are not yet
learned (see §3) from other contexts, under a dim "TRY" sub-title.

## 3. Progressive disclosure

- **Tracking.** The dispatcher records one `(action_id, uses, last_used)`
  row per action in the state DB. Writes are batched: counts accumulate in
  memory and flush off the event loop on the existing debounced persist
  path, never on a keypress. Only action ids are stored, never key payloads
  (the `Keyboard diagnostics omit input` requirement still holds).
- **Fading.** An action counts as learned once `uses >= [keys] learn_after`
  (default 5). In `beginner` mode, learned hints sort after unlearned ones,
  then drop from the panel blocks; the statusbar strip still lists them
  until its space runs out. In `learned` mode, the panel blocks show only
  unlearned hints. `off` hides the panel blocks and keeps the statusbar
  strip.
- **Reset.** The palette action `reset-key-hints` (and `thegn keys
reset-hints`) clears the counts.
- Rebinding an action resets its count, because the user has a new chord to
  learn.

## 4. Which-key overlay

- `?` in non-PTY zones (sidebar, panel, drawer, bars) opens the overlay. In
  a terminal pane, `?` must reach the shell, so the overlay needs the
  `which-key` action (default `Ctrl-Alt-?`, rebindable) or a configured
  `[keys] leader`.
- The overlay renders `cheatsheet_groups` filtered to the current
  `KeyContext`, using the existing `open_layer` popup. Typing filters the
  list and `Enter` runs the highlighted action (palette semantics), so the
  overlay also works as a teaching launcher.
- The post-prefix popup that already exists stays as it is. The new overlay
  is the same component opened with no prefix.

## 5. Config

| Key                     | Default      | Slice              |
| ----------------------- | ------------ | ------------------ |
| `[panel] layout_hints`  | `true`       | shipped (THE-2703) |
| `[panel] context_hints` | `true`       | 1 (this change)    |
| `[keys] hint_level`     | `"beginner"` | 3                  |
| `[keys] learn_after`    | `5`          | 3                  |
| `[keys] leader`         | unset        | 4                  |

Each new shallow key needs `config/config.toml.example`, a
`test/env-overlay-ratchet.txt` pin (these are presentation preferences, not
CI knobs), and a defaults-and-parses test.

## 6. Narrow terminals and accessibility

- Panel blocks need at least 16 columns and two free rows. Below that they
  are not drawn, and chords and labels are cut with `…` rather than wrapped.
- Chords use the same `to_hint` rendering as the cheatsheet and `thegn keys
list`. Colour is never the only signal: titles are text, and dim styling
  only lowers emphasis.
- When the panel is hidden (narrow layout), the statusbar strip and the
  which-key overlay still cover discovery.

## Risks

- **Repaint cost.** The blocks read `model.keyhints`, which is rebuilt every
  frame. Focus changes already force a chrome frame (the focus ring moves),
  so the pane-output fast path does no new work.
- **Noise for experts.** That is why slice 3 adds fading and `off`. Until
  then, `[panel] context_hints = false` is the escape hatch.
