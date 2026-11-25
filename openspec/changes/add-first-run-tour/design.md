# Design: first-run welcome tour

## Shape

`crate::welcome::WelcomeTour` is a pure state machine plus a renderer, in the
same style as `crate::onboarding::OnboardingWizard`: `handle_key` returns a
`TourOutcome` (`Pending`, `Do(effects)`, `Close { completed, writes,
open_setup }`) and never touches I/O. It lives in the existing loop slot
`handlers::onboarding::OnboardingUi` (new `tour` field), so `run.rs` keeps its
single modal check, render call, paste route and probe route. Only the palette
dispatch line changes (it opens the tour).

## Chapters

| #   | Chapter   | Reads                                 | May write                            |
| --- | --------- | ------------------------------------- | ------------------------------------ |
| 0   | Welcome   | -                                     | -                                    |
| 1   | Workspace | live chord hints                      | -                                    |
| 2   | Look      | `theme.preset`, keymap preset         | `theme.preset`, keymap preset        |
| 3   | Sandbox   | `sandbox.*`, support report probe     | `sandbox.profile`, `sandbox.backend` |
| 4   | Agents    | `[[agents]]`, PATH probe of harnesses | `[[agents]]` order (promote)         |
| 5   | Remote    | -                                     | -                                    |
| 6   | Keys      | live chord hints                      | -                                    |
| 7   | Finish    | the pending diff                      | everything above, on Enter           |

Keys: `Enter` accept and continue, `Tab` skip the chapter (its choice is
reset to the current config), `Shift-Tab`/`Backspace` back, `↑↓` choose,
`←→` secondary choice, `Esc`/`Ctrl-C` leave the tour without writing. In the
Keys chapter every other key is a drill attempt. The `esc · skip tour`
control painted under the header on every chapter is that same leave, so the
way out is never only a footer hint.

## Probes (off the loop)

- Sandbox: the existing `ProbeRequest::Sandbox(chain)` →
  `sandbox_support::support_report`, the same report `thegn doctor` prints.
  `r` clears the probe cache and re-asks.
- Agents: new `ProbeRequest::Agents` → for each `harness::HARNESSES` entry,
  `util::which_path(interactive program)` inside `spawn_blocking`.

Both return on the refresh channel (`RefreshKind::Onboarding`) like the
wizard's probes. `handlers::onboarding::apply_probe` routes a result to the
tour when it is open.

## Writes

The finish screen lists each pending write as `key: old → new`. `Enter` saves.
The loop applies the keymap preset (a `ui_state` write through `db_task`, as
today) and hands the config-file writes to `spawn_blocking`; the result comes
back as an in-app toast (`RefreshKind::Toast`). Each write is one
`config_write` call (`set_key`, `promote_agent`), which preserves comments and
unrelated keys and refuses to clobber an unreadable file. Reverting is the
same keys set back, or `thegn config edit`.

`promote_agent(path, name, command, keep)`:

- `[[agents]]` present and contains `name`: move that table to the front,
  keeping every other table and its comments.
- present without `name`: insert `{name, command}` at the front.
- absent: write `[[agents]]` = the chosen agent followed by `keep` (the
  effective built-in list, so the shell fallback and the old default stay).

Table display order is re-pinned by reassigning the original document
positions in the new order (toml_edit sorts tables by position, stably).

## Theme preview

Moving through the theme list emits `preview_theme`; the loop sets the runtime
palette, so the whole screen (tour included) recolours on the next frame.
Leaving without saving restores the configured palette, the same path the
wizard uses.

## The diagram and the drill

A tiny model (`Mini`: worktrees, panes per worktree, palette flag) renders as
a sidebar + tab strip + pane boxes using `active_glyphs()` box glyphs. In the
Workspace chapter `↑↓` highlights project → worktree → tab → pane with a
one-line explanation. In the Keys chapter the four drills (new worktree,
split, palette, close) resolve their chord from the user's live keymap
(`chord_hint_for` + `parse_chord`); a matching key press mutates `Mini`, so
the user sees what the chord does without creating anything. After three
misses a drill names the likely cause (terminal not sending Alt) and the help
page. Multi-key chords cannot be drilled and are shown as text.

## Accessibility

- Fits 80x24: header (1 row), the skip control row, body (≤ 20 rows), footer
  (1 row). Below 60x18 it shows a compact notice with the Esc/Enter keys.
- No colour-only meaning; ASCII glyph fallback via `active_glyphs()`.
- No motion.

## First-run arming

Unchanged detection (`ui_state` key `onboarding_done` absent). First run now
opens the tour instead of the wizard; `thegn setup` still opens the wizard.
`THEGN_SKIP_ONBOARDING` still suppresses both. The marker is written when the
tour closes either way, recording `completed` or `skipped`.

## Left out (not solid by Monday)

- Inline remote-provider setup (Sprites/Vercel tokens).
- Per-worktree sandbox pinning from the tour.
- Opening help pages from inside the tour (it names them instead).
