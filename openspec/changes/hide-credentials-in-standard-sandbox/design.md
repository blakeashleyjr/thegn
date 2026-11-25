## Context

`standard` mounts `$HOME` read-only on bwrap and OCI (the host toolchain
substrate) and shares the host tree on systemd under
`ProtectHome=read-only`. The control-plane guard and the runtime-dir escape
masks (THE-2009) already lay masks over a bind after it is emitted, and the
escape planner (`sandbox_runtime_escape::plan_escape_masks`) translates a host
path through every bind that exposes it and skips a path that holds an
explicit mount.

## Decisions

- **Plan at resolution, mask at launch.** The resolver knows the profile, the
  worktree and the git common dir, so it lists the paths
  (`sandbox_hidden::standard_hidden_paths`) on the spec. The argv builders
  reuse the escape planner and mask encoders, so bwrap and OCI share one
  translation and one honour-explicit-mounts rule.
- **Mask, not omit.** `$HOME` stays one read-only bind; each hidden path gets
  an empty tmpfs (directory) or a read-only `/dev/null` bind (file). Rebuilding
  home from an allowlist is the `sealed` design and would break the shell.
- **`~/.ssh` by entry.** Hiding the whole directory also hides `known_hosts`
  and `config`, so git over ssh through the agent would fail host-key checks.
  Private keys and unknown entries are hidden; host keys, the client config
  and public keys stay.
- **Sibling repositories, not sibling folders.** Only entries with a `.git`
  (or a bare layout) in the parent of the worktree or of the main checkout are
  hidden. Dot-named entries are skipped, since a git-managed `~/.config` or a
  shell plugin clone is a dotfile, not a project.
- **systemd skips what would hide a bind.** `InaccessiblePaths=` over an
  ancestor of the worktree or a mount would hide them, so such a path is not
  hidden there (documented as a gap).
- **Agent runtime state only.** The carve lists the runtime entries Claude
  Code and Codex write plus the provider's login file; everything else in the
  config dir stays under the read-only `$HOME`.

## Risks

- A tool that needs a hidden credential fails in a standard pane. The user
  mounts it through `[sandbox] mounts` or a bundle identity (honoured), or
  uses `open`.
- A new runtime-state entry an agent version adds is read-only until listed.
