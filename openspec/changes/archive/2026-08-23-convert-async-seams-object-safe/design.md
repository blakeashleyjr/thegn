## Context

The seam vocabulary (thegn-core `seam`, svc `Ladder`/`Router`) landed in earlier slices; sync seams (forge, git, ci, editor) already converged. The async stragglers predate the pattern.

## Goals / Non-Goals

- Goals: object-safety for every async seam; delete blind delegation enums; keep behavior byte-identical; burn the async-trait ratchet to zero.
- Non-Goals: changing any provider's semantics, capabilities, or error text; introducing the `async-trait` proc-macro crate (the house pattern is explicit `BoxFuture`).

## Decisions

- **Explicit `BoxFuture`, not `#[async_trait]`**: no new dependency, matches `ControlApi`, and keeps the allocation visible at the seam boundary.
- **`enum Provider` survives**: it is not a blind router — it encodes the Iroh exec-only facade, the capability table, and name resolution. The refactor extracts `remote()/files_ops()/egress_ops()/checkpoint_ops()` accessors returning `Option<&dyn Trait>` so each public method is one line and a new provider variant touches the accessors + caps, not 24 match blocks.
- **Defaulted trait methods keep their defaults** (`ProviderFiles::{write_exec,upload_dir,download_dir}`, issue comments/labels) — wrapped bodies, same fall-through semantics.

## Risks / Trade-offs

- One `Box` per async call at the seam boundary — noise-level next to the network round-trip every such call performs.
- Mechanical wrapping of ~100 methods risks transcription errors; mitigated by unchanged bodies + the per-seam test suites.
