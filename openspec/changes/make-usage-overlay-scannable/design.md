# Design — scannable usage overlay

## Rendering only

`usage_sections` (in `detail/usage_dash.rs`) currently emits
heading → facts grid → windows table → sparkrows per account, unconditionally.
The redesign makes emission a function of `(sort order, expanded set)`:

- Compact row = the existing `window_row` cells for `peak_window()` only,
  prefixed by the account label (toned by `tone(peak_percent)`), plan note
  kept right-aligned. `peak_percent`/`peak_window` already exist in
  `thegn_core::usage` — no new data.
- Sort: `peak_percent` descending; `Loading`/`Unavailable` accounts sink to the
  bottom with their existing toned headings.
- Expansion state lives on the overlay (the `DetailOverlay` already tracks
  `sel`; add the expanded-keys set beside it). Expanding re-emits today's full
  block for that account — no behavior change inside it beyond abbreviating
  `home` (`~`-relative, full path on the expanded facts grid).
- The token rollup block collapses to its heading + totals grid; the top-models
  table appears when expanded.

Pure section-building logic (given accounts + expansion set → section list) is
extracted into a unit-testable function; tests pin worst-first order,
loading-last, compact row shape, and expansion round-trip. Damage: overlay
content changes are `Full` frames as today; no render-plan change.

The System ▸ Usage panel section (`panel/sections/usage.rs`) reuses the compact
row builder — one shared function, per the shared-list-fn lesson from the panel
audits.

## The snapshot verb

`usage.snapshot` (Read) follows the fixed catalog ratchet (Verb + scope arm +
`cap(...)` + `ControlApi` + route; gRPC mirrored or a recorded gap). The CLI is
`thegn usage [--json]`: default a plain aligned table (account, plan, peak
window, used %, resets-in), `--json` the full `AccountUsage` payload plus
history keys — emitted via the one-emitter convention. The gather reuses
`thegn_svc::usage::gather` off-loop exactly as `spawn_usage` does; the CLI path
does not touch the compositor loop (it is a separate process).

`--fetch` (live refresh) is NOT added: the CLI reads local harness state and
whatever the tracker has cached, matching the tracker's opt-in posture on
outward fetches.

## Security

- The snapshot payload contains account emails, org names, plan tiers, and
  credential-home _paths_ (never credential contents — same fields the overlay
  already shows). It is Read-scoped; over MCP it is listed only when the
  effective scope set grants read, so an agent given a none-scoped server
  cannot enumerate the operator's accounts.
- No new write surface, no config keys carrying secrets, no sandbox
  implications (no new spawn paths).

## Open questions

- Expansion key: Enter on the selected row vs a dedicated toggle — decide with
  the existing detail-overlay conventions (`ci_drill` precedent). Whatever
  lands must appear in `docs/help/ai-usage.md` (help ratchet).
- Whether `thegn usage` also prints the host-wide token rollup by default or
  behind `--tokens` (leaning `--tokens`: it is the slow scan and the CLI should
  stay instant).
