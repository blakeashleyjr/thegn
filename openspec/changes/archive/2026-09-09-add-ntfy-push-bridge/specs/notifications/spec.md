# Notifications

## ADDED Requirements

### Requirement: Push-to-phone is a routed delivery channel behind a provider seam

The notification router SHALL support a `push` delivery channel governed by
the same machinery as every existing channel: rules `channels` selectors MAY
include or exclude `push`, DND SHALL suppress it below `allow_priority` (it is
an ephemeral channel; the inbox row remains the durable record), and mode/
profile overlays and burst debouncing apply unchanged. Delivery SHALL go
through a push-provider seam (object-safe trait, config `kind`
implemented-or-`reserved`, `Probe` in `thegn doctor`) whose first implemented
kind is `ntfy` — POSTing to a configured `server`/`topic` with the effective
priority mapped (Alert→high, Notice→default, Info→low) — with `telegram`,
`gotify`, `pushover`, and `webhook` reserved. Publishing MUST be best-effort
and off the event loop (bounded worker, bounded retries, drop-on-overflow;
a push failure never blocks or fails the notification), and the auth token
MUST be a SecretRef (`env:`/`file:`) — never a raw token in config.

#### Scenario: An alert reaches the phone

- **WHEN** `[notifications.push]` is configured with kind `ntfy` and an
  Alert-priority notification passes the rules with `push` among its channels
- **THEN** the ntfy topic receives a high-priority message carrying the
  notification's title and message, published off-loop, and the inbox row is
  recorded exactly as before

#### Scenario: DND holds push like any ephemeral channel

- **WHEN** DND is active with `allow_priority = "alert"` and a Notice-priority
  notification arrives
- **THEN** no push is published; the notification records in the inbox

#### Scenario: An unreachable push server costs nothing

- **WHEN** the configured ntfy server is unreachable
- **THEN** the publish retries a bounded number of times off-loop and is then
  dropped with a counter — the event loop never blocks and no notification is
  lost from the inbox

#### Scenario: Doctor probes the push seam

- **WHEN** `thegn doctor` runs with a push channel configured
- **THEN** a `push`/`ntfy` probe row reports availability (server
  reachability, token presence) in the standard probe shape
