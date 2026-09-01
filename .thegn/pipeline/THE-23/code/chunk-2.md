# THE-23 chunk 2 — host provider, launch, doctor, and env token

## Files touched

- `crates/thegn-host/src/devcontainer_provider.rs` (new)
- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/handlers/repo_trust.rs`
- `crates/thegn-host/src/host_provision.rs`
- `crates/thegn-host/src/agent.rs`
- `crates/thegn-host/src/cmd/doctor.rs`
- `crates/thegn-host/src/hydrate.rs`
- `crates/thegn-host/src/chrome.rs`
- `crates/thegn-host/src/tabbar_env.rs`
- `crates/thegn-host/src/sidebar.rs`
- `crates/thegn-host/src/handlers/switch_cache.rs`
- `test/smoke.sh`

Do not touch core files, config example, OpenSpec/docs, or ratchet files in this
chunk.

## Approach

Create an object-safe host-owned `DevcontainerProvider` seam. Keep all
`Command` construction, executable discovery, bounded probe, CLI version/status
handling, and CLI-specific argv inside its implementation. It returns a
provider capability report and an opaque started-container handle; it never
exposes vendor flags to core or UI code. The probe is suitable for doctor and
does not build, start, or execute repo commands.

In the existing off-loop trusted resolution/provision path, select the CLI
provider when it is ready and a trusted source is present; otherwise use the
existing OCI backend path. The worktree is the bind-mounted workspace in both
paths. Do not add a backend enum or duplicate sandbox resolution. Pane and
agent execution must use the existing `sandbox::enter_argv` path and retain
`sandbox_cpucap::wrap_pane_argv`; background agent handoff retains
`wrap_background_argv`. Lifecycle and feature steps remain trust-gated and
postCreate remains one-time.

Plumb the core selection result and effective env passthrough allowlist through
`repo_trust`; remove the unrestricted local-env closure. Surface parse,
ambiguity, blocked-localEnv, refused/reserved, pending, and non-OCI/provider
degrade reasons once through the existing notification/status path. Respect
`devcontainer = off` before any parse or trust lookup.

Extend doctor text and JSON with the repo-context devcontainer status,
provider probe, category approval/pending state, disposition lists, and backend
honorability. Keep doctor read-only apart from a bounded executable probe.

During off-loop hydration, derive a transient per-worktree status and carry it
through the path-keyed switch cache. Render it in the existing sidebar env token
and active tab-bar env cluster with truthful pending/ready/degraded states. Do
not persist it or add a DB migration. Clear it on worktree switches exactly as
the existing backend/placement cache fields are cleared.

Add a smoke assertion for the doctor block using a temporary `XDG_STATE_HOME`
and a fixture/repo context; do not invoke the live state DB or an unchecked
worktree binary.

## Dependency / overlap

Serial after chunk 1: this chunk consumes the core APIs and config fields from
chunk 1. Chunk 3 is serial after this chunk because its help/spec wording must
match the final provider and sidebar states. The file set is disjoint from both
other chunks.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host devcontainer`
- `cargo nextest run -p thegn-host doctor`
- `cargo nextest run -p thegn-host switch_cache`

Run the focused smoke test target/filter used by the repository for
`test/smoke.sh`, with `XDG_STATE_HOME` set to a newly created temporary
directory. Do not run e2e, `just test`, `just ci`, migrations, or a live-state
`thegn` invocation.

Ratchets in this commit: run completion-slot, control-schema, and all help
ratchet checks; they remain unchanged because the provider adds no action,
argument, panel context, or control operation. Do not add a capability catalog
row merely for the provider or doctor section.

## Done criteria

- No vendor CLI process can be started from core, the UI loop, or an unapproved
  repo config; provider absence falls back to the existing OCI seams.
- Both pane and agent launches preserve worktree bind mounting, backend
  selection, and CPU-cap wrappers.
- Doctor and sidebar use the same status decision and distinguish off,
  ambiguous/invalid, pending, ready, and degraded states without stale cache
  leakage.
- Focused tests and smoke pass with isolated XDG state; no DB schema changed.
- Commit exactly as: `feat(the-23): host devcontainer provider and status`
