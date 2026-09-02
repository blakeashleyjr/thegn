# THE-60 revision 1 — provider-owned install and remote parity

## Gap

The core change removed the old `Tier::ToolVersions` mise install step, but
the host-side replacement was not wired into the existing provider-environment
provisioner. `crates/thegn-host/src/host_provision.rs` still detects the target
with `DETECT_PROBE_SCRIPT`, but its `envplan::plan` now produces no step for a
`ToolVersions` tier and the provision loop has no provider-owned replacement.
Consequently, an OCI/provider worktree with a mise/asdf declaration silently
opens without its declared tools. The new palette action cannot repair that
case: `mise_provider::install` rejects every remote/provider target, and the
action has no target-aware remote operation.

The same omission leaves remote activation incomplete. `activation_for_launch`
returns `Reserved` for remote locations without consuming the existing remote
detection probe, while trust calculation previously inspected the host path
before the remote branch. A remote worktree therefore cannot report or use its
detected declarations, and the UI/status path must not mistake a local
placeholder directory for the remote config set.

## Required correction

- Keep all `mise` process construction, command names, output parsing, timeout,
  sanitization, and install policy in `crates/thegn-host/src/mise_provider.rs`.
  Do not restore a vendor command or implicit install to core.
- Give the provider an explicit target adapter for local, SSH/provider, and
  existing OCI/provider provisioning contexts. The adapter must run the
  approved install against the selected target's worktree, with the same
  bounded/sanitized/no-output contract. It must not download or install a
  host-side mise binary and must refuse unapproved or changed config identity.
- Wire the existing `host_provision.rs` target path so a `ToolVersions` step is
  either an explicit, provider-owned provisioning operation or is clearly
  skipped only when the target cannot answer. It must not silently drop the
  declared toolchain for a target that can run it. Preserve the provision
  marker/idempotence and progress/error reporting.
- Use the existing remote detection/exec seam to feed
  `DetectedToolchainFiles`/config-set identity for remote targets. Do not read
  the host placeholder worktree to derive remote trust or cache state. Remote
  activation may degrade to `Reserved` when the target lacks mise, but that
  outcome must be reported with a deterministic reason and not silently look
  like a local bare shell.
- Keep the explicit `toolchain-install` action target-safe: a missing local
  worktree must not fall back to the compositor cwd, and remote/provider
  targets must either dispatch through the provider target adapter or give a
  truthful refusal. Do not make the action run synchronously on the input
  loop.
- Ensure hydrated/doctor status is cache-only for local worktrees and does not
  inspect a local placeholder for a remote/provider worktree. Presence-only
  status must distinguish remote/reserved, pending trust, missing binary or
  shims, and ready/degraded states without printing environment values.

## Files in scope

- `crates/thegn-host/src/mise_provider.rs`
- `crates/thegn-host/src/host_provision.rs`
- `crates/thegn-host/src/agent.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/handlers/repo_trust.rs`
- `crates/thegn-host/src/hydrate.rs`
- `crates/thegn-host/src/toolchain_ui.rs`

Keep the core activation seam and control/help snapshots unchanged unless a
small typed adapter change is strictly required. Do not reintroduce an
implicit install, `mise trust`, curl/bootstrap path, or a second daemon-only
implementation. Avoid adding a control/API/MCP capability.

## Verification

- Add focused tests for target selection, remote detection parity, trust
  refusal on an edited config, provider/OCI install routing, and the missing
  local-path action guard. Use fake command/target runners; no real mise,
  network, or live state DB is required.
- Run `cargo fmt --all -- --check`, `git diff --check`,
  `just quick thegn-host`,
  `cargo nextest run -p thegn-host mise_provider host_provision doctor hydrate`,
  and the relevant core detection/activation tests.
- Re-run the executable-name audit: every `mise` process construction remains
  in `mise_provider.rs`; `docs/api/control-v1.json` and completion snapshots
  remain byte-for-byte unchanged.

## Commit subject

`fix(host): route mise install through provider targets`
