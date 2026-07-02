# Tasks

## 1. Registry (superzej-proxy)

- [ ] 1.1 Define `ProviderInfo { id, base_url, auth_env, default_model, transforms }`
      and a `providers()` registry table + `lookup(id)` — **unit tests**: every
      entry has non-empty id/base_url/auth_env, ids are unique, known id resolves,
      unknown id errors cleanly.

## 2. Derive config + client from the registry (superzej-proxy)

- [ ] 2.1 Validate/complete the `[proxy]`/route config against the registry (a
      route inherits its provider's default model + base URL unless overridden) —
      **unit tests**: inheritance applies, explicit override wins, route naming an
      unknown provider errors.
- [ ] 2.2 Construct the backend client and resolve `apply_transforms` from the same
      `ProviderInfo` entry, removing the duplicated per-backend metadata from the
      router.

## 3. Docs + validate

- [ ] 3.1 Document the provider registry + how a route inherits provider defaults
      in `config/config.toml.example` + the proxy doc section.
- [ ] 3.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
