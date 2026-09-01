# Chunk 3 — compositor launch and all user-facing handoff surfaces

Commit subject (exact): `feat(the-17): add IDE handoff UI surfaces`

## Files touched

- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/ide_handoff.rs` (new)
- `crates/thegn-host/src/panel_util.rs`
- `crates/thegn-host/src/actions.rs`
- `crates/thegn-host/src/chrome.rs`
- `crates/thegn-host/src/hydrate.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/keymap.rs`
- `crates/thegn-host/src/keymap_specs.rs`
- `crates/thegn-host/src/handlers/sidebar_keys.rs`
- `crates/thegn-host/src/diff_view.rs`
- `crates/thegn-host/src/pr_view.rs`
- `docs/help/terminal-and-panes.md`
- `docs/help/sidebar.md`
- `docs/help/git-and-diffs.md`
- `docs/help/review-a-pr.md`

## Approach

Add `ide_handoff.rs` as the only host launch seam. It accepts a core target,
resolves the focused/selected worktree’s effective config, and sends launch
work off the event loop. For external placement, convert the structured argv
to a `Command`, apply the worktree cwd, call
`thegn_core::sandbox_cpucap::wrap_background_argv` in the worker, and use
`actions::spawn_detached_reaped`. For pane placement, convert argv at the
host edge to the existing command-pane/tab path. Every result returns through
the normal channel and pulses the waker; failures become a status message.
There must be no new direct `Command::spawn`, blocking wait, `which`, shell
parse, or environment lookup in a loop action. Remove the relevant existing
direct/bypass paths in `run.rs` so search/Ctrl-O and the new surfaces all use
the helper.

Hydrate and drain `open_editor` intents alongside the existing focus/preset/
adopt intent carriers. Claim-and-delete remains off-loop and before the model
swap; launch remains an explicit user-visible action. The intent’s target is
revalidated before launch, and stale/malformed intents are dropped with a
diagnostic rather than opening an arbitrary path.

Add `Action::OpenInIde`, id `open-in-ide`, and an `ACTION_SPECS` row. Keep
`Action::Editor` unchanged as the terminal editor tool. Add the sidebar row
menu action for a selected worktree path and a palette handler for the focused
worktree. Add a diff-view action that uses `DiffFile.path` and, when selected,
`DiffLine.new_lineno`; never invent a new diff-line model.

This chunk is explicitly serial after THE-27. In the PR view, consume THE-27’s
`PrReviewSnapshot`/anchored thread projection and add only an `OpenInIde`
outcome using its existing path/new-line anchor. Do not add a comment cache,
thread parser, or duplicate target model. Update the PR footer/action text to
make the handoff discoverable.

Document `open-in-ide` in the existing terminal/panes help action list and
body, and mention row/diff/review entry points in their existing pages. Keep
all four help ratchets empty; do not claim the action in frontmatter without
prose. No new default chord is required—palette and contextual actions are
enough unless a genuinely free chord is ratified.

## Dependencies and overlap

Serial after chunks 1 and 2. It overlaps no files with chunk 1 and mostly no
files with chunk 2, but depends on chunk 2’s `ControlApi`/intent contract and
must be rebased after THE-27’s host changes. Within this chunk, `run.rs`,
`chrome.rs`, `hydrate.rs`, `pr_view.rs`, and `diff_view.rs` are one serial
integration unit; do not parallelize those edits. The new `ide_handoff.rs`
owns launch mechanics so `run.rs` remains a dispatcher, not a god file.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host ide_handoff`
- `cargo nextest run -p thegn-host keymap`
- `cargo nextest run -p thegn-host diff_view`
- `cargo nextest run -p thegn-host pr_view`
- `cargo nextest run -p thegn-host sidebar_keys`
- `cargo nextest run -p thegn-host help`

Run the focused help, action-spec, idle-poll, and thread-QoS ratchet tests
available in this checkout. If a host test invokes the binary, set
`XDG_STATE_HOME` to a fresh temporary directory; never migrate or use the
live state DB. Do not run e2e, `just test`, `just ci`, or a full workspace
compile.

## Done criteria

- Sidebar, diff/hunk, THE-27 PR-thread, palette, and control-intent paths all
  converge on one host handoff helper and one core target policy.
- External launches are detached, CPU-capped through the existing wrapper,
  reaped, and entirely off-loop; pane launches retain existing placement and
  degrade with a status message.
- `open-in-ide` has an action spec, palette registration, contextual labels,
  and written help; all help/action/keymap ratchets remain satisfied.
- Existing file-open bypasses touched by this feature are routed through the
  seam; no vendor CLI spelling leaks into host selection or control code.
- Dormant sidebar rows, missing/deleted lines, stale intents, unsupported
  provider operations, and failed spawns degrade visibly and safely.
- The coder commits exactly as:
  `feat(the-17): add IDE handoff UI surfaces`
