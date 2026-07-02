# Add proxy provider registry (single source-of-truth table)

## Summary

Give szproxy a single **declarative provider registry** — one `ProviderInfo`
table that is the source of truth for every backend's base URL, auth env var,
default model, and request transforms — from which both the config surface and the
client construction are derived. Modeled on
[`lumen`](https://github.com/jnsahaj/lumen)'s `genai`-backed `ProviderInfo` table,
which fans one declaration out to config prompts and client init.

## Impact

- **U/V/W** (LLM proxy track) — consolidates provider/model/route wiring behind one
  table, so adding or editing a provider is a single-place change.
- **AR 570** (tool-format translation) — the per-provider transform metadata lives
  in the same registry entry, keeping translation config beside its provider.
- Extends the `llm-proxy` capability. **No DB schema change** — the registry is
  config/compile-time data driving routing; audit logging (`proxy_requests`) is
  unchanged.

## Rationale

szproxy already routes and relays across backends with per-backend transforms
(`apply_transforms`: ensure-max-tokens, backend defaults, tool-message
compression), but backend metadata (URLs, env keys, default models, transforms) is
spread across the router and config. lumen shows the clean shape: a single
`ProviderInfo` table that _simultaneously_ populates the user-facing config and
constructs the client, so model/default/env-key drift is impossible — the config
and the runtime read the same table. This is a consolidation/refactor that also
makes "add a provider" a one-entry change and keeps AR 570's translation metadata
co-located with its provider.

## Non-goals

- **Changing routing/failover behavior** — the cascade/priority logic is unchanged;
  this only reorganizes where provider metadata comes from.
- **A remote/dynamic provider catalog** — the registry is local declarative data,
  not a fetched marketplace.
- **New audit/accounting** — `proxy_requests` logging is untouched (the fleet-view
  change owns metric extraction).
- **AI-free-shell dependency** — the registry is entirely within the proxy (AI
  layer); the shell does not depend on it.
