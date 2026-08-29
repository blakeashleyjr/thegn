# Chunk 2 — host provider, launch parity, trust, doctor, and status

## Scope

Implement the host-side mise provider and consume the chunk-1 activation plan
for every normal pane and daemon agent. Add doctor and cached status surfaces.
This chunk is serial after chunk 1 and before chunk 3. It must not add a new
control capability or modify the keymap/help ratchets.

## Files touched

- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/mise_provider.rs` (new)
- `crates/thegn-host/src/agent.rs`
- `crates/thegn-host/src/host_provision.rs`
- `crates/thegn-host/src/handlers/repo_trust.rs`
- `crates/thegn-host/src/cmd/repos.rs`
- `crates/thegn-host/src/cmd/doctor.rs`
- `crates/thegn-host/src/toolchain_ui.rs` (new)
- `crates/thegn-host/src/hydrate.rs`
- `crates/thegn-host/src/panel/mod.rs`
- `crates/thegn-host/src/panel/sections/environments.rs`
- `crates/thegn-host/src/tabbar_env.rs`

Do not add a second implementation in `daemon/agent_open.rs`; verify its
existing call to `agent::launch_spec_full` remains the only daemon path. Do not
touch `crates/thegn-svc/src/seam/registry.rs`; the host provider cannot be
introduced into the service crate without reversing dependency direction.

## Approach

1. Put every `mise` executable name, command construction, trust inspection,
   `mise env -s json`, missing-tool query, install operation, timeout, and
   output parser in `mise_provider.rs`. Use a fake runner for unit tests. Never
   curl/install host mise automatically.
2. Rehome the current `host_provision.rs:1276-1284` operation behind the
   provider. Remove unconditional trust. Keep any provisioning step explicitly
   off-loop and target-aware; pane spawn itself must never invoke install.
3. Add the Nix and direnv adapters at the activation-plan boundary, preserving
   their existing caches/warmers. Add a mise cache under the state dir with
   0600 permissions, in-flight dedupe, bounded background resolution, refresh
   channel delivery, and a `TerminalWaker` pulse. Cold cache means shims/base,
   not a blocking launch.
4. In `agent::launch_spec_full`, consume one composed plan after bundle and
   devshell inputs are known. Apply it to sandbox and host launch forms through
   one helper. Preserve selected `[env.<name>]` placement and remote detection;
   return `Reserved`/degraded status rather than failing a shell when a target
   cannot answer.
5. Extend the existing repo-trust surface so `mise.env` is listed, approved,
   and revoked by `thegn repo trust`. Only approved config sets may resolve
   host env or install. Do not persist resolved env values or print them.
6. Add `toolchain_ui.rs` to read cached status and missing-tool summaries
   without subprocesses. Hydrate it off-loop into `PanelData`, render the
   worktree toolchain row in the Environments section, and add the terse status
   token through `tabbar_env.rs`. Missing binary/tools, pending trust, off, and
   ready states must be distinguishable and degrade through existing color/glyph
   chokepoints.
7. Extend doctor JSON/text with the worktree-context toolchain report. It must
   show binary/version, files, tier, inject mode, trust/env state, missing tools,
   and degradation reason; it must never show env values. A missing binary is
   informational, not a failing doctor exit.

## Dependency/overlap

Serial after chunk 1 because it consumes the core seam/config types. Chunk 3 is
serial after this chunk because its action dispatch calls the provider install
operation. Files are disjoint from chunks 1 and 3.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host mise_provider`
- `cargo nextest run -p thegn-host toolchain`
- `cargo nextest run -p thegn-host doctor`
- `cargo nextest run -p thegn-host hydrate`
- `bash test/smoke.sh` with `XDG_STATE_HOME` set to a fresh temporary
  directory, if the smoke harness is used for the doctor/status path

Do not run e2e or any `thegn` command against the normal state directory.

## Done criteria

- `rg` finds no `mise` process construction outside
  `crates/thegn-host/src/mise_provider.rs` (docs/tests may describe it).
- Plain pane, sandbox pane, remote target, and daemon agent all consume the
  same activation plan and never block the event loop on mise.
- Untrusted/edited config never reaches host env resolution or install; shims
  remain the safe fallback; missing tools are visible.
- Existing Nix/direnv behavior and `[env.<name>]` selection remain intact.
- Doctor and cached UI status are redacted, deterministic, and failure-tolerant.
- Commit exactly as: `feat(host): wire mise provider into launches`
