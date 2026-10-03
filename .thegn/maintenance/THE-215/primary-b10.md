# THE-215 — primary instructions: ADVERSARIAL SECURITY REVIEW (batch 10)

Review `git diff main...HEAD` on this branch (the implementation commit is a65b63b9f "fix(sandbox):
sealed tiers hide host HOME behind a private ..."). It has NOT been compiled yet — flag likely compile
errors. Do not change production code; write findings only.

Issue (data): sealed / sealed-tunnel sandboxes mounted the whole host $HOME read-only (bwrap/OCI via
host_toolchain_mounts_ro_home in crates/thegn-core/src/sandbox_mounts.rs; systemd ProtectHome=read-only),
exposing ~/.ssh, ~/.aws, ~/.gnupg, agent auth homes, thegn state to a hostile agent. Also found:
default_writable_carveouts made ~/.local/state and ~/.local/share READ-WRITE for sealed, and
ssh_creds::identity_mounts bound ~/.ssh key targets into sealed panes.

The user (Blake) chose this design — verify it is implemented exactly:

- Option A: SEALED tiers only get a private HOME (tmpfs) + a read-only allowlist: .gitconfig,
  .config/git, the user's real shell rc files (.zshrc etc. — resolved targets), .config/starship.toml,
  .terminfo, and the existing auto caches. Allowlist entries are resolved with canonicalize and REJECTED
  if they resolve into a deny list (.ssh, .gnupg, .aws, .secrets, .config/gh, keyrings, browser
  profiles, .claude\*, thegn state). Sealed drops the .local/state/.local/share RW carve-outs and the ssh
  identity mounts. systemd: ProtectHome=tmpfs + BindReadOnlyPaths for the allowlist. A backend that
  cannot hide HOME FAILS the sealed isolation floor (sandbox_floor.rs). hardened/open UNCHANGED and
  documented as integrity-only.
- User extension reuses the existing `[sandbox] mounts` (NO new config key). HOME visibility
  (hidden / read-only / writable) is reported in `thegn doctor` ONLY (no control-API change).

Hunt for: any path where a secret still becomes readable (symlink in an allowlisted entry pointing into
a denied dir, a directory allowlist entry containing a secret subdir, rc files sourcing secret files —
note but don't block; `[sandbox] mounts` user entries bypassing the deny list — intended?), TOCTOU
between canonicalize and mount, the OCI/Apple-container/systemd backends each matching bwrap, the floor
failing closed for unsupported backends, pipeline env (bwrap) still able to run git, the shell and the
worktree, tests using only SYNTHETIC HOME fixtures (never real credentials), docs/help updated, platform #[cfg] only under platform/ or pinned files, ignored-Result ratchet. Verdict source-review-clear or
revisions-needed with precise file:line findings and concrete exploit scenarios.
