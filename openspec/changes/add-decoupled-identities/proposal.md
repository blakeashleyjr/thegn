# Add decoupled, per-tool identities

## Summary

Introduce named **identities**: a reusable, composable primitive that binds each
credential tool independently — git config, GitHub/forge config, GPG home, SSH
key — plus per-provider agent-account selection, **decoupled from any single
profile**. Profiles and env-bundles reference an identity **by name**, and each
tool is assignable independently (e.g. `git=washu`, `gh=personal`, `gpg=shared`,
`ssh=washu`). This generalizes the profile credential firewall — today hardcoded
to `<profile_root>/config/git`, `<profile_root>/config/gh`, `<profile_root>/gnupg`,
`<profile_root>/ssh/id` — and the bundle `config_dirs`/`accounts` fields into one
named, mix-and-match primitive with a switcher UI.

## Impact

- **H** (Profiles & subprofiles) — items **104** (per-profile key selection via
  bundle/account) and **105** (per-profile credential isolation): the profile
  firewall gains an `identity = "<name>"` indirection that resolves per-tool.
- **AU** (Environment bundles) — item **736** (`[bundle.<name>]` schema): bundles
  gain an optional `identity = "<name>"` reference, composed at any scope.
- Relates to **656** (interactive per-agent account/credential switcher): the
  identity switcher is the git/gh/gpg/ssh analogue of that account chip.

## Rationale

Identity today is **1:1 with the profile directory** — `profile::credential_env`
pins all four tools to `<profile_root>/...` with no indirection, so identities
cannot be reused across profiles or mixed per tool. Yet the seam already exists:
env-bundles (`bundle.rs`, `[bundle.<name>]`) redirect config dirs via
`config_dirs` and pick agent accounts via `accounts` at any scope with a defined
precedence (global → workspace → worktree), and `account.rs` manages multiple
logins per provider. What is missing is a **named, per-tool identity** that both
profiles and bundles reference, each tool independently assignable, plus a UI to
switch it. We ship this incrementally: (A) expose the existing bundle/account
layers with a switcher; (B) converge on the decoupled `[[identities]]` table
referenced by both profiles and bundles.

## Non-goals

- Building the comms subsystem (540) or the proxy per-agent account chip (656)
  themselves — identities are the credential-tool primitive they can consume.
- Secret storage: identities point at **existing** on-disk config dirs/keys;
  secret _values_ still resolve via the existing `env:`/`file:` schemes.
- Moving worktrees or per-profile state (separate concerns).
