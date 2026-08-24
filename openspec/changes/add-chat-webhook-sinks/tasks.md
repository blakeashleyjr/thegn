# Tasks

Ordering: starts after `add-ntfy-push-bridge`'s seam lands (this change
implements its reserved `webhook` kind and adds siblings).

## 1. Routing (thegn-core, pure)

- [ ] 1.1 Named sinks in the route model: `RouteDecision` carries a resolved
      sink set; rules' `channels` accept `push` and `push:<name>`; per-sink
      `min_priority` — **unit tests**: targeting tables, DND/priority
      interaction per sink, unknown sink name is a config error (95% gate).
- [ ] 1.2 Sink config parsing/validation: array-of-tables superset of the
      single-table `[notifications.push]` form (lone table = one sink named
      by kind); `kind` ∈ {ntfy, webhook, discord, slack} implemented,
      `telegram`/`gotify`/`pushover` reserved; URL fields SecretRef-only
      (raw URL rejected naming the sink) — **unit tests** +
      `config/config.toml.example` entries.

## 2. Sinks (thegn-svc)

- [ ] 2.1 Pure payload shapers for `webhook` (versioned JSON schema),
      `discord` (2000-char truncation, priority→color), `slack`
      (text/blocks, priority→color) — **unit tests** on shape, truncation,
      and priority mapping tables.
- [ ] 2.2 Per-sink token-bucket rate limiter + `429 Retry-After` handling
      inside the push worker's bounded-retry budget; over-limit drops
      counted — limiter logic **unit-tested** as pure state transitions;
      HTTP by smoke.
- [ ] 2.3 Doctor probes per sink: config + secret resolution + drop
      counters, never a network post; secrets never in probe output.

## 3. Wiring and docs (thegn-host)

- [ ] 3.1 Route decisions fan out to the named-sink worker; existing ntfy
      behavior byte-identical when it is the only sink.
- [ ] 3.2 `thegn notify … --send-test <sink>` (or equivalent flag on the
      existing notify verb — no new catalog row) for deliberate delivery
      verification.
- [ ] 3.3 Help/notification page prose: sink setup, the exfiltration
      caution, rate-limit behavior; `[[tools]]` config example for chat TUI
      tiles (AM 470/471) in the example config comments (prose ratchet
      green).

## 4. Verification

- [ ] 4.1 Smoke: host green with a `discord` sink configured against an
      unreachable/refusing endpoint (bounded retry, drop, counter — no hang,
      no loop wake); raw-URL config refused.
- [ ] 4.2 Run `just ci` once (includes openspec-validate) as the pre-PR
      gate.
