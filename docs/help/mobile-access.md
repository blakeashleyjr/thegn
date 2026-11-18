---
id: mobile-access
title: Mobile access
order: 17
---

# Mobile access

Three ways to reach a running thegn from your phone, none of which needs a
thegn mobile app.

## Push to phone (ntfy)

thegn's whole notification stack — rules, do-not-disturb, priorities — can
deliver to your phone as a **push** channel. It uses
[ntfy](https://ntfy.sh): a store-and-forward pub/sub server (self-hostable)
with stock mobile apps, so a notification reaches your phone with **no
companion app and no inbound port**.

Enable it under `[notifications.push]`: choose a named `ntfy`, generic
`webhook`, `discord`, or `slack` sink. `telegram`, `gotify`, and `pushover`
remain reserved. For ntfy, configure a server and an unguessable topic, then
subscribe to that topic in the ntfy app.

Push is just another channel: the same `[[notifications.rules]]`, DND, and
per-kind priorities decide what reaches you. Add `"push"` to a rule's
`route` to include or exclude it; `min_priority` is the channel floor.
Delivery is best-effort (bounded retry, dropped on overload) — the inbox
row is always the durable record. The token is a **SecretRef**
(`env:NTFY_TOKEN` / `file:…`), never a raw token in config.

> The ntfy server sees your notification text. Self-host it and reach it
> over your tailnet; keep secrets out of notification bodies.

## Commands from your phone (the command inbox)

Subscribed the other way, ntfy lets the phone hand commands back. The
**command inbox** is a daemon-hosted, signed-command surface — and it is
**off by default**. It exists so a phone can run a small, allowlisted set
of read-mostly capabilities (list worktrees, check PR status, …) without a
network path into the daemon.

It is deliberately narrow and defense-in-depth:

- **Off by default, and refuses to half-enable.** Turning it on requires a
  SecretRef `inbox_secret` and a non-empty `allow` list; a raw token, a
  missing secret, or an empty allow list is a startup configuration error,
  not a silent no-op.
- **Every message is a signed envelope.** A command runs only if its
  HMAC-SHA256 verifies against `inbox_secret`, its timestamp is fresh, and
  its id has not been seen (replay protection). The topic is guessable by
  nature, so the signature — not the topic — is the authenticator.
- **Allowlist ∩ scope ceiling ∩ admin-deny.** An accepted command still
  runs only if its capability is in `allow` **and** its required scope is
  within `scopes`. Admin-scoped capabilities are refused **unconditionally**,
  regardless of config.
- **Catalog dispatch only — never a shell.** A command maps to exactly one
  capability-catalog entry, dispatched through the same path the control
  API uses (`thegn api list` shows the catalog).
- **Bounded blast radius.** A per-minute execution cap throttles a stolen
  topic+secret; replies (to an optional `reply_topic`) are truncated so a
  listing can't exfiltrate unbounded state.

The inbox lives in the pane daemon (so a phone command does not need an
attached UI); it is unavailable when `[daemon] enabled = false`. See
[[daemon-and-sessions]].

## Answering approvals from your phone

When push is configured, every new approval request (a lane waiting to
land, a proposed automation start, a rule asking to be armed, an outward
action) is pushed with the title **thegn approval**. The body is the
daemon's own trusted description of what you are approving (never the
requester's free-text reason) and a reply line:

```text
Reply: approvals.redeem {"nonce":"<32 hex>","decision":"approve|reject"}
```

The nonce is 128 bits of OS randomness, single use, and valid for 24
hours. The daemon stores only its sha256, bound to that one request.

`approvals.resolve` is Admin, so the inbox refuses it unconditionally. The
phone answers through `approvals.redeem` instead, a Write capability that
only resolves the request its nonce was minted for. To enable it, add it to
the allowlist and give the inbox a `write` ceiling:

```toml
[notifications.push.inbox]
enabled = true
inbox_secret = "env:THEGN_INBOX_SECRET"
allow = ["approvals.redeem"]
scopes = ["read", "write"]
```

The phone then publishes a signed envelope to the inbox topic:

```text
{ "v": 1, "id": "<unique>", "ts": <unix secs>, "cap": "approvals.redeem",
  "params": { "decision": "approve", "nonce": "<32 hex>" },
  "mac": "<hex hmac-sha256>" }
```

The MAC is the usual inbox MAC: HMAC-SHA256 under `inbox_secret` over
`v\nid\nts\ncap\n<params>`, with `params` as compact, sorted-key JSON.
`params` takes exactly `nonce` and `decision`; any other field is refused.

An answer needs both halves: the inbox secret (to sign) and the nonce (only
in the push topic). An unknown, used or expired nonce is refused with
"unknown, used or expired approval code", so a replayed envelope cannot
resolve twice even across a daemon restart. The decision is recorded with
approver `push` and has the same effect as `approvals.resolve`. Agents
cannot redeem: the capability is never exposed over MCP, and every token
caller over HTTP, the Lead included, is refused. The only remote path is
the signed inbox envelope. Never let an agent read the inbox secret: thegn
hides `env:` and `file:` refs for it and the push tokens from the Lead.

## A terminal on your phone (mosh)

For a full interactive session, use a mosh client app (Blink, Termius) to
ssh/mosh into the host and run `thegn` there. mosh tolerates roaming and
sleep, and the pane daemon keeps your sessions warm across drops — so a
dropped connection reattaches to the same live screen, not a replay.

This is the supported phone-terminal path, and it needs `mosh-server` on
the host (`thegn doctor` reports whether it is present). Note that mosh is
a terminal transport only: it **cannot** carry the `thegn serve` control
wire, so "mosh for serve" is a non-goal — thin-client roaming is handled by
the control plane's relay leases instead.

## Check it

`thegn doctor` has a **Mobile access** section: the outbound push target,
the command-inbox status (off / on with its allowlist size and scope
ceiling / a named config error), and whether `mosh-server` is installed.
The push provider also appears in the doctor **Providers** list.

See [[configuration]] for every key, and [[cli]] for `thegn api list`.
