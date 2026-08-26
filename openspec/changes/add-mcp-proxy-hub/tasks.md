# Tasks — MCP proxy hub

> Implementation status (uncommitted, for review). `[x]` done, `[~]` partial /
> deferred with a note. **Key deviation:** v1 ships the standalone **in-process**
> hub as the working `thegn mcp proxy` path (the spec-sanctioned fallback, used
> both when the daemon is on and off); the **daemon-owned shared-upstream
> multiplex** (one child shared across agents' shims over a control-IPC bridge)
> is the identified follow-up — everything it needs already exists in core
> (`route::IdRewriter`, `reconcile`, `breaker`) and the daemon already carries
> `mcp_proxy.status`/`reload`. Also depends on `add-credential-broker`'s
> `SecretStore`/`redact` seam, which had **not landed** at this base, so the seam
> is built here to that change's shape (reconcile whichever lands second).

## 1. Core: aggregation, filtering, routing (thegn-core)

- [x] 1.1 `mcp/proxy/aggregate.rs`: merge upstream `tools/list` under
      `<upstream>__<tool>`; merged `initialize` (`initialize_result`); pure over
      `serde_json::Value`; tests incl. name-charset + collision cases.
- [x] 1.2 `mcp/proxy/filter.rs`: default-deny `proxy.tools` glob eval (reuses
      the grants glob matcher); exposed/hidden partition; unset ⇒ nothing.
- [x] 1.3 `mcp/proxy/route.rs`: namespaced call → route (table lookup, never a
      re-parse); JSON-RPC error for unknown/filtered; per-connection
      `IdRewriter`.
- [x] 1.4 Unit tests for 1.1–1.3 (coverage lives on this pure logic).

## 2. Core: breaker, reconcile, partition (thegn-core)

- [x] 2.1 `mcp/proxy/breaker.rs`: clock-injected Closed/Open/HalfOpen; threshold + cooldown from config; exhaustive table tests.
- [x] 2.2 `mcp/proxy/reconcile.rs`: (old, new) → start/stop/restart/refilter
      plan; tests for add/remove/edit/refilter (+ partition-change = start+stop).
- [x] 2.3 `mcp/proxy/partition.rs`: scope-key derivation + placeholder
      expansion; unresolvable ⇒ withheld (typed reason, never literal braces).

## 3. Config (thegn-core)

- [x] 3.1 `[mcp_proxy]` (`McpProxyConfig`) + `[mcp_servers.<name>.proxy]`
      (`McpProxyExposure` + `ProxyScope`). Validation is schema-driven (the
      `ProxyScope` `config_enum!` is strict-checked by construction; pin bumped
      71→72).
- [x] 3.2 Documented in `config/config.toml.example`.
- [x] 3.3 Round-trip covered by `mcp/config.rs` tests + the schema walker.

## 4. Secret store seam (core vocabulary + host impl)

- [x] 4.1 `thegn_core::secret::{SecretRef, SecretStore, SecretStoreKind,
SecretStoreError}` — object-safe, Probe, `keyring:` parse beside
      `env:`/`file:`, `exec` reserved; pure tests. `thegn_core::redact` canonical.
- [~] 4.2 Host keyring impl (`KeyringStore` in `thegn-host/src/secret.rs`, over
  the existing layered store + a names-only index). Secret Service / Keychain
  / Credential Manager ride the `keyring` crate already in-tree; no per-OS
  `#[cfg]` added. macOS/Windows unverified from here.
- [x] 4.3 `thegn mcp secret set|rm|list` (shared `thegn` service; `list` names
      entries via the sidecar index, never values; `set` reads stdin).
- [x] 4.4 Doctor keyring-backend Probe row.

## 5. Daemon upstream supervisor (thegn-host)

- [~] 5.1 Upstream child process + stdio pump + env-ref resolution at spawn +
  `wrap_background_argv` (`thegn.slice`) — implemented as
  `mcp_proxy::upstream::Upstream`, **owned by the in-process hub** (the shim).
  Daemon-owned shared instances are the follow-up (see header).
- [x] 5.2 Circuit breaker drives fast-fail while open; no advertised-table churn
      on trips (breaker trips never change `tools/list`). Health-tick is the
      pure breaker + `Upstream::health_check`; a daemon timer task is folded in
      with the shared-upstream follow-up.
- [~] 5.3 Reconcile-on-reload via `daemon_reload` (pure `reconcile` diff, config
  re-read); `tools_changed` reported. Heartbeat mtime auto-reload +
  `notifications/tools/list_changed` push ride the shared-upstream path.
- [x] 5.4 `mcp_proxy.status` / `mcp_proxy.reload` control ops (status includes
      scope + withheld reasons; config-reflective in v1).

## 6. Shim + standalone fallback (thegn-host)

- [x] 6.1/6.2 `thegn mcp proxy` stdio shim over the in-process hub; worktree
      context resolved from cwd; degrade-never-brick (withheld/failed upstream ⇒
      its tools absent, endpoint still serves). This IS the standalone fallback.
- [x] 6.3 Smoke coverage (`test/smoke.sh`): proxy end-to-end against a stub
      stdio MCP server — namespacing + default-deny filter + routing + refusal
      asserted; wire secret-free; emit --proxy; presets; doctor section.

## 7. Catalog + surfaces

- [x] 7.1 `Verb::McpProxyStatus` (read) / `McpProxyReload` (write); catalog rows
      on Cli/Http/Grpc (Grpc gapped); ROUTES + API_CALLS + `ControlApi` +
      `DaemonService` + client wired so coverage tests pass.
- [x] 7.2 `thegn mcp status` (`--json`) / `thegn mcp reload` CLI verbs.

## 8. Wiring + presets

- [x] 8.1 `thegn mcp wire [--agent <kind>] [--all] [--remove]`: per-vendor
      adapters (claude/cursor/windsurf/vscode/zed/gemini/amp; codex=TOML,
      hand-wire hint); pure marker-tagged merge in `thegn_core::mcp::wire`
      (idempotent, refuses to touch unmarked entries) with unit tests. Vendor
      file paths are best-effort; the merge semantics are the tested guarantee.
- [x] 8.2 `thegn mcp emit --proxy` — the single secret-free entry (no env). The
      legacy `emit` now warns when it would copy env into agent settings.
- [x] 8.3 Embedded presets + `thegn mcp preset list|show [--write]`
      (print-first, append-only); ≥1 fully-local no-API-key memory preset; test
      asserts each parses with grants + proxy exposure and carries no literal
      secret.
- [x] 8.4 Doctor per-upstream live Probe (handshake, exposed/hidden counts,
      scope).

## 9. Docs

- [x] 9.1 `docs/cli.md` `mcp proxy` section + `config/config.toml.example`
      `[mcp_proxy]`/`proxy` keys.
- [x] 9.2 The mcp-emit env-copy caveat is documented (cli.md + the `emit`
      warning); direct `emit` stays the escape hatch.

## 10. Gate

- [~] 10.1 `just ci` — NOT run (full-workspace gate refused by the dev-loop
  hook + heavy CPU contention from concurrent worktrees). Scoped
  `cargo check` used; run `just ci` before PR.
