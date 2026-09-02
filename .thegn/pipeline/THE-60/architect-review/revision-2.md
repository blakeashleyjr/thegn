# THE-60 revision 2 — off-loop remote activation and trust/cache delivery

## Blocking gaps

### 1. Remote activation still blocks the event loop

`activation_for_launch` calls `remote_requirements`, `remote_identity`,
`remote_binary_available`, `remote_shims_dir`, and (when approved)
`remote_env` synchronously. `spawn_worktree_shell_pane` calls
`launch_spec_center_with` directly from the event loop, so an SSH/provider
worktree can perform several bounded transports (up to the configured timeout)
before the pane opens. This violates `docs/ARCHITECTURE.md` §2 and the design's
cold-cache/non-blocking launch contract.

Make remote/provider activation cache-first in the same way as local
activation. A launch may consume a previously validated target cache or return
`Reserved`/safe shims immediately; target detection, target-byte identity,
binary/shims probing, and approved `mise env` resolution must run on an
off-loop worker and deliver the refreshed result through the existing refresh
channel plus `TerminalWaker`. If the ordinary shell-pane route cannot safely
compose a remote spec synchronously, move that resolution/attach handoff
off-loop rather than adding another blocking exception.

### 2. Remote trust cannot be approved through the supported trust surface

The remote branch derives an identity from target-side bytes, but
`handlers/repo_trust.rs` deliberately adds `mise.env` pending requests only for
local locations, and `cmd/repos.rs` computes its pending request from the local
root. For a remote/provider worktree whose checkout is absent or only a
placeholder locally, the target-derived canonical request is never shown and
cannot be approved with `thegn repo trust`. Consequently `remote_activation`
always falls back to shims and `install_on_target` rejects the same request
unless state was written out-of-band.

Extend the existing repo-trust listing/approval path with a target-aware,
off-loop request resolution. It must use the selected remote/provider
worktree's detection and bytes, never inspect a local placeholder, store the
same redacted `mise.env` canonical request under the existing repo-trust table,
and invalidate approval when target config/lock bytes change. Preserve
truthful `Reserved`/unavailable behavior when the target cannot answer.

### 3. Cache completion has no refresh/waker delivery

`prewarm`/`resolve_cached` writes the local activation cache and removes the
in-flight key, but sends no refresh notification and never pulses a
`TerminalWaker`. The first launch therefore remains on shims until another
unrelated model refresh, and the hydrated status does not promptly reflect the
new activation. Add the existing refresh-channel/waker delivery at cache
completion (including a safe failure/degraded result if the UI exposes it),
without retaining resolved values in logs or notifications.

### 4. Hydrated status does not carry missing-tool/degraded facts

`status` always returns an empty `missing_tools` list and reports a failed or
cold approved environment as the generic `shims` state. The environment panel
therefore cannot show the design-required missing-tool/degradation state; only
the synchronous doctor command performs `mise ls --missing`. Keep hydration
cache-only: persist a presence-only missing-tool summary/degradation marker in
the state-dir cache or an equivalent cache record, and render it without a
frame-time subprocess or resolved environment values.

## Files in scope

- `crates/thegn-host/src/mise_provider.rs`
- `crates/thegn-host/src/agent.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/handlers/repo_trust.rs`
- `crates/thegn-host/src/cmd/repos.rs`
- `crates/thegn-host/src/hydrate.rs`
- `crates/thegn-host/src/toolchain_ui.rs`
- `crates/thegn-host/src/panel/sections/environments.rs`
- `crates/thegn-host/src/tabbar_env.rs`

Keep the core activation seam, control schema, completion snapshots, and help
ratchets unchanged unless a strictly typed adapter is required. Do not add a
second daemon implementation, implicit install/trust, local-placeholder
fallback, synchronous remote subprocess, or new control/API/MCP capability.

## Focused verification

- Add a regression proving the ordinary remote shell-pane path does not invoke
  target transport synchronously on the event loop; exercise cache hit, cold,
  unavailable, and `Reserved` outcomes with fake target runners.
- Add remote trust listing/approval/revocation tests proving canonical target
  identity, edited-config re-prompt, and no placeholder reads.
- Add cache completion tests for refresh delivery, waker pulse, missing-tool
  summary, and failure/degraded status redaction.
- Run `cargo fmt --all -- --check`, `git diff --check`, `just quick thegn-host`,
  the relevant `thegn-core` detection/activation tests, and the focused host
  nextest groups for `mise_provider`, `host_provision`, `doctor`, `hydrate`,
  and launch behavior. Re-run the control-schema and completion snapshot
  checks.

## Commit subject

`fix(host): make remote toolchain activation cache-first`
