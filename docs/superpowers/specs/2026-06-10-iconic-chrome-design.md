# Iconic-Minimal Chrome — Design

Date: 2026-06-10 · Status: approved (options confirmed via design Q&A)

Extends the visual/nav overhaul (`2026-06-10-visual-nav-overhaul-design.md`).
Goal: stop looking like "any TUI" — one designed masthead, a real logotype,
and an unmistakable boundary between chrome "furniture" and terminal
"windows". Ships together with the workspace-duplication fix (below) because
both touch the same sidebar/session model.

## Decisions (user-approved)

1. **Two-row masthead** replaces topbar + tabbar (same 2-row budget):
   half-block pixel wordmark left spanning both rows; stats cluster top-right;
   worktree/tab strip + the panel's DIFF/PR/CHECKS switcher on row 1.
2. **Empty-state splash**: large pixel wordmark + version + keybind hints when
   the center has no live pane; fresh launches start dormant (no PTY) until
   the first keypress/center click, dashboard-style.
3. **Tint + rounded cards**: chrome zones (masthead, sidebar, panel,
   statusbar) on the raised `panel` tint; terminal panes are rounded-corner
   cards (`╭╮╰╯`) on the darker `bg0` with `{program} · {worktree}` embedded
   in the top border.

## Logotype (`host/src/logotype.rs`)

Hand-rolled micro pixel font, no deps; only S U P E R Z J. Even pixel heights
so half-blocks (`▀▄█`) never leave a ragged row:

- **Small** 3×4 px → 2 rows; `measure(Small, "SUPERZEJ") == (31, 2)`.
- **Large** 5×6 px → 3 rows (1-px corner cuts); `== (47, 3)`.

`draw()` maps each (top, bottom) pixel pair to `█ ▀ ▄ ␣` with explicit fg AND
bg (termwiz has no transparency — callers pass the surface fill). One text run
per row, drawn in the normal damage-tracked pass. No timers, no animation.

## Masthead (`host/src/chrome.rs` + `layout.rs`)

- `MASTHEAD_ROWS = 2`; `ChromeLayout.masthead` replaces `topbar`/`tabbar`.
  **Center geometry is unchanged** (y=2, rows−3) → the upgrade resizes no PTY.
  Sidebar/panel start at y=2 (each lost the old shared title row).
- Brand breakpoints: ≥96 cols → wordmark (34-col slot); 72–95 → compact
  `superzej` text; <72 → none. `tab_strip(brand_cols)` clips the nav row
  between brand and panel edge.
- Nav row: `WASHU ▸ home ⟨1⟩ ⟨2⟩` — slug prefix uppercased (Faint), leaf in
  accent, chips in `⟨⟩` (active = focus color on a `blend_over(focus, panel,
0.16)` pill); pin chips right-aligned; DIFF/PR/CHECKS over the panel columns
  (segment x-math unchanged → `panel_tab_hit` keeps working, new y anchor).
- Draw and hit-test share one span builder (`strip_chip_spans`) — they can't
  drift.
- Stats: `·` separators (Ghost) instead of `│`; cpu/gpu amber ≥80% / red
  ≥92%, mem amber ≥85% / red ≥95%; date/clock Dim; `[bars]`/`[stats]` config
  vocabulary unchanged.

## Tinted zones

Masthead, statusbar, sidebar, panel fill `S::Panel`; center stays `bg0` — the
tint boundary IS the seam (no row spent on a rule). Section headers read as
quiet labels: `WORKSPACES` (Faint), `CHANGES` / `PULL REQUEST` / `CHECKS` in
the panel (the switcher itself lives on the masthead). Selection language on
the tint: selected `Panel2`, marked `Raise`, active rows get the focus pill
via the new `theme::blend_over(hue, base, t)` (core; `blend` delegates with
BG0) so tints never punch dark holes in tinted surfaces.

## Cards (`host/src/borders.rs`)

`draw_card(rect, title, style)`: rounded ring + `{title}` overlaid at x+2,
`…`-truncated, skipped under 10 cols. `draw_pane_frames` takes a `FrameStyle`

- `title_of: Fn(PaneId) -> String`; focused ring/title in the focus color.
  Title is `{program} · {worktree-leaf}` from the spawn argv (`PtyPane::program`)
  — OSC-title capture is a flagged follow-up (vt100 `new_with_callbacks`).
  The confirm modal reuses the rounded card. Square `draw_box` is gone.

## Splash + dormant launch (`host/src/run.rs`, `hydrate.rs`)

- Render predicate (always on): in `render_tab`, when no visible leaf has a
  live emulator, `draw_splash(center)` replaces the old black hole; chrome
  still draws; overlays (palette/cheatsheet/confirm/drawer) stay above.
- `load_or_seed_session` returns `(Session, seeded)`; only a genuine first
  launch/fresh workspace sets `center_dormant`, which gates the eager
  materialize (no PTY forked while the splash shows — first frame gets
  cheaper). First Key or center click dismisses; `Wake`/`Resized` never do
  (hydration pulses the waker constantly). Hardware cursor hides while no
  focused pane rect exists. `SUPERZEJ_BENCH_FIRST_FRAME_EXIT` skips dormancy
  so `just bench` semantics are unchanged.
- Variants by center size: Large (≥51×11): 3-row wordmark + version + 3 hint
  lines; Small (≥35×6): 2-row wordmark + 1 hint line; Text (≥12×1):
  `superzej vX`; else bg only. Re-centered per frame from the rect.
- Flagged follow-up (not in this change): route sole-pane-exit to the splash
  instead of instant shell respawn.

## Workspace-duplication fix (shipped with this design)

Root cause: home groups were named `{raw basename}/home` (`WASHU/home`) while
the DB workspace list keys by the lowercase canonical slug (`washu`), and
`refresh_tab_model` append-merged the live list — every switch appended a
phantom workspace whose synthetic + live `home` rows both rendered.

- Home groups are now created as `{slug}/home` via `repo::repo_slug_with(db,
root)` (new: canonical slug from an existing DB handle); `Session::resurrect`
  renames legacy raw-basename Home groups (collision-safe, idempotent,
  active-tab preserved).
- `hydrate::merge_workspace_lists(db_backed, live)` is the single merge:
  DB entries authoritative (order kept), stale live fallbacks (empty
  `repo_path`) dropped and re-derived, slug-unique. Used by both
  `workspace_list(Some(db))` and `refresh_tab_model` (replace, never append).
- Model hydrations carry a generation tag; the intake drops stale arrivals
  (a pre-switch hydration landing post-switch can't resurrect the old list).
- Cosmetic: `model.worktree` is now the canonical slug (`washu/home`); the
  masthead displays the prefix uppercased, so the label still reads
  `WASHU ▸ home`.

## Render pipeline fix (termwiz repaint collapse)

Live testing exposed that `BufferedTerminal::flush` falls back to termwiz's
`repaint_all` on the first flush (seq==0) and whenever its cost heuristic
trips — and `repaint_all` collapses every line that merely ENDS with the same
trailing background into one `ClearToEndOfScreen`, discarding their content.
With the panel-tinted right column EVERY line ends in `panel` bg, so a full
repaint painted row 0 and erased the rest ("top bar only renders on one bar";
likely also the historical "layout corrupts over time" mystery). The host now
keeps its own `front` Surface, diffs `scratch` against it explicitly
(`diff_screens`), renders the change list straight on the `Terminal`, and on
geometry changes prepends an explicit `ClearScreen(bg0)`. Both surfaces'
change logs are trimmed each frame (they previously grew unbounded).

## Panel: rich diff, Files tab, drawer, sizing

- **ANSI-aware wrapped diff** (`host/src/ansi.rs` + `draw_diff_filediff`):
  the syntect-highlighted diff is parsed into colored spans (truecolor SGR,
  reset, defaults; everything else dropped), soft-wrapped to the panel width,
  and an add/remove line's tint extends across the full row. Raw escapes
  never reach cells.
- **Files tab** (`PanelTab::Files`, key `2`; tabs are now DIFF FILES PR
  CHECKS on keys 1–4 and the masthead switcher): `git ls-files --cached
--others --exclude-standard` flattened by `panel::build_file_tree` into an
  accordion tree (top-level dirs start collapsed; `visible_file_indices`
  hides collapsed subtrees). Rendering mirrors the sidebar (carets, indent,
  accent cursor bar). Keys: `↵` toggles a folder / opens a file in the bat
  pager drawer (`[tools] bat`, default `bat --paging=always`), `o` opens in
  the editor drawer, `O` hands the file to `xdg-open`, `y` opens the yazi
  drawer at the selection's directory. The tree is cached per worktree and
  rebuilt on switch.
- **Drawer**: `Ctrl Alt f` (the existing `files` builtin) toggles the yazi
  drawer; the spawn paths are unified in `spawn_yazi_drawer` /
  `spawn_command_drawer` (single drawer slot, kill-then-spawn).
- **Panel sizing**: `TogglePanel`/`FocusPanel` now operate on what the user
  sees — revealing an auto-hidden panel sets `panel_forced`, which overrides
  the 100-col threshold in `layout::compute_full` so the panel keeps its
  readable `PANEL_COLS` width on small screens (the width clamp still leaves
  the center ≥ 1 col, i.e. near-full-screen reading is allowed). Hiding
  clears the override. Both sidebar and panel remain individually hideable.

## Column framing + resize robustness

- **Framed columns** (`layout::compute_full` + `chrome::draw_columns_frame`):
  a full-width horizontal rule (`divider`, 1 row) caps the columns directly
  below the masthead so all three column tops align at one level; the layout
  reserves a 1-col separator on each side of the center (`sep_left`/
  `sep_right`), drawn as `│` on the `Panel` tint (sidebar/panel read as framed
  boxes, the center as the darker well between them) meeting the rule with `┬`
  junctions. A separator turns the focus color when its zone owns the keyboard.
  Center geometry shifts down one row (band starts below the divider) — a
  one-time change, not per-resize.
- **Tab container**: the nav-row chips sit in a recessed `Bg0` inset bar so
  they read as a grouped tab strip; the active chip keeps its focus pill.
- **Resize robustness**: every observed resize (the `Resized` event and the
  in-loop `get_screen_size` mismatch) forces `full_repaint = true`, because a
  resize scrambles the physical screen even when the geometry lands back on
  the previous size (a coalesced A→B→A drag leaves `chrome`/scratch dims
  unchanged, so the diff-vs-`front` would otherwise repaint nothing over the
  garbage). Verified by a pyte-backed PTY harness driving a multi-resize tour
  and asserting every chrome element's final cell position.

## Iteration 2 (live feedback)

- **Single-row masthead, regular-font logo**: the pixel wordmark left the
  masthead (it lives on only the empty-state splash now); the bar is one row —
  `◆ superzej v0.1.0` (glyph accent, name Text, version Faint) left, stats
  right. Stats restyled: natural-width values (no padded gaps), base color
  Dim instead of Faint (more presence), threshold colors unchanged.
- **Tab bars live under the divider, in their columns**: the worktree label +
  tab chips top the center column (`layout.center_tabs`, padded pill chips —
  active in focus-on-tint, container row on `bg0`); the DIFF/FILES/PR/CHECKS
  switcher tops the panel column (active segment = accent pill + underline on
  focus). All three column headers align on one row below the divider.
  `WORKSPACES` pops in the accent.
- **In-panel file preview** replaces the bat drawer: Files `↵` renders the
  file via `diff_highlight::highlight_file` (syntect, tested) through the
  shared `draw_ansi_document` renderer (path header + wrapped body); `j/k`
  scroll, `Esc` closes. No subprocess.
- **Auto-expanding panel**: `panel_expanded` (drilled diff or preview, via
  `PanelUi::drilled()` + a loop-top detector) widens the panel to ~2/3 of the
  window (≥ `PANEL_COLS`, clamped to keep the center alive — up to near-full
  width on small screens) and retracts on exit.

## Iteration 3 (live feedback)

- **Shift+J/K walks documents**: on the Diff and Files tabs, `J`/`K` jump to
  the next/previous file's document — opening the drilled view if it isn't up
  — with the cursor following (dirs skipped on Files). Backed by a drilled-
  document cache: every shown doc banks under `(kind, worktree, path)`, and a
  `spawn_blocking` preloader fetches the next two + previous one neighbors
  into it (results ride a channel + waker pulse; the cache clears when fresh
  hydration actually changes the panel data, not on every safety tick).
- **Editor out of the drawer**: `o` opens `$EDITOR` on the selection in a
  fresh center tab; `e` in a split pane. The one-shot command drawer is gone.
- **Promotion shortcuts**: `t`/`s` turn the current artifact into a center
  tab / split pane — the file's pager diff (`git -c color.ui=always diff
HEAD -- path | $PAGER`) on the Diff tab, its `bat` view on the Files tab —
  via `open_command_tab` (fresh tab pointed at the spawned pane) and
  `open_command_pane` (split beside the focused pane).
- **No vertical bars beside the terminal**: the `│` separators and `┬`
  junctions are gone; the 1-col gutters stay as clear `bg0` so the terminal
  well separates from the tinted columns by contrast alone. The horizontal
  divider remains a continuous rule.

## Iteration 4 (stability + feature batch, 25+ items)

- **Never crash on vanished worktrees**: `prune_vanished_group` + a stat
  guard before materialize (registry row deleted, land on home, status
  message); spawn errors report instead of exiting the loop. Registry
  hygiene: `db_worktree_list` deletes dead local rows on the hydration
  thread; resurrect adopts any `repo_root`-matching row with a live dir;
  synthetic home rows dedupe against registered `…/home` rows.
- **Workspace switches reap the outgoing panes** (all four switch paths) —
  ends the persisted-pane-id collisions that bled an editor pane into the
  next workspace. `mouse_sel` clears on worktree change.
- **Input correctness**: `focus::forwards_to_pane` gates unmatched keys and
  pastes (panel/sidebar typing no longer reaches the PTY); Esc and Ctrl+←
  fully reset the panel via `PanelUi::reset_on_leave` in the central leave
  detector; Ctrl+→ auto-reveals a hidden panel at normal width and restores
  on leave (explicit toggles pin it); Ctrl+Space normalizes the legacy NUL
  encoding so the palette opens everywhere.
- **Scrolling**: every panel view clamps at its last line/row
  (`scroll_panel`); the mouse wheel scrolls the widget under it (panel /
  sidebar / pane) instead of the terminal behind; held-key repeats coalesce
  through `drain_key_repeats` + a pending-input queue (no more inertia).
- **Visuals**: focused pane ring is white (`S::Text`) while chrome owns the
  keyboard, focus-blue otherwise; WORKSPACES + the panel switcher are white
  and bold (`draw_text_bold`); the masthead degrades gracefully when narrow
  (drop date → GPU → brand, `fit_stats_cluster`); battery widget (charging
  icon, red ≤ `[stats] battery_warn`) left of the date; PR/forge widget
  (`owner/repo #N`, open green / draft amber / closed+merged purple) beside
  LOC; same-width GPU glyph; alacritty `opacity = 1.0` + `padding.y = 4`.
- **Themes**: named presets — `storm`, `light`, `abyss` (OLED + electric
  cyan), `ember` (charcoal + amber/coral), `aurora` (violet + mint) — via
  `[theme] preset`, cycled live with Ctrl+Alt+t; `[theme.colors]` overrides
  apply on top of any preset.
- **Panel**: bat-backed file previews (`--style=numbers,changes`, syntect
  fallback) with 256-color ANSI support in the span parser; hover preloading
  (diff + bat for the hovered file and the next, `doc_inflight`-deduped);
  syntect warmed at startup; SANDBOXES section in the bottom quarter
  (`panel_split`, podman/docker `ps` parsed in core, superzej-owned first);
  a TESTS tab (key 5) detecting just/cargo/go/pytest/jest/vitest, running on
  `r` off-thread, with ✓/✗/○ per-test indicators and a summary.
- **Panes**: Alt+e/Alt+g/Alt+/ tools open as center tabs (drawer retired for
  one-shots); Alt+p = zellij-style smart split (longer-dimension heuristic;
  FocusPanel moved to `Alt .`); Ctrl+Alt+z zooms the focused zone
  fullscreen (center pane / sidebar / panel) with a `⛶ ZOOM` badge,
  cleared by any zone change.
- **Drawer**: per-worktree keep-alive pool — toggling hides instead of
  killing (yazi position survives), the worktree-change path pre-warms the
  next drawer in the background, and workspace switches drain the pool.
- **Render pipeline**: pure layout changes no longer force a physical clear
  (the front-buffer diff repaints exactly the changed cells — the
  panel-switch flash is gone); pane content sits flush against the ring
  (`pane_padding` default 0).
- **btop fixed**: vt100 0.16 lacks HVP (`CSI r;c f`), btop's only
  positioning form — `rewrite_hvp` translates it to CUP before parsing
  (chunk-split safe). htop was unaffected because it uses CUP.
- **Fonts**: 20 Nerd Font monos installed (user nix profile); `just fonts`
  lists families, `just font name="…"` swaps the alacritty family in place —
  alacritty live-reloads, so switching is realtime.

## Testing

- Core (95% gate): `blend_over` endpoints/midpoint/delegation/malformed.
- Host: logotype exact half-block patterns + measure + clipping + variant
  thresholds + splash centering; card corners/truncation/title colors;
  masthead chips + hit-test round-trip, brand breakpoints, stats separators +
  threshold color (cell-attr assert), splash-when-no-live-panes; layout
  center-geometry regression + tiny-height clamps; session slug naming,
  legacy rename + active preservation, collision skip; merge idempotence /
  stale-fallback drop; `refresh_tab_model` no-dup regression; seeded flag.
- Perf invariants hold: no polling added; everything renders in the existing
  damage-tracked pass; full repaint only on geometry change; dormant launch
  forks no PTY until first input.
