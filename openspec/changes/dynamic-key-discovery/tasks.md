# Tasks

## 1. Slice 1: context keys follow focus in the panel's blank rows

- [x] 1.1 `panel/context_hints.rs`: focus-zone title, fit-to-rows, Layout-dupe skip, 12-tip cap
- [x] 1.2 `draw_panel` stacks the block above Layout keys in blank tail rows only
- [x] 1.3 `[panel] context_hints` (default on) with example, env-overlay pin, parse test
- [x] 1.4 `FrameModel.drawer_focused` mirrored from focus each frame
- [x] 1.5 Unit and draw tests; `docs/help/panel.md`

## 2. Slice 2: finer contexts

- [ ] 2.1 `KeyContext` enum from focus plus the overlay stack (wizard, merge queue, prompt)
- [ ] 2.2 `context_hints` takes `KeyContext`; open panel section's row keys in the panel block
- [ ] 2.3 Wizard and merge-queue key tables share one declaration with their footers
- [ ] 2.4 Drift test: every advertised key is dispatched in its context

## 3. Slice 3: progressive disclosure

- [ ] 3.1 State DB table for per-action use counts (schema bump), batched off-loop flush
- [ ] 3.2 Count dispatched actions by id only (no key payloads)
- [ ] 3.3 `[keys] hint_level` and `[keys] learn_after`, with ratchets
- [ ] 3.4 Learned hints sort last, then leave the panel blocks; reset on rebind
- [ ] 3.5 `reset-key-hints` palette action and `thegn keys reset-hints`

## 4. Slice 4: which-key overlay

- [ ] 4.1 `which-key` action, `?` in non-PTY zones, optional `[keys] leader`
- [ ] 4.2 Overlay reuses the prefix popup layer with context-filtered `cheatsheet_groups`
- [ ] 4.3 Type-to-filter and Enter-to-run
- [ ] 4.4 Help page and keybindings spec update

## 5. Slice 5: fill with "try next"

- [ ] 5.1 Unlearned high-priority actions from other contexts under a dim TRY sub-title when space remains
