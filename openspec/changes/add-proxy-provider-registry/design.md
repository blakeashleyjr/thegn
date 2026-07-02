# Design

## The registry (proxy, declarative)

A single `ProviderInfo` entry per backend:

```
ProviderInfo {
  id,                 // "anthropic", "openai", "openrouter", ...
  base_url,
  auth_env,           // env var holding the key
  default_model,
  transforms,         // the per-provider transform set (AR 570)
}
```

The registry is a static/declarative table (a `providers()` function or config
section) that is the **one** place provider metadata lives. It is pure +
unit-tested: every entry has a non-empty id/base_url/auth_env; ids are unique;
looking up a known id resolves, an unknown id errors cleanly.

## Two consumers, one table

- **Config surface** — the `[proxy]`/route config is validated and completed
  against the registry (a route naming a provider inherits its default model and
  base URL unless overridden).
- **Client construction** — the router builds a backend client from the same
  `ProviderInfo` (base URL + `auth_env` key + default model), and `apply_transforms`
  reads the provider's `transforms` from its entry.

Because both read the same table, config and runtime cannot disagree.

## Invariants

- **Event loop**: none touched — this is proxy-internal wiring.
- **Render**: none.
- **State**: no `user_version` bump; `proxy_requests` logging unchanged.
- **Additivity**: entirely within the proxy (AI layer); no shell dependency. The
  registry has no tokio-on-loop concerns (it is data + lookup).

## Alternatives considered

- **Status quo (metadata spread across router + config)** — the source of the
  drift this change removes.
- **A fetched/remote provider catalog** — rejected as scope creep; local
  declarative data is enough and keeps the proxy hermetic.
- **Per-provider Rust modules** — heavier than a data table for what is mostly
  metadata; a table with a transform enum is simpler and testable.
