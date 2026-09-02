# Chunk 2 — svc webhook providers and bounded delivery policy

## Scope

Implement the three push-provider kinds on the existing service seam. This
chunk is file-disjoint from chunks 1 and 3, but depends on chunk 1's
`PushSinkConfig`, named route vocabulary, and pure renderer; chunk 3 depends on
the provider factory/caps/error APIs here. Run after chunk 1 and before chunk 3.
No new dependency is permitted.

## Files touched (exact)

- `crates/thegn-svc/src/push/mod.rs`
- `crates/thegn-svc/src/push/webhook.rs` (new)
- `crates/thegn-svc/src/push/discord.rs` (new)
- `crates/thegn-svc/src/push/slack.rs` (new)
- `crates/thegn-svc/src/push/rate_limit.rs` (new)
- `crates/thegn-svc/src/seam/registry.rs`
- `crates/thegn-svc/src/conformance.rs`

Do not touch `crates/thegn-svc/src/provider.rs`: use its existing
`provider_http_client()`. Do not add a Cargo dependency or move any HTTP type
into `thegn-core`.

## Approach

1. Preserve the object-safe `PushProvider` shape and add a serializable caps
   description for payload limit, markdown flavor, priority-color support, and
   dry-run capability. The factory takes one already-validated sink config at a
   time and returns ntfy or one of the three new providers; reserved kinds
   remain visibly reserved.
2. Put pure payload/request helpers in focused provider modules. Generic
   webhook emits versioned JSON with `v = 1`, `kind`, `priority`, `message`,
   `source`, `worktree`, and `ts`. Discord uses a valid incoming-webhook envelope
   and visible
   character-counted truncation at 2,000. Slack uses text plus minimal section
   structure and a priority color. All helpers consume the core rendered value;
   no provider invents event names or templates.
3. Add pure rate-limit state transitions: per-sink token bucket with explicit
   capacity/refill defaults, `Retry-After` clamped to the bounded retry window,
   and decisions `Send`, `Defer`, or `Drop`. A rate-limited message is counted
   and dropped when the bounded queue/retry budget cannot carry it; there is no
   unbounded backlog or digest/coalescing.
4. Keep reqwest POSTs thin and off-loop, using the existing provider client and
   bounded timeout/attempt conventions. Never put URL text in `Display`,
   `Debug`, `ProbeReport`, tracing fields, or `PushError`. Classify HTTP status
   and transport errors for retry/auth/final handling.
5. Extend the existing push registry with one probe per effective named sink.
   Probes validate SecretRef syntax/presence and URL parse, build a request
   shape as the offline “dry-run ping”, and return caps/notes without opening a
   socket or posting. Keep conformance’s known seam as `push`; do not add a
   capability catalog row.

## Tests to run

- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc push`
- `cargo nextest run -p thegn-svc conformance`

Tests must cover each factory kind, reserved kind reporting, generic schema and
timestamp, Discord 2,000-character Unicode truncation/marker, Slack and
priority-color mappings, newline/control safety, URL-redaction assertions,
token-bucket transitions/refill/drop, bounded Retry-After, status
classification, and probes that make zero network calls.

## Ratchets

This chunk adds no direct dependency and no platform cfg. Verify
`test/async-trait-ratchet.txt`, `test/ignored-result-ratchet.txt`, and the
provider conformance/kind-coverage tests. Any best-effort worker result must
carry the repository's explicit `// best-effort:` reason. Do not touch
completion, help, env-overlay, control-schema, or capability ratchets: this
chunk adds no external action or config environment knob.

## Done criteria

- `webhook`, `discord`, and `slack` are implemented siblings of ntfy on the
  same object-safe push seam; Telegram/gotify/pushover remain reserved.
- All actual HTTP uses the existing reqwest client in svc provider code, with
  bounded timeout/retry and no new crate.
- Per-sink rate limiting and Retry-After are bounded, unit-tested state
  transitions; no event-loop or unbounded queue path exists.
- Offline probes validate/shape without POST and expose no URL or secret.
- The factory/conformance tests prove every implemented kind is built and every
  reserved kind remains reserved.
- Commit exactly as: `feat(the-62): add webhook push providers and bounded delivery`
