## Why

thegn already downloads, version-pins, and manages exactly one external tool
— the `pi` coding agent under `~/.thegn/pi` — but that logic is a one-off
baked into `cmd/agent.rs`. Upcoming work needs the same "acquire and pin an
external binary" behavior for more tools (a debug adapter, user-declared MCP
servers, future helpers). Rather than copy the pi install per tool, this change
extracts the pattern Zed's extensions use — resolve a tool by user-override →
PATH → download-and-pin, per platform, with an update policy and graceful
fallback — into one reusable, unit-tested resolver.

## What Changes

- Add a **`managed_tool` module** to `thegn-core` holding a pure, testable
  `ManagedTool` spec and the resolution _decision_ logic:
  - a tool spec: stable `name`; `source` = `GithubRelease { repo, per-(os,arch)
asset pattern }` or `Npm { package }`; a pinned `version`; a `check_updates`
    policy (`always` / `once` / `never`); and PATH fallback command names.
  - a pure 3-tier `resolve()` decision returning which tier satisfies the tool:
    (1) explicit user override (`binary.path` / extra args from config), (2)
    project-shell PATH lookup (via `util::which_path`), (3) the managed
    download+pin location under `~/.thegn`.
  - deterministic cache/bin **path computation** and an `is_current()`
    version-marker check (generalizing today's `.thegn-pi-version` marker).
  - per-`(os, arch)` **asset selection** for GitHub-release sources.
- Keep the **side-effecting fetch in `thegn-host`** (npm subprocess for
  `Npm`; `reqwest`/subprocess download + `chmod +x` for `GithubRelease`),
  reusing the existing `run_setup_cmd` pattern — because `thegn-core` is
  substrate-agnostic and carries no HTTP client. Core decides; host acts.
- **Refactor the managed-pi install** (`cmd/agent.rs` `setup`/`is_current`,
  `pi_assets.rs` `PI_PIN`) to describe pi as a `ManagedTool` and drive install
  through the shared resolver. `thegn agent setup` behavior is unchanged.
- **Surface resolved managed tools in `thegn doctor`** — for each managed tool,
  which tier resolved it, the path, and pinned-vs-current version.

Non-goals (later phases, listed for context, not built here): the BugStalker
debug adapter, user-declared MCP servers, and capability-scoped grants. This
change only lands the resolver + the pi refactor as its first consumer.

## Capabilities

### New Capabilities

- `managed-tools`: how thegn resolves and pins external tool binaries — the
  spec/source model, the 3-tier resolution order, per-platform asset selection,
  the pin/update policy, `doctor` reporting, and the core-decides/host-fetches
  split.

### Modified Capabilities

- `agent`: the managed-pi install is re-expressed as a `managed-tools` consumer.
  Observable behavior of `thegn agent setup` (idempotent install, re-seed of
  the `thegn-acp` package, version marker) is unchanged; the requirement
  delta records that pi acquisition now flows through the shared resolver.

## Impact

- **Code:** new `crates/thegn-core/src/managed_tool.rs` (+ `lib.rs` export);
  `crates/thegn-host/src/cmd/agent.rs` and `src/pi_assets.rs` refactored onto
  the spec; `crates/thegn-host/src/cmd/doctor.rs` gains a managed-tools
  section. Config gains an optional per-tool override block (read via existing
  layered-config plumbing in `config.rs`).
- **Dependencies:** none new in core (pure logic). Host reuses existing
  `reqwest`; no tokio work on the event loop (install stays a CLI/`spawn_blocking`
  path, as pi does today).
- **Invariants:** core stays HTTP/tokio-free and 95%-coverage-gated (the pure
  resolver is fully unit-tested; the fetch seam is `cov_ignore` + smoke-tested).
  No event-loop or render-plan surface is touched.
- **Roadmap (`tasks.md`):** foundation for **R 235** (ACP registry —
  one-command install/launch of authenticated agents), the **AL** MCP-server
  group (455–466, user-declared MCP servers consume this), and **D 54 / #657**
  worktree setup/post-create tooling; generalizes the existing managed-pi work
  in the agent track (Q–R).
