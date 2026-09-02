# Chunk 3 — git-view rendering, atomic UI behavior, merge reporting, and docs

Commit subject (exact): `feat(the-32): render submodule pointers and conflicts`

## Files touched

- `crates/thegn-host/src/panel/mod.rs`
- `crates/thegn-host/src/panel/sections/changes.rs`
- `crates/thegn-host/src/handlers/panel_changes.rs`
- `crates/thegn-host/src/gitmut.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/sidebar_view.rs`
- `crates/thegn-host/src/integrate.rs`
- `docs/help/git-and-diffs.md`
- `docs/help/sidebar.md`
- `docs/help/workspaces-and-worktrees.md`
- `docs/help/bars.md`
- `docs/help/merge-queue.md`
- `test/help-ratchet.txt`
- `test/help-prose-ratchet.txt`
- `test/help-panel-prose-ratchet.txt`
- `test/help-context-ratchet.txt`
- `test/glyph-literal-ratchet.txt`
- `test/ignored-result-ratchet.txt` (only if a new best-effort result is
  intentionally ignored and documented)

Chunk 1 and chunk 2 must be landed first. This chunk is file-disjoint from
them; it owns all host rendering/merge consumers and documentation. There is
no new action, capability, completion slot, or control-schema row, so do not
modify those snapshots unless a checker regenerates an unrelated formatting
line.

## Approach

1. Extend `ChangeRow` and `build_change_rows` to join typed submodule diffs.
   Render the pure core row policy in the changes section: caps-resolved
   submodule glyph, path, old→new abbreviated SHAs, and state/direction. Never
   render `+0/-0`. Add the bounded preview result path in the existing worker
   channel; no render code or loop handler invokes git.
2. Keep submodule pointers atomic in every staging entry point. Whole-file
   stage/unstage uses existing `StageFile`; line selection calls the core
   validator, clears/does not enqueue an invalid selection, and shows a concise
   status message. Add host fixture/model tests for pointer add/move/delete and
   unavailable/dirty previews.
3. Add the separate sidebar indicator and `[ui]` toggle to the pure display
   model. Resolve the new glyph through `caps::active_glyphs()` and verify the
   glyph-literal ratchet has no new draw-site debt. Preserve the existing
   `Full` chrome invalidation and render-plan behavior.
4. In `integrate.rs`, obtain typed gitlink conflict metadata from the service
   plumbing result, partition those paths before regenerable/custom-driver/
   rerere logic, and defer them. Use the core formatter in drain outcomes and
   handoff variables, including ours/theirs SHAs. Do not call driver merge,
   rerere, or blanket `git add -A` for a submodule conflict; ordinary text
   conflict behavior stays unchanged.
5. Document the two keys, trusted-layer rule, worktree/clone behavior,
   pointer rendering and atomic staging, bounded/no-fetch summaries, conflict
   policy, and disk-versus-LOC measurement boundary. Regenerate/update only
   the applicable help/env ratchets in this same commit. Explain that the
   completion/control snapshots are intentionally unchanged because no action
   or capability was introduced.

## Tests to run

Use fixture-only git config (`protocol.file.allow=always`,
`commit.gpgsign=false`) and a fresh temporary `XDG_STATE_HOME` for any DB or
CLI invocation.

- `just quick thegn-host`
- `cargo nextest run -p thegn-host changes`
- `cargo nextest run -p thegn-host sidebar`
- `cargo nextest run -p thegn-host integrate`
- `cargo nextest run -p thegn-host gitmut`
- `cargo nextest run -p thegn-host panel`
- `just quick thegn-core`
- `cargo nextest run -p thegn-core fold`

Do not run a full workspace gate, e2e, migration, or the built binary.

## Done criteria

- Changes rows and previews show readable old→new pointer state, bounded local
  summaries, and explicit degraded states without fetch or `+0/-0`.
- Submodule staging is whole-entry only and is covered at both core and host
  selection boundaries.
- Sidebar state is cached/degraded like the ordinary git fields, independently
  toggleable, caps-safe, and render-plan tests remain green.
- Merge/fold/PR-facing paths name pointer conflicts with both SHAs and never
  auto-resolve them through a driver or rerere.
- Example config, relevant help pages, env/help/glyph/ignored-result ratchets,
  and generated config reference inputs are synchronized in this commit.
- No new action, capability, completion slot, control-schema row, or vendor
  modification was introduced.
- `git diff --check` is clean.
- Commit exactly as `feat(the-32): render submodule pointers and conflicts`.
