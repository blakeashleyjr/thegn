# Model Proxy

## ADDED Requirements

### Requirement: The model proxy is opt-in and the shell never depends on it

thegn SHALL provide a local model-proxy daemon (`tgproxy`) that is disabled by
default and enabled only by an explicit `[model_proxy] enabled = true`. While
disabled or unconfigured, thegn MUST NOT launch the proxy process, spawn proxy
threads, write proxy DB rows, or render proxy UI, and every other capability
MUST behave exactly as it does without the feature. A stale pre-alpha
`[llm_proxy]` section MUST NOT enable or configure the resurrected proxy — it
keeps its existing tolerated-and-warned behavior.

#### Scenario: Disabled proxy leaves the shell untouched

- **WHEN** thegn runs with no `[model_proxy]` section
- **THEN** no proxy process exists, no `model_proxy_*` rows are written, and
  agent launches receive no proxy environment

#### Scenario: Stale legacy config does not resurrect the proxy

- **WHEN** a config carries a pre-alpha `[llm_proxy]` section and no
  `[model_proxy]` section
- **THEN** the section is dropped with the existing one-line config warning and
  the proxy stays disabled

### Requirement: Providers form a declarative registry with seam semantics

`[[model_proxy.providers]]` entries SHALL declare each upstream once — name,
`kind`, base URL, credentials, rate/concurrency limits, pricing, and optional
multi-key lanes — and both config validation and runtime client construction
SHALL derive from that single registry. `kind` MUST be either implemented
(`anthropic`, `openai` — the wire-protocol adapters, covering openai-compatible
local runtimes such as Ollama/vLLM) or `reserved`, producing the standard
implemented-or-reserved diagnostic for anything else. Upstream dispatch SHALL
be an object-safe seam trait with capability bits, and vendor-specific request
shaping MUST live only inside the adapter implementation files.

#### Scenario: A reserved kind is diagnosed, not crashed on

- **WHEN** a provider entry declares `kind = "gemini"` and `gemini` is reserved
- **THEN** `thegn config validate` and `thegn doctor` report the kind as
  reserved/not-yet-implemented and the proxy excludes that provider from routing

#### Scenario: A local runtime is just another lane

- **WHEN** a provider entry points `kind = "openai"` at an Ollama base URL and a
  route lists it after a cloud provider
- **THEN** requests fail over onto the local runtime when the cloud lane is
  exhausted, with no local-runtime special case outside the adapter

### Requirement: Provider credentials are SecretRefs and are never persisted

Provider `api_key` values MUST be SecretRefs (`env:VAR` or `file:PATH`). A raw
literal key MUST fail `thegn config validate` and MUST prevent proxy start with
a diagnostic naming the offending provider. Resolved key material SHALL exist
only in the proxy process's memory: it MUST NOT be written to the state DB,
logs, status output, or debug formatting, and the host MUST pass providers to
the daemon by reference, resolving SecretRefs inside the proxy process.

#### Scenario: A raw key is refused

- **WHEN** a provider entry sets `api_key = "sk-live-abc123"`
- **THEN** validation fails, the proxy does not start, and the diagnostic names
  the provider and the accepted SecretRef forms

#### Scenario: Status output never leaks a key

- **WHEN** `thegn proxy status` or `thegn doctor` describes a provider
- **THEN** output states whether the key reference resolves, never any part of
  its value

### Requirement: Requests route through named tiers with aliasing and a deterministic auto tier

`[[model_proxy.routes]]` SHALL define named tiers, each a priority list of
(provider, model) backends; the client's requested model id selects a route
directly (`model-proxy/<route>`) or through the `[model_proxy.aliases]` map. A
reserved `auto` target SHALL select a tier via a pure, deterministic classifier
over request features only (estimated prompt tokens, tool presence, streaming
flag, optional client mode header) — it MUST NOT issue model calls, and an
explicit route or alias match MUST take precedence over `auto`. The routing
strategy SHALL be configurable: `sequential` (declared order), `load_balanced`
(rotating first choice), or `cost_aware` (cheapest lane first by the price
table).

#### Scenario: Alias beats auto

- **WHEN** a request names a model id present in the alias map
- **THEN** the aliased route is used and the auto classifier is not consulted

#### Scenario: Auto pick is deterministic

- **WHEN** two identical requests target `auto`
- **THEN** the same tier is selected both times, and the selection is
  reproducible from the request features alone

#### Scenario: Cost-aware ordering prefers the cheaper lane

- **WHEN** `routing = "cost_aware"` and a route holds lanes at different prices
- **THEN** the cheapest eligible lane is attempted first and costlier lanes are
  reached only by fall-through

### Requirement: Failover classifies responses and cools down unavailable lanes

The proxy SHALL attempt a route's lanes in strategy order, classifying each
response: a usable success is served; a request-specific failure falls through
with no penalty; an availability failure (rate limit, auth/credit exhaustion,
upstream 5xx) falls through AND cools the lane down, honoring `Retry-After`
and reset-window hints, with recovery probing after cooldown. For streaming
requests the proxy SHALL wait a bounded first-byte window before falling
through to the next lane, and MUST kill and account a committed stream that
goes silent past the idle timeout. When every lane of a route is exhausted and
a `last_resort` tier is configured, the proxy SHALL borrow other routes' lanes
as a final tier; otherwise it MUST return a classified error to the client
rather than hanging.

#### Scenario: Provider outage fails over

- **WHEN** the first lane returns HTTP 529 and the second lane is healthy
- **THEN** the client receives the second lane's response, and the first lane is
  skipped for subsequent requests until its cooldown lapses

#### Scenario: A request-specific error does not poison the lane

- **WHEN** a lane rejects one malformed-for-it request with a 400
- **THEN** the request falls through, and the lane remains first choice for the
  next request

#### Scenario: Slow first byte falls through

- **WHEN** a streaming lane produces no usable output within the first-byte
  window
- **THEN** the request is retried on the next lane and the client stream carries
  the successful lane's output

### Requirement: Usage-aware ordering respects account headroom without refusing service

When `usage_aware = true`, lane ordering SHALL incorporate the `[usage]`
tracker's per-account window state supplied by the shell: lanes whose account
has crossed the warn threshold are deprioritized, and lanes past the critical
threshold are skipped — except that at least one lane per request MUST always
remain eligible, degrading to plain strategy order when every lane is
throttled. The proxy MUST NOT fetch quota state itself; it consumes snapshots
the shell already gathers.

#### Scenario: A nearly-exhausted subscription account yields to a fresh one

- **WHEN** the first lane's account is past warn and a peer lane's account is
  fresh
- **THEN** the fresh lane is attempted first

#### Scenario: All-throttled still serves

- **WHEN** every lane's account is past the critical threshold
- **THEN** the request is still routed using plain strategy order

### Requirement: Every request is accounted as metadata; content is never recorded

The proxy SHALL record one audit row per request — route, backend, model,
protocol, caller scope (workspace/worktree/agent via the attribution key),
input/output/cache-read/cache-creation tokens, cost in USD with its source
(measured, estimated, or subscription at $0), duration, time-to-first-byte,
and outcome classification. Prompt, message, tool-call, and response content
MUST NOT be logged or persisted at any log level in release builds. Stats
rollups (spend, token throughput, latency percentiles, per-route/backend/scope
breakdowns) SHALL be computed by one shared pure implementation consumed by
the `/stats` endpoint, the CLI, and the usage panel alike.

#### Scenario: Subscription lanes account at zero marginal cost

- **WHEN** a request is served by a provider marked `subscription = true`
- **THEN** its audit row records the token counts and a $0 cost with source
  `subscription`

#### Scenario: The audit row carries no content

- **WHEN** any request is proxied, successfully or not
- **THEN** its stored row and all log lines contain identifiers, counts, and
  timings only — no prompt or completion text

### Requirement: Per-scope budgets alert through the usage path and optionally enforce

`[model_proxy.budget]` SHALL define spend ceilings per scope over rolling
windows, with `on_breach` one of `warn` (default — raise a notification via
the existing usage-alert path and keep serving), `refuse` (reject further
requests in that scope with a budget-classified error), or `downgrade` (route
the scope's requests to the cheapest eligible lane). Budget state SHALL
persist across restarts and window rollovers MUST advance anchors rather than
leak spend between windows.

#### Scenario: Warn is the default and never blocks

- **WHEN** a scope crosses its ceiling with `on_breach` unset
- **THEN** a usage alert is raised and requests continue to be served

#### Scenario: Refuse rejects with a classified error

- **WHEN** a scope crosses its ceiling under `on_breach = "refuse"`
- **THEN** subsequent requests in that scope receive a budget-exceeded error and
  other scopes are unaffected

### Requirement: The proxy lifecycle is supervised, single-instance, and off the UI loop

When enabled, the host SHALL launch and supervise the proxy off the UI event
loop (never before the first frame), restarting crashes with backoff. The
listen endpoint SHALL be the single-instance lock: a process that loses the
bind exits successfully, deferring to the winner. `thegn proxy stop` SHALL
terminate gracefully, and disabling the feature SHALL stop the daemon.
Supervision events surface through the existing status/notification paths with
waker delivery, adding zero idle wakes.

#### Scenario: Second instance defers

- **WHEN** a proxy is already bound and another launch attempt occurs for the
  same state dir
- **THEN** the newcomer exits 0 without disturbing the incumbent

#### Scenario: Crash restarts with backoff

- **WHEN** the proxy process dies unexpectedly
- **THEN** the host restarts it on a backoff schedule and surfaces a status
  warning, and the UI loop's idle behavior is unchanged

### Requirement: Proxy control projects through the capability catalog

`model_proxy.status`, `model_proxy.stats`, `model_proxy.start`, and
`model_proxy.stop` SHALL be rows in the one capability catalog, exposed on the
operator surfaces (control HTTP/gRPC and CLI) and not on MCP or plugin
surfaces. Their scopes MUST come from `required_scope(verb)` — `read` for
status/stats, `admin` for start/stop — with no second policy table, and the
per-surface coverage tests SHALL hold for all four rows. The CLI verbs are
`thegn proxy status|stats|start|stop` (plus the hidden `serve` entry the
daemon itself runs), with `--json` output for status and stats.

#### Scenario: Scope gating follows the catalog

- **WHEN** a control token holding only `read` calls `model_proxy.stop`
- **THEN** the call is rejected by the same scope check every surface uses

#### Scenario: Not reachable from MCP or plugins

- **WHEN** the MCP tool list or plugin verb table is enumerated
- **THEN** no model-proxy capability appears

### Requirement: `thegn doctor` probes the proxy and its providers

`thegn doctor` SHALL report the model proxy: enabled state, listen address
(warning when non-loopback), daemon reachability, and — per provider — kind
(implemented or reserved), SecretRef resolvability (never values), and route
membership. A disabled proxy SHALL print as a single quiet line, not an error.

#### Scenario: Doctor flags an unresolvable key

- **WHEN** a provider's `env:VAR` names an unset variable
- **THEN** doctor lists that provider as not-configured with the variable name,
  and no key material appears in output

### Requirement: Agents opt in to proxy routing per entry, safely

An `[[agents]]` or `[[tools]]` entry MAY set `route_via_proxy = true`; at
spawn thegn SHALL then inject the provider base-URL environment
(`ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL`) pointing at `listen` plus a minted
per-worktree attribution key carrying the caller scope. Injection MUST be
preceded by a liveness probe of the endpoint: if the proxy is not reachable,
thegn MUST skip injection, launch the agent unmodified, and surface a status
warning — a down proxy MUST NOT strand an agent on a dead loopback endpoint.
The default is off, and entries without the flag are never touched.

#### Scenario: Opt-in injection routes the agent

- **WHEN** an agent entry with `route_via_proxy = true` launches while the proxy
  is up
- **THEN** the agent's environment points at the proxy with an attribution key,
  and its requests appear in the audit rows attributed to its worktree

#### Scenario: Down proxy never breaks the launch

- **WHEN** the same entry launches while the proxy is unreachable
- **THEN** the agent starts with its normal direct-provider environment and a
  warning notes the skipped routing

### Requirement: Proxy spend is visible in the usage surfaces

When the proxy is enabled, the System ▸ Usage panel section and the usage
dashboard SHALL include a proxy-spend block — cost and token totals by route
for the current day and trailing week — hydrated off the event loop from the
audit rows on the existing usage refresh cadence with waker delivery. The
block MUST be absent when the proxy is disabled, and the `docs/help/ai-usage.md`
page SHALL describe it under the existing `panel:usage` help context. Under
the e2e determinism freeze the proxy and its UI are disabled.

#### Scenario: Spend appears beside quota

- **WHEN** the proxy has served requests and the user opens System ▸ Usage
- **THEN** per-route spend/token totals render alongside the per-account quota
  windows without blocking the UI loop

#### Scenario: Disabled means invisible

- **WHEN** `[model_proxy]` is absent
- **THEN** the usage surfaces render exactly as they do today, with no proxy
  block
