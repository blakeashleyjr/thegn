# Tasks — MCP proxy hub

## 1. Core: aggregation, filtering, routing (thegn-core)

- [ ] 1.1 `mcp/proxy/aggregate.rs`: merge upstream `tools/list` replies under
      `<upstream>__<tool>` names; merged `initialize` capabilities. Pure over
      `serde_json::Value`; unit tests incl. name-charset and collision cases.
- [ ] 1.2 `mcp/proxy/filter.rs`: default-deny `proxy.tools` glob evaluation
      (reuse the grants glob matcher semantics); exposed/hidden partition of
      an upstream's advertised tools; tests for unset-list ⇒ nothing.
- [ ] 1.3 `mcp/proxy/route.rs`: namespaced call → (upstream, tool); JSON-RPC
      error for unknown/filtered names; per-connection id rewriting tables.
- [ ] 1.4 Unit tests to the 95% core gate for 1.1–1.3.

## 2. Core: breaker, reconcile, partition (thegn-core)

- [ ] 2.1 `mcp/proxy/breaker.rs`: clock-injected Closed/Open/HalfOpen state
      machine with threshold + cooldown from config; exhaustive table tests.
- [ ] 2.2 `mcp/proxy/reconcile.rs`: (old effective config, new) → per-instance
      start/stop/restart/refilter plan; tests for add/remove/edit/refilter.
- [ ] 2.3 `mcp/proxy/partition.rs`: scope key derivation + `{workspace}`/
      `{worktree}`/`{repo_root}`/`{branch}` expansion; unresolvable ⇒ withheld
      (typed reason, never literal braces); tests.

## 3. Config (thegn-core)

- [ ] 3.1 `[mcp_proxy]` table: `enabled`, `health_interval_secs`, breaker
      thresholds/cooldown; `[mcp_servers.<name>.proxy]` subtable: `tools`,
      `scope`. Exhaustive destructure in config validation.
- [ ] 3.2 Document every key in `config/config.toml.example`.
- [ ] 3.3 Extend config round-trip/validation tests (scope enum via
      `config_enum!`).

## 4. Secret store seam (core vocabulary + svc impls)

- [ ] 4.1 `thegn_core::secret::SecretStore` seam: object-safe trait, Probe,
      `keyring:` ref parsing beside `env:`/`file:`; pure tests.
- [ ] 4.2 Platform impls behind the seam (Secret Service / Keychain / Windows
      Credential Manager; `reserved` where unimplemented) in svc/host platform
      code — no `#[cfg]` outside `platform/` (platform ratchet).
- [ ] 4.3 `thegn mcp secret set|rm|list` (namespaced service id; `list` names
      entries, never values).
- [ ] 4.4 Doctor: keyring backend Probe row.

## 5. Daemon upstream supervisor (thegn-host)

- [ ] 5.1 Daemon module: spawn/own upstream instances per (server, partition
      key); stdio pump; env-ref resolution at spawn; argv through
      `wrap_background_argv` (`thegn.slice`).
- [ ] 5.2 Health ticker (daemon tokio task) driving breaker transitions;
      fast-fail errors while open; no advertised-table churn on trips.
- [ ] 5.3 Reconcile-on-reload + heartbeat mtime check; emit
      `notifications/tools/list_changed` after table changes.
- [ ] 5.4 Control-plane ops: `mcp_proxy.status` / `mcp_proxy.reload` service
      handlers (status includes withheld reasons, breaker states).

## 6. Shim + standalone fallback (thegn-host)

- [ ] 6.1 `thegn mcp proxy` stdio shim: connect daemon over control IPC,
      per-connection id rewrite, worktree-context resolution from cwd.
- [ ] 6.2 Standalone fallback: no daemon ⇒ in-process upstreams via the same
      core logic; degrade never brick.
- [ ] 6.3 Smoke-test coverage (`test/smoke.sh`): proxy end-to-end against a
      stub stdio MCP server, filter + namespacing asserted.

## 7. Catalog + surfaces

- [ ] 7.1 New Verbs + catalog rows `mcp_proxy.status` (read) /
      `mcp_proxy.reload` (write), surfaces Cli/Http/Grpc; ROUTES + gRPC + CLI
      tables wired so the coverage tests pass. Sequence after the in-flight
      write-MCP-tools branch (SURFACE_GAPS retirement) merges.
- [ ] 7.2 `thegn mcp status` / `thegn mcp reload` CLI verbs (`--json` on
      status).

## 8. Wiring + presets

- [ ] 8.1 `thegn mcp wire [--agent <kind>] [--remove]`: per-vendor adapter
      files (claude/codex/cursor/windsurf/vscode/zed/amp/gemini as
      implementable; vendor paths only in impl files), marker-tagged
      idempotent merge, refusal to touch unmarked entries; unit tests on the
      pure merge.
- [ ] 8.2 `thegn mcp emit --proxy` variant emitting the single secret-free
      proxy entry.
- [ ] 8.3 Embedded presets + `thegn mcp preset list|show [--write]`
      (print-first; append-only write); include ≥1 fully-local memory preset;
      test asserting every preset parses as valid config with grants present.
- [ ] 8.4 Doctor: per-upstream Probe (handshake, exposed/hidden counts,
      scope).

## 9. Docs

- [ ] 9.1 `docs/cli.md` + `docs/help/` update for the new `mcp` verbs and the
      `[mcp_proxy]` / `proxy` subtable config (config-reference page is
      generated — doc comments + example entries carry it).
- [ ] 9.2 Update the mcp-servers capability docs note that direct `emit`
      copies env into agent settings while the proxy path does not.

## 10. Gate

- [ ] 10.1 Run `just ci` once at the end (includes openspec validate, lint
      ratchets, coverage on core).
