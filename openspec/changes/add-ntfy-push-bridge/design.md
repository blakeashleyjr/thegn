# Design

## Outbound: the push channel

**Chokepoint.** All emit sites already funnel through `NotifyState::decide`
(`thegn-host/src/notify.rs`) → the pure router
(`thegn_core::notification_route::decide`). The delta is one more channel:
`RouteDecision` gains `push: bool`, computed by the same rule evaluation
(`channels` selector lists may name `push`), DND gating (push is an ephemeral
channel — suppressed below `allow_priority`), mode/profile overlays, and the
existing `NotifyDebounce` burst suppression. Pure logic ⇒ unit-tested in core
under the 95% gate.

**Publisher seam** (`thegn_svc::push`): object-safe trait per
`thegn_core::seam` (`kind`, `publish(note) -> BoxFuture<Result<()>>`,
`probe()`), kinds: `ntfy` implemented; `telegram`, `gotify`, `pushover`,
`webhook` reserved. The ntfy impl POSTs `{server}/{topic}` with headers
`Title` (kind + worktree), `Priority` (Alert→`high`, Notice→`default`,
Info→`low`), `Tags` (kind), and `Authorization: Bearer <token>` when
configured. Uses the existing bounded reqwest client pattern
(`provider_http_client`-style connect timeout + a per-publish deadline).

**Runtime shape.** `NotifyState` sends accepted push jobs on an unbounded-in,
bounded-drain channel to one publisher worker (QoS `Background`); overflow
drops oldest with a counter (best-effort delivery — the inbox row is the
durable record). Retry: ≤2 with backoff, then drop. The worker never touches
the event loop; a delivery-failure status chip update, if shown, arrives via
the normal channel + `TerminalWaker` path. **Render damage: none new** (status
line updates are ordinary `Full`-on-chrome-change; nothing here recomposes on
pane output). CLI-pushed notifications (`thegn notify push`, control
`notify.push`) flow through the same decide path in the running host, so they
push too — one pipeline.

**Config** (`config/config.toml.example` documented, config-key recipe):

```toml
[notifications.push]
kind = "ntfy"                  # ntfy | reserved: telegram, gotify, pushover, webhook
server = "https://ntfy.sh"     # self-hosted strongly recommended (see Security)
topic = ""                     # required; treat as a capability URL
token = ""                     # SecretRef: env:NTFY_TOKEN / file:~/.config/thegn/ntfy-token
min_priority = "notice"        # channel floor, applied before rules
```

## Inbound: the guarded command inbox

**Placement.** The subscriber lives in the **daemon** process (already tokio;
survives UI detach — a phone command must not require the compositor to be
attached). `thegn serve`/plain daemon both run it. When `[daemon] enabled =
false`, the inbox is unavailable and doctor says so; it does not fall back
into the UI process.

**Transport.** SSE (`GET {server}/{topic}/sse`) with reconnect/backoff and
`since=` resume; long-poll `/json` as the degraded path. One topic for
commands, an optional second for replies.

**Envelope** (verification pure in `thegn-core`, table-tested):

```json
{
  "v": 1,
  "id": "<uuid>",
  "ts": 1756100000,
  "cap": "worktree.list",
  "params": {},
  "mac": "<hex hmac-sha256>"
}
```

- MAC over the canonical serialization of `(v,id,ts,cap,params)` with
  `inbox_secret` (SecretRef, **required** — enabling the inbox without it is a
  startup config error, not a warning).
- Freshness: `|now - ts| ≤ 300s`; replay: `id` must be unseen within the
  window (in-memory LRU sized to the window; no DB table, no schema bump — a
  daemon restart shrinks the window, acceptable because freshness still
  bounds it).

**Dispatch.** A verified envelope becomes one capability-catalog invocation
through the same dispatch the control API uses: admitted iff `cap` ∈
configured `allow` list AND `required_scope(cap)` ∈ configured `scopes`, with
**admin-scope capabilities refused unconditionally** regardless of config.
The inbox is thereby a new _door_ projecting the one catalog — no second
policy table, no bespoke verbs, no shell. Refusals and MAC/replay drops are
counted and visible in doctor/log; replies (when a reply topic is set)
publish `{id, ok, result}` truncated to a hard cap (8 KiB) so a listing can
never exfiltrate unbounded state.

**Config:**

```toml
[notifications.push.inbox]
enabled = false                # hard default off
topic = ""                     # separate from the outbound topic
inbox_secret = ""              # SecretRef; REQUIRED when enabled
allow = []                     # capability ids; empty = nothing callable
scopes = ["read"]              # ceiling; admin-scope caps always refused
reply_topic = ""               # optional
```

## Mosh and the phone terminal (feasibility judgment)

- **Panes**: already mosh (`TransportKind::Mosh` default for ssh placements;
  control reads stay ssh). Nothing to build.
- **`thegn serve` thin clients**: mosh is not a byte-pipe — SSP synchronizes a
  predicted terminal-grid state machine over UDP. The serve wire is HTTP/WS +
  gRPC; it structurally cannot ride mosh. Roaming/disconnect tolerance for
  thin clients is already specced as relay leases (`control-plane`
  "Persistent relay…"). **Decision: non-goal**, recorded here so it isn't
  re-litigated.
- **Supported phone path**: mosh app (Blink, Termius) → any reachable host →
  `thegn` (attach); the daemon keeps sessions warm across drops. Deliverable:
  a `docs/help/` mobile-access page (help-context key satisfied; keybindings
  page untouched) + a doctor note listing `mosh-server` presence beside the
  ssh/mosh transports. Pairs naturally with `add-tailnet-host-discovery`
  (THE-8) for reachability, without depending on it.

## Security

This section is load-bearing: the inbox is a **phone-initiated command
surface**.

- **Default off, twice.** Outbound push requires explicit `[notifications.push]`
  config; the inbox additionally requires `enabled = true`, a secret, and a
  non-empty allowlist. Empty allowlist = subscribed-but-inert is _not_ a
  state: enabling with `allow = []` is a config error naming the fix.
- **AuthN**: ntfy topic ACLs/tokens protect transport access, but topics are
  guessable-bearer by nature, so the envelope MAC is the real authenticator —
  a message is a command only if signed with `inbox_secret`. Secrets are
  SecretRefs; raw tokens in config are rejected by the existing config rules.
- **AuthZ**: allowlist ∩ `required_scope` ceiling ∩ unconditional admin-deny,
  enforced at the single catalog dispatch point. The blast radius of a stolen
  phone/topic+secret is exactly the allowlisted read-mostly capability set.
- **Confidentiality (residual risk, stated honestly)**: the ntfy server sees
  message plaintext (notification text out; command envelopes and replies
  in/out). Mitigations: self-hosted ntfy (documented as the recommended
  deployment, ideally reached over the user's tailnet), reply truncation, and
  never routing secrets into notification bodies. We do not roll our own E2E
  crypto layer.
- **Availability/DoS**: SSE reconnect backoff caps at minutes; per-minute
  execution cap on the inbox (excess dropped with a counter); MAC verification
  is cheap and constant-time compare.
- **Sandbox implications**: none — publisher and subscriber are host/daemon
  processes; nothing joins pane sandboxes or `thegn.slice`.

## Alternatives considered

- **Phone → control API directly** (existing pairing/token path): already
  specced and remains preferred where a network path exists; ntfy covers the
  NAT-bound, no-inbound-port case with stock apps. Complement, not overlap.
- **Telegram bot first**: reserved kind instead — ntfy is self-hostable and
  vendor-neutral; the seam keeps Telegram (AI 423) additive.
- **Persisted replay cache** (DB table + `user_version` bump): rejected;
  freshness window bounds replay across restarts.

## Open questions

- Whether the inbox's execution results should also land in the notification
  inbox as rows (auditability) — leaning yes via the existing `notify.push`
  path; decide at implementation.
- Priority→ntfy mapping for `min_priority` interaction with per-rule
  `set_priority` overrides (order of application is the router's existing
  one; confirm with a table test).
