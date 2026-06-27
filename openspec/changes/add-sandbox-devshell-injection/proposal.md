# Add sandbox dev-shell injection

## Summary

Make a repo's `nix develop` devShell toolchain available **out of the box**
inside sandboxed worktree panes (and to contributors), without relaxing sandbox
isolation by default. Two tiers: **Tier A** (always-on, read-only) resolves the
already-built devShell on the host and injects its exported env (PATH first) into
panes — no store writes, no daemon, zero isolation cost; **Tier B** (opt-in) binds
the nix daemon socket so full `nix develop`/`build`/`fmt` work inside the sandbox.

Source design: `docs/superpowers/specs/2026-06-26-sandbox-devshell-injection-design.md`.

## Impact

- **AE** (container provisioning / devshell, item 359) — devShell tooling in sandboxes.
- Extends the **sandbox** capability.

## Rationale

Sandboxed panes (and automated agents) currently lack `shellcheck`/`yamllint`/
`taplo`/`cargo-llvm-cov`, so `just lint`/`coverage`/`nix fmt` silently skip; you
cannot `nix develop` inside the sandbox (read-only store, no daemon). Resolving on
the host (which has full nix) and injecting the result is the zero-isolation-cost
fix.

## Non-goals

- direnv as a required mechanism (a committed `.envrc` is convenience only).
- Resolving devShells for remote worktrees (v1 skips, logs).
- Per-pane devShell selection (one devShell per repo for v1).
