# Clamp the repo config overlay by trust, not specificity

## What Changes

thegn's config layers cascade most-specific-wins, and a repo-root
`.thegn.*` overlay is applied **last and unclamped** over the global/profile
sandbox config. Because that file is checked into a repository the user may have
cloned, this is a live sandbox-escape / code-exec-on-open hole: a hostile
`.thegn.toml` can set `enabled = false`, choose `backend`/`network = "host"`,
replace the egress allow-list, bind arbitrary host paths, pass through host env
tokens, and run `init_script`/`prepare` on the host.

This change splits config keys into **preferences** (papercut-class — keep the
most-specific-wins cascade) and **constraints** (breach-class — resolve by
_trust_: a more-trusted level sets a bound and less-trusted levels may only move
_inward_). The repo layer becomes a **clamped request**: constraints may only
tighten, additive requests (mounts, scripts, image, ports) are **trust-on-first-
use** gated, and every denial is surfaced (never silent). A new
`thegn config explain <key>` shows the effective value, the layer that set
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

## Delivery evidence

- Commit `2730bed1` landed the trust engine, launch-path clamp, TOFU store and
  CLI, and `config explain`; rename commit `91b4e0b4` carried the implementation
  to the current `thegn-*` paths.
- `thegn-core/src/config_resolve.rs` contains the exhaustive repo-field
  classification and hostile-repo regression suite; `repo_trust.rs` and
  `db_trust.rs` contain canonical identity and additive v32 migration tests;
  host `handlers/repo_trust.rs`, `cmd/repos.rs`, and `cmd/config.rs` apply,
  surface, approve/revoke, and explain the decisions.
- The sandbox and state-db requirements were already synchronized into the
  base specs by `507a46a1`. This delta is retained as the historical accepted
  scope and must be archived with spec application skipped, so later
  extensions to those base requirements are not overwritten.

## Why

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
