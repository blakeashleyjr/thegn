# Tasks

## 1. Router + envelope (thegn-core, pure)

- [ ] 1.1 `RouteDecision.push` computed by `notification_route::decide`
      (rules `channels` accept `push`; DND treats push as ephemeral; modes/
      profiles/debounce unchanged) — **unit tests**: rule include/exclude,
      DND suppression, priority floor, `set_priority` interaction table.
- [ ] 1.2 Config: `[notifications.push]` + `[notifications.push.inbox]`
      (kinds via `config_enum!`: `ntfy` | reserved `telegram`/`gotify`/
      `pushover`/`webhook`; SecretRef fields; enabled-without-secret and
      enabled-with-empty-allowlist are config errors) — **unit tests** +
      `config/config.toml.example` entries.
- [ ] 1.3 Inbox envelope: canonical serialization, HMAC-SHA256 verify,
      freshness window, replay LRU, allowlist ∩ scope ceiling ∩ admin-deny
      admission — **unit tests**: tampered MAC, stale ts, replayed id,
      unlisted cap, admin cap refused even when allowlisted.

## 2. Publisher + subscriber (thegn-svc)

- [ ] 2.1 `push` seam: trait + `ntfy` publisher (bounded reqwest client,
      priority/title/tags mapping, Bearer token, ≤2 retries then drop);
      pure request-shaping functions unit-tested, HTTP exercised by smoke.
- [ ] 2.2 ntfy SSE subscriber with reconnect/backoff + `since=` resume and
      long-poll fallback; per-minute execution cap; drop counters.
- [ ] 2.3 Probe: `ProbeReport { seam: "push", id: "ntfy" }` (server
      reachability, token presence, inbox on/off + allowlist size).

## 3. Wiring (thegn-host)

- [ ] 3.1 `NotifyState` → publisher worker channel (QoS Background,
      drop-on-overflow, never blocks the loop); waker-pulsed status updates.
- [ ] 3.2 Daemon inbox task: subscribe → verify (core) → dispatch through the
      capability-catalog door used by the control API; record executions via
      the existing `notify.push` path for audit; unavailable (with reason)
      when the daemon is disabled.
- [ ] 3.3 Doctor wiring for both probes + mosh-server presence note.
- [ ] 3.4 `docs/help/` mobile-access page (mosh → attach path, ntfy setup,
      inbox safety model); help + prose ratchets green.

## 4. Verification

- [ ] 4.1 Smoke: host runs green with push configured against an unreachable
      server (best-effort drop, no hang); inbox refuses to enable without a
      secret.
- [ ] 4.2 Run `just ci` once (includes openspec-validate) as the pre-PR gate.
