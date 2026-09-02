# Design — chat webhook sinks

## Where this sits

`add-ntfy-push-bridge` defines the push-provider seam: a `push` delivery
channel in `RouteDecision`, an object-safe provider trait in `thegn-svc`, a
bounded best-effort worker, `kind` implemented-or-`reserved` (`ntfy`
implemented; `telegram`, `gotify`, and `pushover` reserved), doctor probes.
This change implements `webhook` and adds `discord` and `slack` as siblings —
formatters over the same worker, not a new
mechanism. Everything below is delta on that seam.

## Named sinks and routing

One chat channel is rarely the whole story ("alerts to #oncall, merge-queue
to #ship"). The router therefore addresses **named sinks**:

- Each configured sink has a unique `name` (defaulting to its kind when only
  one of that kind exists) and a `kind`.
- Rules' `route` selectors accept `push` (all sinks) and `push:<name>`
  (one sink). `RouteDecision` carries the resolved sink set instead of a
  boolean — a pure-core change under the 95% gate.
- Per-sink `min_priority` gives the common "chat only gets alerts" shape
  without a rule.

**Config shape.** `add-ntfy-push-bridge` specifies a single
`[notifications.push]` table. The compatible superset keeps that table and
places an array of named sinks below it — e.g.

```toml
[[notifications.push.sinks]]
name = "phone"
kind = "ntfy"
server = "https://ntfy.example"
topic = "thegn"
token = "env:THEGN_NTFY_TOKEN"

[[notifications.push.sinks]]
name = "oncall"
kind = "slack"
url = "env:THEGN_SLACK_ONCALL_URL"
min_priority = "alert"
```

— where a lone scalar `[notifications.push]` keeps parsing as one sink named
by its kind. The nested array leaves `[notifications.push.inbox]` unchanged.

## Payload shapes (pure, unit-tested)

- `webhook`: `POST` `application/json` of
  `{v: 1, kind, priority, message, source, worktree, ts}` — a stable,
  versioned shape a receiver can rely on; no templating in v1.
- `discord`: `{content}` with the effective priority as an embed color strip
  (Alert red / Notice neutral / Info dim), message truncated to Discord's
  2000-char bound with an ellipsis marker.
- `slack`: `{text, blocks: [section]}` with an attachment color by priority,
  truncated to Slack's text bounds.

Shaping is a pure function `(note, priority, sink) -> HttpRequestShape` so
the truncation/color/schema tables carry unit tests; actual HTTP stays in
the svc worker and is exercised by smoke.

## Rate limiting

Platform limits are real and low (Discord ≈30/min per webhook; Slack ≈1
msg/sec sustained). Per-sink token bucket in the worker, plus honoring
`429 Retry-After` for the in-flight message (one deferred retry within the
seam's bounded-retry budget). Over the bucket: drop, count, expose the
counter in the doctor row. Deliberately **no** growing send queue and no
digest/coalescing in v1 (a digest mode is a plausible follow-up; it changes
message semantics and is not needed for correctness).

## The bot judgment (serenity)

Rejected in-process, on four grounds:

1. **Weight**: serenity is a gateway framework (websocket session
   management, sharding, its own tokio task tree, a large dependency
   surface) inside a compositor whose svc layer otherwise holds thin HTTP
   clients. The cost is permanent; the benefit duplicates the control plane.
2. **Credential class**: bot tokens are a standing, broad credential
   (read/write on every channel the bot joins) — categorically worse than a
   single-channel webhook URL under SecretRef.
3. **Wrong side of the seam**: an interactive bot is a _client_ of thegn
   (subscribe to events, invoke scoped verbs) — exactly what the scoped
   control API + event feed (and `add-event-feed-subscriptions`' filters)
   exist for. An out-of-process `thegn-discord-bot` needs zero changes here
   and can be community-owned.
4. **Inbound commands already have one blessed pattern**: the ntfy
   command-inbox envelope (signed, allowlisted, scope-ceilinged capability
   calls). A second inbound door with different rules would be a policy
   fork.

**Revisit criteria** (recorded so "reserved" means something): sustained
demand for chat-initiated control that the out-of-process bridge cannot
serve; if met, the shape is the command-inbox envelope carried over a chat
transport in the daemon — never message-content parsing, never a second
admission policy, and still preferably out-of-process.

## Security

- **Webhook URLs are bearer credentials.** SecretRef only (`env:`/`file:`);
  a raw `https://hooks.slack.com/…` in config is a validation error naming
  the sink. Resolved URLs never appear in logs, doctor output, or error
  messages (log the sink name, not the URL).
- **Egress**: sinks post to user-configured hosts from the svc worker;
  under a sandboxed/filtered-network deployment the doctor row surfaces
  unreachability. No new listener, no inbound surface at all.
- **Message content leaves the machine**: notification messages can carry
  branch names, issue titles, log fragments. That is the feature, but the
  per-sink `min_priority` and the rules' `channels` targeting are the
  containment knobs; the help page says plainly that chat sinks exfiltrate
  whatever the routed notification says, so route deliberately. No secrets
  are ever interpolated into payloads by thegn itself.
- **Probe never posts.** Doctor validates config + secret resolution only; a
  visible test delivery is outside this change and not a side effect of
  diagnostics.
- **Blast radius**: worst case on a leaked config is nil (URL is a
  SecretRef); worst case on a leaked env/file secret is spam into one chat
  channel — revoke the webhook at the platform.

## Alternatives considered

- **Embedded serenity bot** — rejected above.
- **A separate `[notifications.chat]` seam** parallel to push — rejected:
  same trait, same worker, same routing; a second seam would duplicate the
  DND/priority/rules plumbing for no behavioral difference.
- **Shell-out sinks** (`mode = "command"`-style, user script per message) —
  already possible via the sound-command hook and automations' `run`
  action; a first-class formatter with rate limiting is what those can't
  give.
- **Templated payloads in v1** — deferred; fixed shapes keep the truncation
  and platform-limit logic testable and the config surface small.

## Open questions

- Whether `push:<name>` channel targeting should also apply to the `ntfy`
  kind's existing selectors (it should fall out of named sinks for free —
  confirm against the landed shape of `add-ntfy-push-bridge`).
- Discord embed vs plain content for Alert priority (embed is nicer, costs
  payload size) — decide at implementation with the truncation tests.
- Whether `telegram` (roadmap AI 423) should land as a fourth formatter here
  once the seam exists — it is bot-API-key based (not a webhook), so it
  stays reserved in this change.
