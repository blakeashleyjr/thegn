# Tasks — resurrect-model-proxy

Reference implementation: git history at `85f3d1fb^` (`crates/thegn-proxy/`,
`crates/thegn-core/src/proxy/`, `crates/thegn-host/src/{proxy_daemon,cmd/proxy}.rs`).
Restore module-by-module behind compiling seams; do not one-shot revert the
excision commits. Iterate with `just quick <crate>` + targeted
`cargo nextest run -p <crate> <substring>`; the heavy gates run once at the end.

## Phase 1 — pure core (`thegn-core::proxy`, 95% gate)

- [x] 1.1 Restore `proxy/classify.rs` (Serve/Soft/Exhausted, error-body
      keyword matching) with its unit tests; adapt to current core style.
- [x] 1.2 Restore `proxy/cost.rs` (PricePoint/PriceTable/Usage, subscription
      $0 source) + unit tests; extend `Usage` with cache-read/cache-creation
      token fields.
- [x] 1.3 Restore `proxy/creds.rs` (CredPool, roundrobin/failover/random/
      weighted lane ordering) + unit tests.
- [x] 1.4 Restore `proxy/ratelimit.rs` (per-identity token bucket,
      non-blocking `try_take`/`reserve`) + unit tests.
- [x] 1.5 Restore `proxy/transform.rs` (ensure_max_tokens, backend defaults,
      estimated_request_tokens, context-limit checks) WITHOUT the
      `compress.rs` dependency (token reduction is out of scope) + unit tests.
- [x] 1.6 Restore `proxy/stats.rs` rollups against the new
      `ModelProxyRequestRow` shape + unit tests (spend, tokens/sec, p50/p95,
      per-backend/route/scope).
- [x] 1.7 NEW `proxy/route_select.rs`: deterministic `auto` tier classifier
      over request features (estimated tokens, tools, streaming, mode hint) —
      exhaustive unit tests proving determinism and precedence (alias > auto).
- [x] 1.8 NEW `proxy/usage_order.rs`: usage-aware lane ordering over an
      `AccountUsage` snapshot (warn ⇒ deprioritize, crit ⇒ skip, always ≥1
      eligible) — unit tests incl. the all-throttled degradation.
- [x] 1.9 Confirm `core::backoff` is reused (no copy) for restart pacing.

## Phase 2 — config

- [x] 2.1 `config_model_proxy.rs`: `[model_proxy]` section — enabled/listen/
      routing/usage_aware/timeouts, `[[model_proxy.providers]]` registry
      (kind implemented-or-reserved via `config_enum!`, SecretRef-only
      `api_key`/`api_keys`, key_strategy, rpm/burst/inflight/context_limit/
      pricing/subscription/defaults), `[[model_proxy.routes]]` +
      `[model_proxy.aliases]` + `[model_proxy.budget]`. Unit tests: parse,
      defaults, layering, trust clamp.
- [x] 2.2 Validation: raw-literal `api_key` fails `thegn config validate` and
      proxy start with a pointed diagnostic; non-loopback `listen` warns;
      route referencing an unknown provider fails. Unit tests.
- [x] 2.3 Verify the stale-`[llm_proxy]` warn-and-drop path is unchanged
      (regression test: `[llm_proxy]` present + `[model_proxy]` absent ⇒
      proxy disabled, one config_warn).
- [x] 2.4 Document every key in `config/config.toml.example`; confirm the
      generated config-reference help page picks the section up.

## Phase 3 — DB

- [x] 3.1 `db_model_proxy.rs` + store trait: `model_proxy_requests` and
      `model_proxy_budget_state` tables, additive migration, `user_version`
      bump; row structs metadata-only (no content columns exist to misuse).
- [x] 3.2 Migration tests: older DB gains tables; orphaned legacy `proxy_*`
      tables untouched; accounting survives restart.

## Phase 4 — the daemon crate (`crates/thegn-proxy`)

- [x] 4.1 Revive the crate skeleton (Cargo.toml, lib/main, server, state,
      shared) bound to current workspace deps; listen-socket-is-the-lock
      single-instance behavior.
- [x] 4.2 `ModelUpstream` seam trait (object-safe, BoxFuture, caps bits,
      Probe) with `AnthropicUpstream` + `OpenAiCompatUpstream` impls; vendor
      quirks only in impl files.
- [x] 4.3 Restore router.rs (slot/lane ordering, cascade, cooldown, health,
      last_resort) wired to the Phase 1 pure modules + `cost_aware` strategy + optional usage-aware ordering input.
- [x] 4.4 Restore relay.rs / anthropic_stream.rs (streaming passthrough,
      first-byte peek, idle kill, heartbeats, OpenAI⇄Anthropic translation).
- [x] 4.5 Accounting write path: audit row per request incl. cache tokens;
      assert-no-content review of every tracing statement; the
      dev-only body-log env gated out of release builds.
- [x] 4.6 Budgets: restore budget.rs windows/anchors onto
      `model_proxy_budget_state`; `on_breach` warn/refuse/downgrade.
- [x] 4.7 `/stats` + `/healthz` endpoints from the shared stats rollup.
- [ ] 4.8 Restore/adapt the crate's e2e test suite (`tests/e2e.rs` in
      history): failover, cooldown, streaming fall-through, alias/auto
      routing, budget refuse, subscription $0 accounting. (Deferred: the
      module-level unit tests cover the pure routing/failover/budget/auto/
      usage logic; the mock-server e2e harness is a follow-up.)

## Phase 5 — host integration

- [x] 5.1 `model_proxy_daemon.rs`: launch/supervise off the UI loop,
      `core::backoff` restarts, graceful stop, status-line warnings via the
      existing channel + waker path.
- [x] 5.2 `cmd/proxy.rs`: `thegn proxy status|stats|start|stop` (+ hidden
      `serve`), `--json` for status/stats; cli_help entries.
- [x] 5.3 Capability catalog: four `model_proxy.*` Verb rows, OPERATOR
      surfaces, `required_scope` (read/read/admin/admin); per-surface
      coverage tests green with no new SURFACE_GAPS entries.
- [x] 5.4 `thegn doctor` probe: enabled/listen/reachability, per-provider
      kind + SecretRef resolvability (never values), non-loopback warning,
      quiet single line when disabled.
- [x] 5.5 Agent env injection: `route_via_proxy` on `[[agents]]`/`[[tools]]`
      entries, per-worktree attribution key mint, probe-before-inject,
      skip-with-warning when down. Unit tests for the env assembly.
- [x] 5.6 Usage surfaces: proxy-spend block in System ▸ Usage +
      usage dashboard (off-thread hydrate on the usage cadence, Full damage);
      budget warn alerts through the usage-alert path; block absent when
      disabled; disabled under `THEGN_E2E`.
- [x] 5.7 Decide + implement the sandbox stance from design Open Questions
      (wrap `tgproxy` in `thegn.slice` via `wrap_background_argv` or document
      why not); `thegn doctor` reflects it.

## Phase 6 — docs and gates

- [x] 6.1 `docs/help/ai-usage.md`: proxy-spend block prose (help-prose
      ratchet); confirm no new unclaimed actions/contexts.
- [x] 6.2 `docs/ARCHITECTURE.md`: add the proxy as the second background
      process beside the pane daemon, with its enforcing gates; note the
      resurrection boundary (ACP/bouncer/compression stay out).
- [x] 6.3 `test/smoke.sh`: disabled-by-default assertion + a
      config-validate-rejects-raw-key case.
- [ ] 6.4 Run `just ci` once (includes openspec-validate, coverage, ratchets)
      and fix fallout. (Pre-PR gate — run at land time, not per-edit.)
