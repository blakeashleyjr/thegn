# Clamp the repo config overlay by trust, not specificity

## Summary

superzej's config layers cascade most-specific-wins, and a repo-root
`.superzej.*` overlay is applied **last and unclamped** over the global/profile
sandbox config. Because that file is checked into a repository the user may have
cloned, this is a live sandbox-escape / code-exec-on-open hole: a hostile
`.superzej.toml` can set `enabled = false`, choose `backend`/`network = "host"`,
replace the egress allow-list, bind arbitrary host paths, pass through host env
tokens, and run `init_script`/`prepare` on the host.

This change splits config keys into **preferences** (papercut-class — keep the
most-specific-wins cascade) and **constraints** (breach-class — resolve by
_trust_: a more-trusted level sets a bound and less-trusted levels may only move
_inward_). The repo layer becomes a **clamped request**: constraints may only
tighten, additive requests (mounts, scripts, image, ports) are **trust-on-first-
use** gated, and every denial is surfaced (never silent). A new
`superzej config explain <key>` shows the effective value, the layer that set
it, and the clamp trace.

## Impact

- **O (configuration)** — adds constraint-vs-preference merge semantics below the
  profile level; the global/profile/env/`--set` layers are byte-for-byte
  unchanged (no compat break in trusted layers).
- **AB / sandbox capability** — `Config::repo_sandbox` / `resolve_env` now clamp
  the repo overlay via a pure engine (`config_resolve`); backend selection and
  bind-mount model are otherwise unchanged.
- **state-db** — adds a `repo_trust` table (schema v32) recording approved gated
  requests, keyed by canonical request JSON.
- **AJ / capability-grants** — trust-on-first-use reuses the grant deny-reason
  vocabulary; no change to `grants.rs`.

Extends the `sandbox` and `state-db` capabilities.

## Rationale

The specificity gradient (global → profile → repo) runs _opposite_ to the trust
gradient: repo config is the least-trusted authorship layer (cloned, and a slice
may be agent-authored at runtime). So preferences want most-specific-wins while
constraints want most-trusted-wins. Encoding the merge class per key — override /
ceiling / floor / accumulate — makes the resolution rule uniform and testable,
and turns "repo overrides sandbox" into "repo requests within the sandbox
bound", which is the only safe reading. Trust-on-first-use (à la `direnv allow` /
editor workspace-trust) keeps the additive escape hatch usable without making a
clone a consent dialog for sandbox escape.

## Non-goals

- **Zones** — the new trust level between profile and workspace is a separate
  change (`add-workspace-zones`); this change only reserves the slot.
- **Per-key `enforced` lock syntax** — merge class is a static property of each
  key in code; no TOML lock keyword in v1.
- **Model-alias resolution / meta-audit** — deferred; no alias system exists yet.
- **Network egress enforcement mechanism** — unchanged (the per-container DNS
  filter); this change only parameterizes it (deny-all encoded as a `*` block).
