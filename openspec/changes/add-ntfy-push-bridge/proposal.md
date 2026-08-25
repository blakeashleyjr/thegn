# Add ntfy mobile push (bidirectional) and the mobile-access story

Linear: THE-12

## Why

thegn's notification stack is deep on the desktop — rules engine, DND, modes,
priorities, desktop toasts, inbox (`openspec/specs/notifications`) — but stops
at the machine's edge: roadmap **AI 422 "Push to phone (ntfy)"** is open, and
the specced mobile companion (`control-plane` "Mobile companion…") presumes a
paired thin client with a network path to the daemon. ntfy is the
infrastructure-free complement: a store-and-forward pub/sub HTTP server
(self-hostable) with stock mobile apps, reaching a phone with **no companion
app and no inbound port** — and, subscribed in the other direction, giving the
phone a way to hand commands back (the multi-agent-shogun pattern the issue
cites). The issue's second bullet, Mosh, is largely **already built**: `mosh`
is the default interactive-pane transport for ssh placements
(`thegn_core::placement`, J 122 done) — what remains is judging it honestly
for the thin-client path and documenting the phone-terminal route.

## What Changes

- **Outbound: a `push` delivery channel** in the notification router.
  `RouteDecision` gains a push flag governed by the same rules/DND/mode/
  priority machinery as every other channel (`channels` selectors accept
  `push`). Delivery is a **push-provider seam** (`thegn_svc`), kind `ntfy`
  implemented first; `telegram` (AI 423), `gotify`, `pushover`, `webhook`
  reserved. The ntfy publisher POSTs to `server`/`topic` off-thread
  (best-effort: bounded retry, drop-on-overflow, never blocks the loop), maps
  Alert/Notice/Info to ntfy priorities, and authenticates via a SecretRef
  token (`env:`/`file:` — never a raw token in config). Probe in `thegn
doctor`.
- **Inbound: a guarded command inbox, off by default.** When
  `[notifications.push.inbox]` is explicitly configured, the **daemon**
  process (its own tokio runtime — never the UI loop) subscribes to a
  separate command topic over SSE. Each message is a signed JSON envelope
  (`{v, id, ts, cap, params, mac}`, HMAC-SHA256 over a canonical form with a
  required SecretRef secret). A verified message maps to **one capability-
  catalog invocation**, admitted only when the capability id is in the
  configured `allow` list AND `required_scope(verb)` is within the configured
  scope set — the same catalog dispatch the control API uses, never a second
  policy table, and never a shell command. Replays (seen id / stale ts) and
  bad MACs are dropped and counted. Optional reply topic publishes truncated
  results.
- **Mosh / phone terminal: judged and documented, not rebuilt.** Feasibility
  against the daemon PTY model: mosh's SSP protocol synchronizes a predicted
  terminal grid — it cannot carry the `thegn serve` control wire (HTTP/WS +
  gRPC), so "mosh transport for serve" is a non-goal; thin-client roaming is
  already owned by the relay-lease requirement (`control-plane`). The
  supported phone-terminal path — mosh client app (Blink/Termius) → host →
  `thegn` attach, sessions surviving via the daemon — becomes a `docs/help/`
  mobile-access page plus a doctor note when `mosh-server` is absent.

## Impact

- **tasks.md**: AI 422 (ntfy push) in full; the reserved `telegram` kind stubs
  AI 423; J 130 (mobile client attach) gets its documented path; J 122 (mosh)
  referenced as done.
- **Specs**: delta on `notifications` (push channel seam) and `control-plane`
  (command inbox as a new door projecting the existing catalog).
- **Crates**: `thegn-core` (route decision + envelope verification, pure,
  unit-tested under the 95% gate), `thegn-svc` (ntfy publisher + subscriber,
  reqwest), `thegn-host` (NotifyState wiring, daemon subscriber task, doctor,
  help page).
- **DB**: no schema change expected; the replay cache is in-memory with a
  bounded window (a persisted variant would need a `user_version` bump — noted
  in design, not chosen).
- **Related in-flight changes**: `make-daemon-default` /
  `add-runtime-session-split` (the daemon is the inbox's home; inbox requires
  a running daemon and says so), `add-osc-attention-signaling` (outbound
  attention over OSC — a sibling channel, no overlap in mechanism),
  `add-fleet-view` (do **not** build on it — excised-proxy dependency),
  `add-cli-namespaces-and-remote-open` (`thegn notify` verbs unchanged). The
  separate in-flight **MCP write-tools with scope gating** branch is a
  dependency for which capabilities are safely invokable — the inbox reuses
  `required_scope`, re-scoping nothing.

## Non-goals

- A thegn mobile app (the control-plane mobile-companion requirement stands on
  its own; ntfy is the app-free complement).
- Mosh transport for `thegn serve` / the control wire.
- End-to-end message encryption on the ntfy channel (residual risk documented
  in Security; mitigation is self-hosting/tailnet, not a crypto layer of our
  own).
- Free-form remote shell execution from the phone — the inbox is capability-
  catalog-only, allowlisted, and off by default.
