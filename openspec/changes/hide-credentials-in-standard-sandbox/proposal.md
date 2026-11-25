# Hide credentials in the standard sandbox

## Why

`standard` is the default profile. It binds `$HOME` read-only, which keeps
host files intact but leaves every secret in home readable: `~/.ssh` private
keys, cloud CLI logins (`~/.aws`, `~/.config/gcloud`, `~/.azure`, `~/.kube`),
token files (`~/.netrc`, `~/.git-credentials`, `~/.config/gh`,
`~/.docker/config.json`) and every other repository in the user's home. It
also carved the whole `~/.claude` (and `~/.codex`) read-write, so a pane could
rewrite the agent's settings and hooks that the host agent later runs. The
owner decision (2026-11-25) keeps `standard` as the default but tightens it.

## What Changes

- A local `standard` spec carries `hidden_paths`: the canonical host paths of
  well-known credential locations, every `~/.ssh` entry except `known_hosts`,
  `config`, `authorized_keys` and `*.pub`, and the git repositories beside the
  worktree and beside its main checkout (dot-named entries skipped; the
  worktree, main checkout and git common dir stay visible).
- bwrap and local podman/docker mask each hidden path a bind exposes, with an
  empty tmpfs for a directory or a read-only `/dev/null` bind for a file;
  systemd makes it inaccessible. A path at or above an explicit mount is kept.
  The policy fingerprint covers the list, so kept containers are recreated.
- `standard` no longer binds the link targets of `~/.ssh` identity keys; git
  over ssh uses the ssh agent.
- Agent config dirs (`~/.claude`, `~/.codex` or their env overrides) are no
  longer carved read-write wholesale. Only runtime state (sessions, project
  state, todos, session env, shell snapshots, logs, prompt history and the
  login file) is writable; settings, hooks, plugins and skills stay read-only.
  The bundle path's unmanaged-agent carve narrows the same way.
- The first-run wizard's sandbox step and the new-worktree dialog say what
  `standard` hides and what it does not stop.
- `sealed` is unchanged.

## Impact

- Code: `thegn-core` `sandbox_hidden` (new), `sandbox`, `sandbox_mounts`,
  `sandbox_policy_fp`, `sandbox_pin`, `bundle`; `thegn-host` `onboarding`,
  `wizard`.
- Docs: `sandboxing`, `sandbox-profiles`, `sandbox-security-model`,
  `known-limitations`.
- Behaviour: tools in a standard pane that read hidden credentials (cloud
  CLIs, `gh`, https git with `~/.netrc`, ssh with key files and no agent) fail
  until the user mounts the path explicitly or uses `open`.
