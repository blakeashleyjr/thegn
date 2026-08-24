## Context

A2 of the convergence: make the one two-implementation seam actually selectable and route its hot path through the trait.

## Goals / Non-Goals

**Goals:** `[git] backend`, `glyph_reads` on the trait, a shared host handle, doctor visibility. **Non-Goals:** moving writes off `CliGit` (spec: writes go through the CLI); a native write engine.

## Decisions

- `glyph_reads` default method composes `is_dirty`/`ahead_behind`/`current_branch` + CLI numstat; `GixGit` overrides to try the bridge batch first. The free function is deleted rather than deprecated (one caller).
- `GitBackend: Probe` supertrait so `Arc<dyn GitBackend>` probes; doctor shows the selected engine and notes the CLI write engine when it differs.
- Host handle mirrors `forge_handle` (OnceLock; default = native). Render/event-loop impact: none (reads stay on blocking threads). No help context change; `docs/help/configuration.md` already describes `config validate`/doctor; the example documents the key.

## Risks / Trade-offs

- [A `cli` selection makes the glyph scan spawn git per read] → that is the escape hatch's documented cost.
