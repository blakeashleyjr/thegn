---
files:
  - crates/thegn-core/src/hooks.rs
  - crates/thegn-core/src/lib.rs
  - crates/thegn-core/src/config.rs
  - crates/thegn-core/src/config_resolve.rs
  - crates/thegn-core/tests/env_overlay_coverage.rs
  - crates/thegn-core/tests/config_example.rs
  - config/config.toml.example
  - test/env-overlay-ratchet.txt
overlaps: []
after: []
---

# Chunk 1 — core lifecycle policy, config, and trust

## Scope and approach

Implement the substrate-free policy described in `architect/design.md`.
Create `thegn_core::hooks` as a new sibling module. Add typed hook config to
global `Config`, `WorkspaceConfig`, and the typed repo overlay. Support string
shorthand plus `{ command, wait, timeout_secs, on_failure }` entries, event
defaults, ordered global → workspace → repo accumulation, and the legacy
`[sandbox].prepare` → head-of-`post_create` normalization.

Extend the existing repo overlay classifier in `config_resolve.rs` with
canonical per-event `hooks.<event>` gated requests. Return unapproved repo
hooks as pending and omit them from executable policy. Approved repo hooks are
still warn-only. Keep all command execution, process environment reads, and
filesystem work out of core.

Add pure tests for ordering, trust pending, canonical edits, invalid policy,
prepare compatibility, forced/unattended failure bounds, and secret-free
`THEGN_*` environment projection. Update config example comments for every
new key and update `test/env-overlay-ratchet.txt` with the six structured hook
keys (explicitly documenting why they have no `THEGN_*` scalar override).

Do not add a capability/catalog row, control API field, CLI argument, or
completion slot. The control-schema, completion, and help ratchets are
therefore intentionally unchanged in this chunk; run their existing tests if
the core config parser exposes a drift.

## Dependencies and overlap

No overlap with chunks 2 or 3. This chunk is the API prerequisite for chunk 2;
the Lead must run chunk 1 before chunk 2. Chunk 3 may be prepared in parallel
as documentation, but its final verification runs after the public config
shape is settled.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core hooks`
- `cargo nextest run -p thegn-core env_overlay_coverage`
- `cargo nextest run -p thegn-core config_example`

Do not run a full-workspace build, `just test`, `just ci`, or e2e.

## Done criteria

- All paths in the frontmatter are the only paths touched by this coder.
- Core policy has no shell/process/filesystem/substrate dependency.
- Existing repo trust canonicalization and pending-request behavior are used,
  not duplicated.
- Every new config key is documented and the config example parses.
- The env-overlay ratchet is updated with an explicit structured-policy reason;
  completion/control/help ratchets remain clean and unchanged.
- Scoped tests above pass.
- Commit exactly with subject: `feat(the-19): add core lifecycle hook policy`
