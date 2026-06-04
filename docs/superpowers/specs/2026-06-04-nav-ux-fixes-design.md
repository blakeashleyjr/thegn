# Navigation UX fixes — design

Date: 2026-06-04
Status: approved (user picked the recommended option for each issue)

## Problems

1. **Pane-focus keys unreliable / incomplete.** `Super Alt h/l` are not bound
   at all (only arrows + `j/k`), and on the user's machine Super+Alt chords may
   never reach zellij (WM grabs them — the config already warns about this).
   Headless repro confirmed the underlying actions are fine: after
   `new-worktree` the center terminal IS focused and `move-focus left/right`
   cycles sidebar ↔ terminal ↔ panel correctly. The failure is key delivery.
2. **Native `new-tab` breaks the model.** From the center terminal, a native
   zellij new-tab creates a bare, chrome-less `Tab #N` — no sidebar/tabbar/
   panel, not listed in the tabbar strip (it only lists `{slug}/…` tabs).
3. **`Alt+[ / Alt+]` is a silent no-op.** They map to zellij swap-layout
   cycling, but the superzej layouts define no `swap_tiled_layout` variants.

## Decisions (from user Q&A)

- "New tab" = **a second full-chrome tab on the SAME worktree** (a page of
  terminals), named `{slug}/{branch} ·2` (`·3`, …), listed in the tabbar.
- Trigger: **Alt+t**, plus repoint zellij tab-mode `n` so a bare tab can't be
  created by habit.
- `Alt+[ / ]` should **cycle arrangements of the center terminals only**,
  chrome stays pinned.
- Super+Alt may never fire on this machine → plain **Alt+h/j/k/l** become the
  primary focus keys.

## Design

### 1. Focus keybinds (config/zellij.kdl)

- `Alt h/j/k/l` → `MoveFocus Left/Down/Up/Right`. This *rebinds* zellij's
  default `MoveFocusOrTab` so worktree edges never spill into tab switching
  (same reasoning as the existing Super+Alt binds). Alt+arrows stay tab
  cycling: **arrows = tabs, letters = panes**.
- Add `Super Alt h` / `Super Alt l` for parity with the existing `j/k`.

### 2. Same-worktree tabs (`superzej new-tab`, Alt+t)

- **New layout `layouts/worktree-tab-extra.kdl`**: identical chrome wrapper
  (sidebar | tabbar/terminal | panel | statusbar) but the center pane is a
  plain default shell — NOT `pick-agent --in-place`, which would rename the
  tab and clobber the `·N` name.
- **New command `superzej new-tab`** (clap `NewTab`, `src/commands/new_tab.rs`):
  1. Resolve the current worktree from cwd (`repo::toplevel`); derive the base
     tab name: `{slug}/home` for the main checkout, else
     `repo::branch_tab(slug, branch)`.
  2. Scan `zellij::tab_names()` for `base` / `base ·N`; pick the lowest free
     N ≥ 2.
  3. `zellij new-tab --name "{base} ·N" --cwd <worktree> --layout
     worktree-tab-extra` (fallback: no layout, as new-worktree does).
- **Keybinds**: `Alt t` → `Run superzej new-tab` (floating, close_on_exit,
  like Alt+w); `keybinds.tab` `n` → same Run + `SwitchToMode "Normal"`.
- **Resolution**: `resolve-worktree` (panel feed) tries the exact tab name,
  then retries with a trailing ` ·N` stripped — so the diff/PR panel works on
  extra tabs. Tabbar lists the tab automatically (prefix match); the sidebar
  shows it as an extra branch row (acceptable).
- **Packaging**: ship the new layout in `install.sh`, `nix/package.nix`,
  `nix/hm-module.nix` alongside the other layout files.
- No DB row for extra tabs — they are pure zellij tabs; closing them never
  touches the worktree.

### 3. Swap layouts for the center column

Add to each tab layout file (`superzej.kdl`, `home-tab.kdl`,
`worktree-tab.kdl`, `worktree-tab-extra.kdl`) — they are deliberately
self-contained copies, keep in sync:

- `tab_template name="chrome"`: the existing wrapper with `children` in the
  center column slot (under the tabbar).
- Three `swap_tiled_layout` variants using that template, all constrained
  `min_panes=6` (chrome = 4 plugin panes + ≥2 center terminals):
  1. `vertical` — center terminals stacked top-to-bottom (base-equivalent;
     FIRST so any unexpected jump lands on the familiar arrangement),
  2. `horizontal` — side-by-side columns,
  3. `stacked` — zellij stacked panes.

With a single center terminal (5 panes) no variant matches, so the plugins'
`next_swap_layout()` restore primitive (sidebar/main.rs:299, panel
main.rs:247) keeps snapping to the base template exactly as today.

**Risk**: toggle restore (Ctrl+Alt+s/p) while ≥2 center terminals exist could
cycle into a variant instead of base. Mitigated by ordering `vertical` first
and zellij's dirty-reapply semantics; MUST be covered by a regression test,
and if it misbehaves the variants get tighter constraints.

## Testing (end to end, headless)

Python pty harness (a client must be attached for layouts/plugins): attach,
then assert via `zellij action list-clients` / `dump-layout`:

1. Alt+w → new worktree tab; center terminal focused (regression).
2. `Alt+h` / `Alt+l` (ESC-prefixed bytes to the pty) cycle sidebar ↔ terminal
   ↔ panel; edges do NOT switch tabs.
3. Alt+t → tab `{slug}/{branch} ·2` with full chrome (4 plugin panes in
   dump-layout), focused center shell at the worktree cwd; Alt+t again → `·3`.
4. Tab-mode `n` (Ctrl+t, n) → same result, never a bare tab.
5. `resolve-worktree --tab "{base} ·2"` prints the worktree path.
6. Alt+n split + Alt+] → arrangement changes (dump-layout differs), chrome
   panes still present; Alt+] cycles back around.
7. Toggle regression: Ctrl+Alt+s hide/show with 1 and with 2 center panes —
   chrome geometry restored, no variant hijack with 1 pane.
8. Existing suites still pass: `test/one-session.sh`, `test/smoke.sh`,
   `cargo build --release`, clippy/fmt.
