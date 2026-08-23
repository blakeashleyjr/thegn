## Context

Row 4 of the extensibility convergence: the surface-consistency gates the audit listed that did not depend on the forge refactor, plus the documentation that makes the architecture discoverable and teachable. Every gate reuses an idiom already in the repo (schema walk, shrink-only ratchet, source-scan test).

## Goals / Non-Goals

**Goals:** the action registry is provably complete; no string-keyed palette verbs; one source for notification kinds; env knobs and the home-manager module cannot drift from the schema; strict validation catches typos; one place describes the architecture and names each invariant's gate.

**Non-Goals:** `--json` structural snapshots per command (the emitter ratchet is the first step; snapshots need per-command fixtures), generating the home-manager module from the schema (the drift test is the floor), `[git] backend` / editor seam (next phases).

## Decisions

- **Palette verbs become Actions, not a second registry.** Moving the four blocks into the `run.rs` action match costs nothing (same function, same locals) and gives them rebinding, `thegn keys list`, help coverage and the existing dispatch test for free. A "palette-only registry" would have been a third list.
- **Unknown keys: strict error, lenient warn-and-drop unchanged.** A launch is never blocked by a typo; `config validate --strict` (and the example-file test) is where it bites. The did-you-mean uses a ≤2-edit nearest key over the table's own properties. `llm_proxy` stays quiet in strict mode because the loader already warns.
- **Env completeness at depth ≤ 1 only.** Deeper tables are structured config; the 359 shallow keys without knobs are pinned with the header explaining that most are fine forever — the gate exists so a new key is a decision.
- **hm-module drift is a text parse, not a Nix evaluation.** The generate block is a flat attrset with four line shapes; parsing it keeps the test hermetic. Enum subsets are checked against canonical + aliases so `podman` (an alias) passes and `builtin` (nothing) fails.
- **Docs: single source with pointers**, not generation — CLAUDE.md needs agent-facing phrasing and `openspec/config.yaml` is injected into every proposal; both keep only the hard invariants and point at `docs/ARCHITECTURE.md`. The stale-docs guard exempts lines that _describe_ a ban.
- Render/event-loop impact: none. Help context: `docs/help/configuration.md` (existing page) documents env vars, unknown keys and the Nix module; new action ids are claimed by `command-palette`, `terminal-and-panes`, `copy-and-select`.

## Risks / Trade-offs

- [Moving palette blocks into the action match changes borrow context] → compiled and the palette/keymap suites pass; behaviour identical.
- [Unknown-key errors could reject a config that serde accepted] → only in `--strict`; the example file is tested clean; map tables accept any name.
- [hm-module parser is brittle to new Nix shapes] → the test asserts a minimum parsed count and fails loudly on a parse collapse.
