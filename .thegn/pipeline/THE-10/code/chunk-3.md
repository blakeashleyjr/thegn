# Chunk 3 — documentation, help ratchets, and reviewed baselines

Commit subject (exact): `docs(the-10): update project vocabulary, help ratchets, and snapshots`

## Scope

Make user documentation agree with the landed config, CLI, UI, and keymap
contracts. Update only reviewed generated snapshots after Chunk 2; do not
hand-edit generated help pages.

## Exact files touched

- `README.md`
- `CHANGELOG.md`
- `docs/help/bars.md`
- `docs/help/cli.md`
- `docs/help/command-palette.md`
- `docs/help/configuration.md`
- `docs/help/daemon-and-sessions.md`
- `docs/help/getting-started.md`
- `docs/help/index.md`
- `docs/help/merge-queue.md`
- `docs/help/panel.md`
- `docs/help/pipeline-board.md`
- `docs/help/projects.md`
- `docs/help/release-channels.md`
- `docs/help/review-a-pr.md`
- `docs/help/sandboxing.md`
- `docs/help/search-replace.md`
- `docs/help/sidebar.md`
- `docs/help/terminal-and-panes.md`
- `docs/help/terminal-compatibility.md`
- `docs/help/workflows.md`
- `docs/help/workspaces-and-worktrees.md`
- `docs/help/best-practices.md`
- `test/help-ratchet.txt`
- `test/help-prose-ratchet.txt`
- `test/help-panel-prose-ratchet.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__100x30__linux.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__160x40__linux.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__200x50__linux.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__80x24__linux.txt`
- `test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__80x24__linux.txt`
- `test/muse/snapshots/glitch_hunt_panel_accordion__after/xterm__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_panel_accordion__after/xterm__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__after_tall_short/kitty__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__after_tall_short/kitty__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__after_tall_short/vt220__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__after_tall_short/vt220__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__after_tall_short/xterm__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__after_tall_short/xterm__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__after_wide_narrow/kitty__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__after_wide_narrow/kitty__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__after_wide_narrow/vt220__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__after_wide_narrow/vt220__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__after_wide_narrow/xterm__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__after_wide_narrow/xterm__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__before/kitty__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__before/kitty__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__before/vt220__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__before/vt220__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__before/xterm__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_rendering__before/xterm__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_resize__after_storm/xterm__100x30__linux.txt`
- `test/muse/snapshots/palette__theme_query/kitty__100x30__linux.txt`
- `test/muse/snapshots/panel_git__branches/xterm__100x30__linux.txt`
- `test/muse/snapshots/panel_git__branches/xterm__160x40__linux.txt`
- `test/muse/snapshots/panel_system__system/xterm__100x30__linux.txt`
- `test/muse/snapshots/panel_work__work/xterm__100x30__linux.txt`
- `test/muse/snapshots/responsive_breakpoints__layout/xterm__100x30__linux.txt`
- `test/muse/snapshots/responsive_breakpoints__layout/xterm__160x40__linux.txt`
- `test/muse/snapshots/responsive_breakpoints__layout/xterm__200x50__linux.txt`
- `test/muse/snapshots/responsive_breakpoints__layout/xterm__80x24__linux.txt`
- `test/muse/snapshots/sidebar__focused/xterm__100x30__linux.txt`
- `test/muse/snapshots/themes__abyss#styled/xterm__100x30__linux.txt`
- `test/muse/snapshots/themes__ember#styled/xterm__100x30__linux.txt`
- `test/muse/snapshots/themes__light#styled/xterm__100x30__linux.txt`
- `test/muse/snapshots/themes__storm#styled/xterm__100x30__linux.txt`

Do not touch these two nonmatching baselines unless an actual reviewed diff
requires it:

- `test/muse/snapshots/chrome_regions__chrome/xterm__40x12__linux.txt`
- `test/muse/snapshots/responsive_breakpoints__layout/xterm__40x12__linux.txt`

## Approach

1. Rewrite one-repo user-facing prose to project, and retitle
   `workspaces-and-worktrees.md` without renaming its stable id/file. Rewrite
   `docs/help/projects.md` to describe programs/multi-repo groups and explain
   that legacy `thegn project` is the compatibility alias. Keep tracker and
   build/container terms qualified and unchanged.
2. Document the four config aliases, env alias, deterministic precedence, and
   N = 3 release window. Document `thegn program`/`--program` and old command
   aliases. Keep generated keybindings/config-reference pages generated from
   their source inputs.
3. Update help frontmatter to canonical `*-project` action ids and prose to
   mention both canonical labels and legacy ids. Run the help ratchet updater
   only after coverage is real; delete stale debt but do not add to the frozen
   `test/help-context-ratchet.txt`.
4. Re-record/review only changed Linux baselines with the repository’s e2e
   tooling in the later implementation lane. The architect lane does not run
   e2e; a coder may update the listed files only after a reviewed generated
   diff and must leave the two 40x12 files alone unless they demonstrably
   change.

## Overlap and dependency

No file overlaps Chunk 1 or Chunk 2. This chunk is serially dependent on both:
the docs must describe their canonical names and the snapshots must reflect
Chunk 2’s rendered strings. There are no implementation dependencies after
this landing.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host help --no-fail-fast`
- `cargo nextest run -p thegn-host keymap --no-fail-fast`
- `cargo nextest run -p thegn-core config_example --no-fail-fast`
- Run the scoped help-ratchet test/filter used by the repository after the
  prose updates; do not run the full CI gate.

E2E is intentionally not run in this architect lane. If a later coder is
authorized to update baselines, it must use the documented isolated e2e
workflow and review the exact diff; no live state DB is permitted.

## Done criteria

- All 20 `docs/help/` files found by the audited workspace search, plus
  `best-practices.md`, use the correct qualified vocabulary; stable page ids
  and filenames remain intact.
- README, changelog, config/help prose and generated-page inputs document
  canonical project/program terms and all compatibility aliases.
- Help ratchets pass, `help-context-ratchet.txt` remains unchanged/empty, and
  generated pages are not hand-edited.
- Only reviewed matching snapshots change; the two 40x12 nonmatching files do
  not change without evidence.
- Scoped tests pass, and the coder commits exactly:
  `docs(the-10): update project vocabulary, help ratchets, and snapshots`.
