# Tasks

## 1. Router + envelope (thegn-core, pure)

- [x] 1.1 `RouteDecision.push` computed by `notification_route::decide`
      (rules `channels` accept `push`; DND treats push as ephemeral; modes/
      profiles/debounce unchanged) — **unit tests**: rule include/exclude,
      DND suppression, priority floor, `set_priority` interaction table.
- [x] 1.2 Config: `[notifications.push]` + `[notifications.push.inbox]`
      (kinds via `config_enum!`: `ntfy` | reserved `telegram`/`gotify`/
      `pushover`/`webhook`; SecretRef fields; enabled-without-secret and
      enabled-with-empty-allowlist are config errors) — **unit tests** +
      `config/config.toml.example` entries.
- [x] 1.3 Inbox envelope: canonical serialization, HMAC-SHA256 verify,
      freshness window, replay LRU, allowlist ∩ scope ceiling ∩ admin-deny
      admission — **unit tests**: tampered MAC, stale ts, replayed id,
      unlisted cap, admin cap refused even when allowlisted.

## 2. Publisher + subscriber (thegn-svc)

- [x] 2.1 `push` seam: trait + `ntfy` publisher (bounded reqwest client,
      priority/title/tags mapping, Bearer token, ≤2 retries then drop);
      pure request-shaping functions unit-tested, HTTP exercised by smoke.
- [x] 2.2 ntfy stream subscriber (ntfy `/json` newline-stream) with
      reconnect/backoff + `since=` resume; per-minute execution cap; drop
      counters. (See design note: `/json` chosen over `/sse` — both ntfy-native
      streams; newline-JSON framing is trivially/robustly parsed.)
- [x] 2.3 Probe: `ProbeReport { seam: "push", id: "ntfy" }` (server
      reachability, token presence, inbox on/off + allowlist size).

## 3. Wiring (thegn-host)

- [x] 3.1 `NotifyState` → publisher worker channel (QoS Background,
      drop-on-overflow, never blocks the loop); waker-pulsed status updates.
- [x] 3.2 Daemon inbox task: subscribe → verify (core) → dispatch through the
      capability-catalog door used by the control API (`build_call` +
      `dispatch_local`); record executions via the existing `notify.push` path
      for audit; unavailable (with reason) when the daemon is disabled.
- [x] 3.3 Doctor wiring for both probes + mosh-server presence note
      (`Mobile access` section + `mobile_access` JSON).
- [x] 3.4 `docs/help/` mobile-access page (mosh → attach path, ntfy setup,
      inbox safety model); help + prose ratchets green.

## 4. Verification

- [x] 4.1 Smoke: host runs green with push configured against an unreachable
      server (best-effort drop, no hang); inbox refuses to enable without a
      secret.
- [ ] 4.2 Run `just ci` once (includes openspec-validate) as the pre-PR gate.
      (Left for the human reviewer — see the return note; scoped `just quick` + targeted nextest were used during implementation per the dev-loop
      policy.)
