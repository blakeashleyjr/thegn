# Add environment bundles

## Summary

Add named **environment bundles** — composable units of env vars + credential/
config-dir redirection + optional dotfiles + per-provider account selection — that
**bind at any scope** (global / workspace / worktree) and inject at the existing
pane-spawn seam. This is the "missing middle" between `account.rs` (one var, one
provider) and heavyweight process profiles (group H): "work vs personal" within a
single running process, applied to **every** pane (shells too, not just agents).

Source design (design-only): `docs/superpowers/specs/2026-06-22-env-bundles-design.md`.

## Impact

- **AU** (environment bundles) — the whole group.
- Generalizes `account.rs`; relates to **H** (heavyweight profiles, the hard
  firewall) and item 656 (account switcher chip).

## Rationale

`account.rs` already proves per-scope binding for one credential-home var; bundles
generalize that mechanism (arbitrary env, config-dir redirection, dotfile tiers,
secret resolvers) and run every pane through one `env::compose()` seam. "Multiple
Claude profiles" falls out as the first consumer.

## Non-goals

- Replacing the heavyweight-profile firewall (group H) — bundles are soft isolation.
- Auto-loading a repo `.env` (opt-in, allowlisted, credential-key-filtered).
