## Context

superzej's panel is a three-tab accordion (Git/Work/System) of ~24 sections; a
section is a `pub fn content(ctx: &SectionCtx) -> Vec<PanelRow>` plus registry
entries in `panel/mod.rs` + `panel/sections/mod.rs` — none of which are ratcheted
god-files. The ratcheted `run.rs` (23299) and `chrome.rs` (4819) are both at
their ceilings, so anything requiring a `run.rs` PanelMsg handler (like a
custom Enter action) can't be added without banking room first.

Per-worktree CI is already cached (`ci_runs_cache`, `db.get_ci_cache(worktree)`,
`db.worktrees()`); `CiState::is_failure()` marks failing runs. Hydration
(`hydrate.rs`, 2714/3000 lines) runs producers on `spawn_blocking` and pulses the
waker — the sanctioned place to build cross-worktree data off the loop.

Zed's multibuffer is the model: a stream of excerpts, each a window onto a
source, with jump-to-source as the primary action.

## Goals / Non-Goals

**Goals:**

- A pure, exhaustively-tested aggregation model in core (the reusable
  "multibuffer" data structure).
- A read-only Work-tab section rendering it, grouped by worktree with source
  labels, across the three panel widths.
- Off-loop population from the cross-worktree CI cache (query-free).

**Non-Goals:**

- One-key jump-to-source (needs a `run.rs` arm; ship `jump_target()` and defer
  the keystroke).
- Interactive cross-worktree content **search** (needs a query input; the model
  accepts content-match excerpts so it's ready).
- Growing any ratcheted file.

## Decisions

### Pure model in core, data fetched by host

```
pub enum ExcerptKind { CiFailure, DirtyFile, ContentMatch }   // severity order
pub struct Excerpt { worktree, worktree_label, kind, file, line: Option<u64>, text, detail }
pub struct Aggregation { excerpts: Vec<Excerpt> }             // stored pre-sorted
pub enum AggRow { Group { label, count }, Excerpt(usize) }    // idx into sorted excerpts

Aggregation::from_excerpts(Vec<Excerpt>) -> Self              // sorts (label, kind, file, line, text)
  .excerpts()/.is_empty()/.len()
  .rows() -> Vec<AggRow>                                      // dividers + excerpt-index rows
  .jump_target(flat_idx) -> Option<&Excerpt>
  .summary() -> AggSummary { failures, dirty, matches, worktrees }

// pure builders (host feeds already-fetched data — keeps core I/O-free)
ci_failure_excerpts(worktree, label, &[CiRun]) -> Vec<Excerpt>
dirty_file_excerpts(worktree, label, &[(path, status)]) -> Vec<Excerpt>
content_match_excerpts(worktree, label, &[(file, line, text)]) -> Vec<Excerpt>
```

- **Why pure + host-fetched:** keeps the reusable logic in the coverage-gated
  core (no DB/git there) and the DB reads in the host, mirroring the
  Phase-1/Phase-2 core-decides/host-acts split.
- **Sort key** `(worktree_label, kind severity, file, line, text)` → deterministic
  groups in label order, failures first within a group. Group-severity ordering
  (worktrees-with-failures float up) is a later, easy tweak; deterministic label
  order is simpler to test now.
- **`rows()` returns indices, not borrows**, so the panel can map cursor row →
  `Excerpt(i)` → `jump_target(i)` without lifetime gymnastics.

### Section wiring (no ratcheted files)

- `panel/mod.rs`: add `Section::Across` (enum + `SECTION_ORDER` + label/key +
  `tab() = Work`) and a `PanelData.across: Aggregation` field.
- `panel/sections/mod.rs`: `mod across;` + one dispatch arm.
- `panel/sections/across.rs`: `content(ctx)` renders `across.rows()` — a group
  header per worktree, then `label · file:line · text` rows — with the three
  view widths (normal: text only; half/full: + worktree + detail). Empty state
  when the aggregation is empty. Unit-tested with a synthetic `Aggregation`.
- Navigation uses the existing generic `accordion_key` (j/k, section hop) — no
  new keybind.

### Population (hydrate.rs, off-loop)

A `spawn_blocking` producer reads `db.worktrees()`, and for each calls
`db.get_ci_cache(path)`, deserializes the `Vec<CiRun>`, builds
`ci_failure_excerpts`, collects all, `Aggregation::from_excerpts`, and hands it
back over the model channel (waker-pulsed) into `model.panel.across`. Cheap
(DB reads only), query-free, and stale-safe via the existing hydration
generation discipline.

## Risks / Trade-offs

- **[no jump keystroke]** Users see the source (`worktree · file:line`) but can't
  one-key jump yet. → The label makes the source actionable manually; the model
  is jump-ready; the keystroke lands when `run.rs` room is banked.
- **[CI-cache freshness]** Aggregation reflects cached CI, not a live fetch. →
  Acceptable: it mirrors what the per-worktree CI section already shows; refresh
  follows the existing CI cache cadence.
- **[cost across many worktrees]** Reading every worktree's CI cache. → DB reads
  only, on `spawn_blocking`; no git/network. Bounded by worktree count.

## Migration Plan

Pure addition: a new core module, a new section, a new optional `PanelData`
field (defaults empty), and a hydration producer. No schema/config change, no
persisted state. Rollback = revert.

## Open Questions

- Group ordering: label-asc now; revisit "failures-first" once dirty/content
  producers land and the surface has mixed kinds.
