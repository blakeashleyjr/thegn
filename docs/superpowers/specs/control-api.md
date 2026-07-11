# The thegn control API (v1)

The contract a thin client — desktop, web, or the mobile companion — builds
against. One service seam (`thegn_svc::control::ControlApi`, implemented by
the pane daemon) exposed over three transports:

- **HTTP + WebSocket** (primary): the routes below, served on the daemon's
  unix socket (local; same-uid peers are implicitly admin unless
  `[serve] local_admin = false`) and on `thegn serve`'s TCP listener
  (bearer token **required**).
- **SSE** (`GET /v1/events/sse`): the same feed as JSON envelopes
  (pane bytes base64) — a curl-friendly convenience; WS is primary.
- **gRPC** (`thegn.control.v1.Control`, same TCP port): a mechanical
  mirror for external tooling. See
  `crates/thegn-svc/proto/thegn/control/v1/control.proto`.

Transport security (v1): the TCP listener is **plaintext** — bind it to a
trusted network (tailscale/wireguard) or tunnel over `ssh -L`. Every request
is still token-gated. The pairing-URL format reserves `fp=<cert-fingerprint>`
so TLS + pinning lands later without a format break.

## Scopes

Every token holds a scope set (csv in `pairings.scope`). The verb→scope table
is `thegn_core::control::required_scope` — the single tested policy source.

| Scope   | Grants                                                                                 |
| ------- | -------------------------------------------------------------------------------------- |
| `read`  | list sessions/leases, snapshots, `/v1/me`, the event feed, git status                  |
| `write` | open/attach/detach/kill sessions, terminal input, resize, open-worktree, drive-browser |
| `git`   | stage + commit through the GitBackend seam (implies read, **not** write)               |
| `admin` | pairing management, daemon shutdown (implies everything)                               |

`git` deliberately does not imply `write`: a companion that can commit must
not be able to type into terminals. Read-only views require only `read` —
an under-scoped request is rejected **before any action runs** (403 /
`PERMISSION_DENIED`).

## Tokens & pairing

- Control token: `szc1_<id:8hex>_<secret:64hex>` — the bearer credential
  (`Authorization: Bearer …` or `x-api-key`). Only `sha256(secret)` is stored.
- Pairing code: `szp1_…` — single-use, short-TTL, embedded in a pairing URL:
  - app scheme: `thegn://pair?host=H&port=P&t=szp1_…[&fp=…]`
  - web form: `http://H:P/pair#t=szp1_…` (fragment ⇒ never in server logs)
- Redeem: `POST /v1/pair {code, label}` (unauthenticated — possession of the
  single-use code is the credential) → `{token, pairing_id, scopes, approved}`.
  With `[serve] require_approval` the token parks (`approved: false`) until
  `thegn pair approve <id>` / an in-app approval.
- Revoke: `DELETE /v1/pairings/{id}` (admin) or `thegn pair revoke <id>`.

## Routes

| Route                                                                                 | Verb scope | Notes                                                                                                               |
| ------------------------------------------------------------------------------------- | ---------- | ------------------------------------------------------------------------------------------------------------------- |
| `GET /health`                                                                         | —          | liveness                                                                                                            |
| `POST /v1/pair`                                                                       | —          | redeem a pairing code (single-use, atomic)                                                                          |
| `GET /v1/me`                                                                          | read       | `{pairing_id, label, scopes}` — scope switching is client-side token selection                                      |
| `GET /v1/sessions`                                                                    | read       | list daemon sessions (worktree hints, geometry, lease state)                                                        |
| `POST /v1/sessions`                                                                   | write      | open a session `{argv, cwd?, env?, rows, cols, worktree?}`                                                          |
| `GET /v1/sessions/{s}/snapshot`                                                       | read       | `{seq, rows, cols, ansi_b64}` — the warm screen as an ANSI repaint                                                  |
| `POST /v1/sessions/{s}/input`                                                         | write      | `{b64\|text, enter?}`                                                                                               |
| `POST /v1/sessions/{s}/resize`                                                        | write      | `{rows, cols}`                                                                                                      |
| `POST /v1/sessions/{s}/detach`                                                        | write      | `{client_id}`; last client out opens a relay lease                                                                  |
| `GET /v1/sessions/{s}/attach`                                                         | write      | WS upgrade (`?client_id&rows&cols&observer`); binary `EventFrame`s down, raw-binary stdin / JSON `{type:resize}` up |
| `DELETE /v1/sessions/{s}`                                                             | write      | kill the PTY                                                                                                        |
| `GET /v1/events`                                                                      | read       | WS: the broadcast feed (activity/lease/pairing/session-list)                                                        |
| `GET /v1/events/sse`                                                                  | read       | same feed, JSON envelopes                                                                                           |
| `GET /v1/leases`                                                                      | read       | relay leases (detached sessions kept warm)                                                                          |
| `POST /v1/worktrees/open`                                                             | write      | `{repo, branch?}` → the running compositor's intent mailbox                                                         |
| `POST /v1/browser`                                                                    | write      | **reserved**: v1 always 501                                                                                         |
| `GET /v1/git/status?worktree=`                                                        | read       | porcelain codes per changed file                                                                                    |
| `POST /v1/git/stage`                                                                  | git        | `{worktree, paths}` — GitBackend seam; git stays source of truth                                                    |
| `POST /v1/git/commit`                                                                 | git        | `{worktree, message}` → `{commit}`                                                                                  |
| `GET /v1/merge/list?worktree=`                                                        | read       | `{queue}` — the repo's merge-queue rows (scoped to the worktree's repo)                                             |
| `POST /v1/merge/add`                                                                  | git        | `{worktree}` → `{queued, message}` — enqueue the worktree's current branch                                          |
| `POST /v1/merge/clear`                                                                | git        | `{worktree}` → `{cleared}` — empty the queue for the worktree's repo                                                |
| `GET/POST /v1/pairings`, `DELETE /v1/pairings/{id}`, `POST /v1/pairings/{id}/approve` | admin      | pairing lifecycle                                                                                                   |
| `POST /v1/push/register`                                                              | —          | **reserved** for push notifications (AI 422/423); absent in v1                                                      |

## The event wire

WS binary messages carry `thegn_core::control_wire::EventFrame`
(`[tag:u8][len:u32 BE][payload]`, 1 MiB cap): `Hello` (proto version, server,
your scopes), `PaneSnapshot` (ANSI repaint at `seq`), `PaneDelta` (raw PTY
bytes, `seq`), `Activity` (JSON), `Lease` (opened/refreshed/released/reaped),
`Pairing` (requested/approved/revoked), `Sessions` (re-list), `SessionExit`.

Warm-attach contract: the snapshot at `seq` folds all output through `seq`;
the first live delta carries `seq + 1` — no gap, no overlap. A slow consumer
is never allowed to block the PTY: its deltas drop and a fresh snapshot
resyncs it.

## Companion surface (read-mostly)

Monitor: `GET /v1/sessions` + `/v1/leases` + the event feed, and `snapshot`
for screens — all under `read` alone. Act: `git stage/commit` under `git`.
Accounts/scopes: hold one token per pairing and switch client-side; `/v1/me`
reflects the presented token. Push: reserved (`/v1/push/register`), not in v1.

The scope-enforcement guarantees are pinned by
`crates/thegn-svc/src/control/tests.rs` (router-level, with a recording
fake proving rejected requests perform zero actions).
