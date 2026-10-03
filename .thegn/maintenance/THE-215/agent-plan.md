# THE-215 plan: needs-decision (no code changed)

## Verified on main (1343726b6)

Issue is current. `sandbox.rs:911` home_ro = is_oci || read_only_root(); `sandbox.rs:928/934`
append `host_toolchain_mounts_ro_home`, which (`sandbox_mounts.rs:128-135`) binds the whole
$HOME. `read_only_root()` is true for hardened/sealed/sealed-tunnel (`config.rs:565`), so sealed
shares hardened's wholesale-HOME view. Worse than the issue states: `default_writable_carveouts`
(`sandbox_mounts.rs:201`) makes `~/.local/state` and `~/.local/share` READ-WRITE for sealed too
(thegn state DB, keyrings, atuin, etc. live there), and `ssh_creds::identity_mounts`
(`sandbox.rs:981`) binds ssh key targets into sealed panes.

## Why this is a product decision

Changes security policy semantics (what sealed can see), and the issue's acceptance wants
a doctor/capability "HOME visibility" property (hidden/ro-visible/writable), which touches the
capability surface and likely a config key for the allowlist. Allowlist contents decide whether
the user's real shell/starship/git config work in sealed panes (pipeline env relies on bwrap).

## Options

A. Private HOME for sealed tiers only (recommended). bwrap: `--tmpfs $HOME`, then ro-bind an
explicit allowlist (resolved via canonicalize, reject targets under a deny list / outside
approved roots such as /nix/store and the HOME itself): .gitconfig, .config/git, .zshenv/.zshrc
(symlinks into /nix/store), .config/starship.toml, .config/direnv?, .terminfo, plus the existing
auto_cache_mounts and the worktree. Deny always: .ssh, .gnupg, .aws, .secrets, .config/gh,
.config/{gcloud,doctl,op}, .netrc, .docker, .kube, .mozilla, .config/{BraveSoftware,google-chrome},
.local/share/keyrings, .local/state/thegn, .claude\*, .codex, agent auth homes. Drop sealed's
`.local/{state,share}` carve-outs (keep tmpfs-backed history inside $HOME tmpfs). Hardened/open
unchanged (documented as integrity-only). OCI: same via tmpfs home + allowlist; systemd:
`ProtectHome=tmpfs` + BindReadOnlyPaths for the allowlist; Apple container: no host HOME mount
(already not injected, same_abi false) - assert in a test; backends that cannot hide HOME
fail the sealed isolation floor (`sandbox_floor.rs`).
B. Deny-list over ambient HOME (bwrap `--tmpfs` over each secret dir). Smaller diff, but fails
open on any credential path not enumerated (issue explicitly asks deny-by-default). Not advised.
C. Make hardened also private, sealed identical. Cleanest model, but breaks users relying on
dotfile visibility in hardened; larger blast radius.

## Decisions needed from Blake

1. Option A vs C.
2. Allowlist contents (above is a draft); is user shell config (.zshrc etc.) expected in sealed?
3. New config key for user-extensible allowlist (trips 3 config ratchets) or reuse `[sandbox] mounts`
   (already honoured after the home mounts, so extra entries work without a new key).
4. New doctor/capability field `home_visibility` (hidden|ro|rw): control-API snapshot update.

## Implementation sketch once decided (~400-600 lines)

- pure fn `sealed_home_mounts(home, &Probe) -> Vec<Mount>` + `HomeView` enum in sandbox_mounts.rs;
  canonicalize + deny-list check (unit tests, synthetic HOME in tempdir, symlink-into-.ssh case).
- sandbox.rs: branch on `profile.hides_home()`; skip ro $HOME + local-state carve-outs + ssh identity mounts.
- backend argv tests for bwrap/OCI/systemd; floor test for unsupported backend.
- real bwrap integration test with synthetic HOME sentinels (skips if bwrap missing).
- doctor line + capability field; docs + help page.

## Residual window (review fixes)

Allowlist symlink targets are re-validated when the bwrap argv is built
(`sandbox_mounts::sealed_mount_still_ok`, fail-safe omit). A retarget between that
check and bwrap's own mount setup remains possible (no fd-based bind); OCI/systemd
validate at resolution only. Accepted: it requires an attacker already writing the
user's HOME dotfile symlinks.
