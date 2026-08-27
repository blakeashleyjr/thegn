# Design — add-pipeline-board-access

## Why a new `RowKind` rather than reusing `EmptyHint`

`EmptyHint` already models "a passive row whose `↵` synthesizes an action", and
its plumbing (`SidebarOutcome::Synthetic`) is exactly the seam this row needs.
What it cannot model is a **second** hinted action: its key and mouse handlers
hard-code `Action::NewTerminal`. Overloading it would have meant branching on the
label, which is the drift this codebase's row model exists to avoid. So the row
is `RowKind::PipelineSummary`, and it reuses `Synthetic` — the seam, not the
variant.

## Why the counts ride on the row rather than in the label

`SidebarRow::label` is bare text by contract (glyphs and connectors are composed
at render time), and the counts need two different tones — the live count is dim,
the human-parked count is the attention slot. So the row carries a
`PipelineSummary` and `sidebar_view` composes it, which also lets the slim rail
render just the one number that matters at four columns wide.

## Why the rollup is computed in `monitor_pipeline`

That module is already the pure fold over the roster (`ordered_rows`,
`stage_badges`, `stage_blocked`), unit-tested with no model, clock or DB. The
sidebar rollup is a fourth derivation of the same rows, computed on the same
off-loop hydration pass from the same single read — so it costs one extra pass
over rows already in memory, and no new wake source.

## Why `Alt b` and not the `Ctrl Alt` chrome layer

The chrome-toggle layer is where the monitor itself lives (`Ctrl Alt M`), and
that layer is precisely what fails on a legacy-encoding terminal: `Ctrl+M` is CR,
so the chord never arrives distinctly. Putting the board's own door in the same
layer would reproduce the bug it exists to fix. `Alt b` (b for board) is free
across every Alt family — creation (`w`/`t`/`p`), tool launches (`g`/`y`/`e`),
the `Alt m` media prefix, navigation, and the digit summons — and encodes as a
plain `ESC b` everywhere.

## Why a SECOND click on a masthead chip, not the first

The first click already opens the chip's own popup (the CPU sparkline, the memory
breakdown), which is a real surface people use; stealing it for the monitor would
be a regression. Double-click would need click timing the masthead does not track.
"Click again while your popup is up" needs no timer, reads as drill-down, and
lands on the same `MonitorTab::for_widget` mapping the popup's `↵`/`M` already
uses — so there is one mapping, not two.

## Why tab switches now report `PrefsChanged`

`MonitorPrefs::last_tab` is a persisted preference, and `PrefsChanged` is the
loop's only door to persisting them (it is what `[`, `]`, `g` and `s` already
report). Recording the tab at close instead would have meant catching six
different close paths; reporting the preference change where the preference
changes is the same shape as every other pref in that overlay.

## What was NOT done, and why

The audit asked for `parse_chord` to lower-case single-letter chord tokens and
synthesize `Shift` only from an explicit `Shift` keyword, so that `"Ctrl Alt M"`
would mean Ctrl+Alt+m. **That would collide six default chord pairs onto one
chord each** — `Alt w`/`Alt W` (worktree vs workspace), `Alt t`/`Alt T` (tab vs
terminal), `Alt n`/`Alt N` (split down vs right), `Alt x`/`Alt X` (close vs close
worktree), `Ctrl Alt p`/`Ctrl Alt P` (panel vs promote pin), and
`Ctrl Alt m`/`Ctrl Alt M` (notification mode vs monitor) — with the loser
silently unreachable. Letter case IS the Shift modifier in this grammar, and
`Key::modified` already folds an explicit `Shift` into the uppercase form so the
two spellings agree. The rule is now documented on `parse_chord` and pinned by a
test; what changed is the help page, which said `Ctrl-Alt-M` without saying that
the capital is the Shift.
