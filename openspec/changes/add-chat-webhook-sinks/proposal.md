# Discord / Slack / chat integration — webhook sinks, not bots

Linear: THE-62

## Why

"Discord / Slack / etc integration?" splits into three honestly different
lanes, and only one of them is cheap and clearly right:

1. **Outbound notifications into a chat channel.** Discord and Slack both
   accept _incoming webhooks_ — a single authenticated POST per message, no
   gateway connection, no bot account, no library. thegn already has the
   exact right seam for this: the notification router's push-provider seam
   (in-flight `add-ntfy-push-bridge`, THE-12) — object-safe trait, config
   `kind` implemented-or-`reserved`, best-effort off-loop delivery, doctor
   probe — which provides the `webhook` kind. "queue needs a
   human" landing in the team's Slack channel is the actual value of this
   issue, and it is three payload formatters deep, not a platform.
2. **Interactive bots** (the issue's seed link, serenity-rs). serenity is a
   full Discord gateway/bot framework: a heavy always-on tokio dependency, a
   bot-token credential class, voice/gateway machinery — inside the thegn
   process it buys a worse version of what the control plane already offers.
   Judged and rejected in-process (design.md): an out-of-process bot built on
   the scoped control API + event feed needs **zero thegn changes**, and the
   inbound-command story already has one blessed pattern — the signed
   command-inbox envelope owned by `add-ntfy-push-bridge`.
3. **Chat clients as panes** (roadmap AM 470/471 — discordo, slack TUIs).
   Already covered by the `[[tools]]` picker today; a config example, not
   code.

## What Changes

- **Implement three sink kinds in the push-provider seam** (layers on
  `add-ntfy-push-bridge`'s seam; this change lands after it):
  - `webhook` — generic JSON POST of a stable payload
    (`{kind, priority, message, source, worktree, ts}`) for anything with an
    HTTP endpoint (n8n, Zapier, a team's own service).
  - `discord` — Discord incoming-webhook payload (content within Discord's
    2000-char bound with truncation, priority mapped to embed color).
  - `slack` — Slack incoming-webhook payload (text + minimal blocks,
    priority mapped to attachment color).
- **Named sinks, one router.** The router supports one or more named push
  sinks; rules' `route` selectors address all of them (`push`) or one by
  name (`push:<name>`), so "alerts → Slack, everything → phone" is a routing
  rule, not code. Priority/DND/mode/profile machinery applies per sink
  unchanged.
- **Webhook URLs are SecretRefs.** A Discord/Slack webhook URL _is_ the
  credential — anyone holding it can post. URLs resolve via `env:`/`file:`
  only; a raw URL in config is a validation error.
- **Client-side rate limiting per sink** (Discord ~30/min per webhook, Slack
  ~1/msg-sec), honoring `429 Retry-After`, on top of the seam's bounded
  best-effort worker: over the limit coalesces to drop-with-counter, never a
  send queue that grows and never a blocked loop.
- **Doctor probes per sink** report config + secret resolution without
  posting. Deliberate test delivery is outside this change and never a
  diagnostic side effect.
- **Bots stay out, with criteria.** No serenity, no gateway, no bot tokens in
  thegn. If interactive chat control is ever wanted, the revisit path is
  fixed: the command-inbox envelope (allowlisted capability calls, HMAC,
  scope ceiling) over a chat transport, preferably as an out-of-process
  bridge speaking the control API — not an embedded bot framework.

## Impact

- **Roadmap:** the "Etc integration" half of the notification polish tail
  (AI group); **AM 470/471** (Discord/Slack tiles) noted as covered by
  `[[tools]]` with a config example; composes with **AP 504** — an
  automation's `notify` action reaches chat through these sinks with no
  automation-side delivery code (`add-automation-rules`, THE-21, sibling).
- **Specs:** delta on `notifications` (chat/webhook sink kinds, named-sink
  routing, rate limiting, SecretRef URLs, probe semantics).
- **Code:** `thegn-svc` — three payload shapers + per-sink limiter in the
  push seam (pure request-shaping unit-tested; HTTP by smoke); `thegn-core` —
  named-sink route targets in `RouteDecision`/rule parsing (95% gate),
  config validation (SecretRef-only URLs); `thegn-host` — doctor rows,
  config example, `docs/help/` notification page prose.
- **Config:** sink tables under `[notifications]` (name, kind, `url =
"env:…"|"file:…"`, optional per-sink `min_priority`) — exact TOML shape
  reconciled with `add-ntfy-push-bridge`'s `[notifications.push]` at land
  time (design.md); every key documented in `config/config.toml.example`.
- **In-flight overlap:** **depends on `add-ntfy-push-bridge`** (the seam, the
  `webhook` reservation, the command-inbox pattern this change deliberately
  does not duplicate); `add-event-feed-subscriptions` strengthens the
  out-of-process bot story (referenced, not depended on); no overlap with
  `add-osc-attention-signaling` (different channel). No capability-catalog
  change (no new externally invokable verb; delivery is internal routing).
- **No DB change, no render-path change**; all delivery work is off-loop in
  the existing push worker.

## Non-goals

- An embedded Discord/Slack/Telegram **bot** (gateway connections, slash
  commands, message reading) — out-of-process via the control API instead.
- Inbound chat commands — the command inbox belongs to
  `add-ntfy-push-bridge`; a chat transport for it is a later change if ever.
- Rich per-message templating/threading/mentions per sink (a `mention`
  escape hatch may ride the payload shaper later; v1 is fixed shapes).
- Building or bundling chat TUI clients (AM 470/471) — `[[tools]]` config
  example only.
