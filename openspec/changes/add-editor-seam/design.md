## Context

The seam pattern (openspec/specs/provider-seams) is established: object-safe trait + `Probe` + classifying error + config selection. The editor differs from forge/ci in that every "impl" is process-bound and the product of resolution is a _shell line_, not an API client.

## Goals / Non-Goals

- Goals: one resolution ladder; program-aware jump syntax; placement decided by the seam (caps), overridable by config; doctor probe; env knobs.
- Non-Goals: LSP-style editor RPC; per-workspace editor selection; Windows `start`-style openers (the launch line already runs under the user's shell).

## Decisions

- **The seam lives in thegn-core**, not svc: resolution is pure config+env logic with no subprocess or network, and the host needs it on synchronous paths (key handlers, CLI). Probe stays cheap by contract.
- **`open()` returns a launch plan** (`EditorLaunch{command, placement}`) rather than spawning: the host owns PTY panes and detached spawns; the CLI hands the terminal over. Keeps core substrate-free.
- **Three-layer id** (`template`/`tool`/`visual`/`env`/`vi`) reported via `Probe` so doctor shows _which_ layer won and why.
- **`open_in = auto`** maps windowed programs (code/zed/subl/gvim/idea…) to `External`, terminal programs to `Pane`; `pane`/`external` force it. A GUI editor forced into a pane is the user's call (e.g. `code --wait` workflows).
- **Quoting** via `util::sh_quote` (bare when shell-safe); template substitution quotes `{path}` but leaves the rest of the user's template verbatim.

## Risks / Trade-offs

- The program table is a heuristic keyed on basename; unknown programs degrade to "open the file, no jump" (caps say so honestly).
- The legacy `[[tools]] editor` default `${EDITOR:-vi} .` is recognized and skipped so the env layer keeps winning for unconfigured users — behavior-compatible with the old string hack.
