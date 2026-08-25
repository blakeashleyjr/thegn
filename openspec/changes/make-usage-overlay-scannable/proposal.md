# Make the usage overlay scannable (and grep-able)

Linear: THE-65

## Why

The AI-account usage surface (V 300: the `Alt u` overlay, the System ▸ Usage
panel section, the statusbar gauge) carries good information but reads as a
dense wall. Today `usage_dash.rs` emits, for **every** account: a heading, an
identity facts grid (org/seat/tier/home), a full table of every rate-limit
window, and a sparkline+forecast row per window — eight accounts produce ~40
undifferentiated rows. The user's report (screenshot on THE-65): "good info but
very dense, unclear", "hard to grep". Two concrete failures:

- **No hierarchy for the scan question.** The question is "which account is
  closest to a limit?" — but accounts render in discovery order, every window
  gets equal weight, and the discriminating identity facts (a 60-cell
  credential-home path) visually outweigh the numbers.
- **Not literally grep-able.** The data exists only as a TUI overlay; there is
  no `thegn usage` CLI, so nothing can be piped, filtered, or watched from a
  script.

## What Changes

1. **Compact-by-default overlay**: one row per account — label, plan, the
   _peak_ window's gauge + percent, and its reset countdown — sorted
   worst-first (`peak_percent` descending, unavailable/loading last), with the
   account heading toned by its peak window's tone. All gauges align in one
   column grid so percentages line up down the screen.
2. **Expandable detail**: selecting an account (the overlay already carries
   `sel`) expands it in place to today's full content — every window row,
   trend/forecast sparklines, and the identity facts grid (home shown
   abbreviated, expandable). Dense facts move _behind_ the expansion instead of
   preceding the numbers.
3. **The host-wide token rollup** keeps its clearly-labeled separate block,
   collapsed to its totals line by default.
4. **`thegn usage --json`** (with a plain-text table default): a new
   `usage.snapshot` capability row (Read scope) projected across the catalog
   surfaces, emitting the same per-account windows/facts payload the overlay
   renders — the literal grep answer, and what a statusline/script consumes.
5. The System ▸ Usage **panel section** adopts the same compact row shape.
6. `docs/help/ai-usage.md` documents the expansion key and the CLI verb (help
   ratchet).

## Impact

- **Roadmap**: V 300 (the shipped tracker — presentation + CLI door; data
  model untouched).
- **Specs**: new `usage-tracker` capability spec (the tracker landed
  pre-openspec with no spec; this change adds the requirements it introduces —
  a fuller retroactive spec of V 300 is welcome later but not required here).
- **In-flight / sibling changes**: independent of, but coordinated with,
  `add-agent-harness-seam` (THE-31 — the seam refactors how usage is
  _gathered_; this change is how it is _presented_; neither blocks the other).
  The `usage.snapshot` MCP projection rides the write-tools branch's scope
  gating for its listing (Read scope).
- **Render/event-loop**: presentation-only — the overlay re-renders on the
  existing refresh channel; expansion toggles are ordinary overlay damage
  (Full), no new wake sources, no new idle work.
- **No DB schema change.**
