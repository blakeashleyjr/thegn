## Why

The git read engine was a hard-coded `GixGit::new()` at 15 host sites and the hot-path `glyph_reads` was a free function bypassing the `GitBackend` trait — so the one seam with two implementations could not actually be selected, and a user hitting a gix edge case had no escape hatch. Audit item A2.

## What Changes

- `[git] backend = auto | gix | cli` (`config_enum! GitBackendKind`; pin 68 → 69), documented in the example.
- `GitBackend::glyph_reads` is a trait method (default composes the backend's reads; `GixGit` overrides to ride the bridge `exec.batch`); the free fn is gone. `GitBackend: Probe` so both engines report to `thegn doctor` (the selection plus "writes (always): cli").
- `thegn_svc::git::backend_for(kind) -> Arc<dyn GitBackend>` with a `kind_coverage` test.
- Host `git_handle::{install, get}` (the `forge_handle` shape); every read site takes the handle; `just lint` rejects `GixGit::new()` in the host. Write/plumbing sites keep an explicit `CliGit` — writes always go through the CLI.

## Capabilities

### Modified Capabilities

- `git-backend`: the read engine is config-selected; glyph reads go through the trait.

## Impact

`crates/thegn-core/src/config.rs`, `config_validate.rs` (pin), `config/config.toml.example`, `crates/thegn-svc/src/git/mod.rs`, `seam/registry.rs`, `crates/thegn-host/src/{git_handle.rs,main.rs,run.rs,hydrate.rs,remote_poll.rs}`, `justfile`. No schema/render change. Roadmap: audit A2; git-backend spec.
