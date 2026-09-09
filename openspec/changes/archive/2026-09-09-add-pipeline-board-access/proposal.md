# Add direct doors to the pipeline board (action + sidebar row), and unstick the monitor keys behind them

> Builds on the in-flight `add-pipeline-board`, which landed the board itself.
> This change is only about **reaching** it — plus the four monitor/keymap
> defects a pilot hit while trying to, which live in the same files.

## Why

The board shipped as the monitor's **tenth** tab, and nothing else. That makes
it the one tab a user cannot reliably get to:

1. **The digit keys stop at nine.** `MonitorTab::visible` is indexed by `1`–`9`,
   so on a machine that shows every hardware family the board has no digit.
2. **The opening chord does not encode everywhere.** The monitor opens on
   `Ctrl Alt M`, which by the keymap's case grammar is Ctrl+Alt+**Shift**+m —
   a chord a legacy-encoding terminal cannot deliver (Ctrl+m is CR). A user in
   that terminal has no keyboard route to the monitor at all, board included.
3. **Nothing advertises that a pipeline is running.** With the board shut, the
   only evidence of a live roster is a per-worktree stage tag beside an activity
   dot — which says "this worktree is at `code`", never "three agents are up".

The pilot that found this also found four defects underneath, all in the files
this change already touches:

4. **`dispatched_at_ms` was written in seconds** (`util::now()` into an `_ms`
   column), so every board row rendered ~20671d old and the sidebar's
   blocked-since read 1970.
5. **`MonitorPrefs::last_tab` was never assigned** — persisted, read back at
   open, and written by nothing, so "reopen where you left off" always reopened
   on CPU.
6. **The monitor swallowed every chord it did not implement**, including the
   Alt/Super layer its own code comments hand to the compositor. So the chord
   that opens the monitor could never close it, and `Ctrl-g` (key lock) closed
   the monitor instead of locking anything.
7. **The monitor's palette keywords named only its first eight tabs**, so
   searching the palette for `containers`, `podman` or `pipeline` found nothing.

## What Changes

1. **Action `open-pipeline-board`** (`Alt b`, palette: "Pipeline board") opens
   the monitor directly on the board. Pressed again it closes; pressed while the
   monitor sits on another tab it jumps to the board.
2. **A sidebar Pipeline row** — `Pipeline ▸ 3 running`, plus a human-parked count
   in the attention tone — emitted only while the roster has live rows, at the
   tail of the tree (above `TERMINALS`). `↵`/click run the same action.
3. **A click door on the masthead stat chips.** A second click on a chip whose
   popup is already open expands into the monitor at that chip's tab (the same
   door `↵`/`M` opens from inside the popup); the first click still opens the
   popup. The `uptime` chip gains a tab mapping so it has the door too, and the
   click that opens a stat popup says in the statusbar where `↵` would land.
4. **The four defects above**, each with a test.

## Impact

- `tasks.md` group: the agent-pipeline line under `add-pipeline-board`.
- New action id ⇒ `keymap_specs`, `docs/help/system-monitor.md` claim + prose,
  and the action-family gates (`every_action_key_has_a_spec_and_round_trips`,
  `declared_default_chords_actually_dispatch`,
  `every_action_has_search_keywords`, `action_docs_ratchet`,
  `claimed_actions_are_mentioned_in_the_page_body`).
- New `RowKind` ⇒ the sidebar row-model, view, key and mouse matches.
- No new wake source, no new query: the roster rollup is a third derivation of
  the roster read the attention scan already performs off-loop.
