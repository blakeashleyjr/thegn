# Design — decoupled per-tool identities

## The identity primitive

A named identity is a set of independent, optional per-tool bindings. Unset
tools fall through, so users can mix git from one identity, GitHub config from
another, and a shared GPG home:

```toml
[identities.washu]
accounts = { claude = "washu" }

[identities.washu.git]
config = "~/.config/git/washu"
ssh_key = "~/.ssh/id_washu"

[identities.washu.gh]
config = "~/.config/gh-washu"

[identities.washu.gpg]
home = "~/.gnupg"
```

`Config::identities` is a `BTreeMap<String, IdentityConfig>` with nested git,
GitHub, and GPG records plus provider accounts. Each binding maps to an
environment variable the existing profile firewall already owns:
`GIT_CONFIG_GLOBAL`, `GIT_SSH_COMMAND`, `GH_CONFIG_DIR`, or `GNUPGHOME`.
Identities add indirection, not a second credential path.

## Resolution and fallback

Identity resolution happens in the existing pane-spawn composition seam,
`bundle::compose`; `profile::credential_env` continues to install the
profile-root fallback. For each tool, composition applies:

1. the active profile's `[profiles.<p>].identity` as the lowest-precedence
   identity base;
2. identities referenced by active bundles in their existing scope order;
3. directly bound identities in global → workspace → worktree order.

Each layer overrides only the tools it sets. Unset tools fall through to a
less-specific identity and finally to the profile-root paths, preserving
identity-less behavior byte-for-byte. A bundle's explicit `env`,
`config_dirs`, and `accounts` fields remain above its referenced identity.

Firewall invariants remain unchanged: identities never inject forge tokens,
`GIT_SSH_COMMAND` forces `IdentitiesOnly=yes`, and unknown identity names warn
and are skipped. Existing identity directories are added to
`ResolvedEnv.mounts` read-write and path-preserving; the existing
`profile::sandbox_cred_mounts` continues to cover profile-root fallbacks.

## Switcher UI

The `switch-identity` palette lists `[identities.<name>]` plus a clear row and
marks the effective direct binding. Selection writes advisory `ui_state`: at
workspace scope when a repository is focused, otherwise at global scope. The
underlying core resolver also supports worktree bindings for internal callers,
but this change did not ship a worktree-scope selector in the UI.

The selection affects subsequently spawned panes; existing processes retain
their existing environment. The UI reports `Identity → <name>` or
`Identity cleared` through the transient status line. No persistent identity
chip was added.

## Event loop, storage, and compatibility

The switcher and status update use the existing overlay/chrome `Full` frame.
Identity resolution is on the already off-loop pane-spawn path, with no timer,
polling, or new wake source. Direct bindings reuse the `ui_state` KV, so there
is no SQLite schema change or `user_version` bump.

With no `[identities.<name>]` tables and no profile/bundle identity references,
the existing profile credential paths and pane environments are unchanged.
This makes adoption additive and migration-free.

## Help

The existing command-palette help page claims `switch-identity` and describes
the named, per-tool binding and subsequent-pane behavior. No new help context
or standalone identity page is required.

## Alternatives considered

- Extending `Bundle` alone was rejected because a pane-scoped bundle cannot
  express the reusable profile-level firewall base. Explicit bundle fields
  remain available as the raw override.
- Identity subprofiles were deferred because live subsystem teardown/rebind is
  a different lifecycle problem. This change only resolves identity for new
  pane processes.
