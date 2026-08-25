# Event-feed subscriptions — what the Herdr socket API teaches

Linear: THE-34

## Why

Herdr (herdr.dev) drives a comparable multiplexer over a local socket API.
Studying its socket API against thegn's control plane
(`/v1/events` WS + SSE, `EventFrame` wire in `thegn-core/src/control_wire.rs`)
yields four adoptable ideas and three things thegn already does better:

Worth adopting:

1. **Subscription filters.** Herdr clients subscribe to typed events with
   per-resource filters (`{"type":"pane.agent_status_changed","pane_id":…}`).
   thegn's feed is all-or-nothing: every subscriber gets every frame kind for
   every session — a read-mostly monitor watching one agent still receives
   the whole instance's traffic.
2. **Snapshot + deltas as the bootstrap pattern.** Herdr's `session.snapshot`
   hands a client full initial state; events keep it current. thegn's
   `Sessions` frame is a _re-list poke_ — every list change makes every
   subscriber issue another `sessions.list` call (poll amplification), and a
   fresh subscriber races the poke.
3. **Loss is visible.** thegn's feed pumps silently _skip_ on broadcast lag
   (`RecvError::Lagged → continue`): a slow client loses events with no
   signal to resynchronize. Herdr clients at least know to re-bootstrap from
   a snapshot. (Neither system replays; parity there is fine — the fix is an
   explicit lag signal, not a journal.)
4. **Stable machine-readable error codes.** Herdr errors carry
   `code` + `message` (`not_found`, `invalid_params`, …). thegn's HTTP error
   body is `{"error": "<prose>"}` — clients must parse prose or switch on
   bare status codes, while `ControlError` already _is_ the enum.

Already better in thegn (keep, don't churn): scoped token auth (Herdr has
none — OS file permissions only); a binary wire that carries raw PTY bytes
without base64 bloat; `sessions.wait` with activity conditions (Herdr's
`agent.wait` equivalent, already landed including `OutputMatches`); a pinned
JSON schema (`thegn api schema` / `docs/api/control-v1.json`, matching
Herdr's `herdr api schema`).

## What Changes

- **Feed filters.** `GET /v1/events[?kinds=…&session=…]` (WS and SSE) and
  matching optional fields on the gRPC events request: `kinds` narrows frame
  kinds, `session` narrows session-keyed frames. Filters only narrow; absent
  filters mean today's full feed, byte-identical.
- **Bootstrap snapshot.** `?snapshot=1` sends, right after `Hello`, one
  `State` frame carrying the current session and worktree lists, so a client
  starts consistent instead of racing a re-list.
- **Lag signaling.** For subscribers that opt in, a lagged broadcast consumer
  receives a `Lagged{missed}` frame instead of a silent skip — the client's
  cue to re-snapshot. Legacy subscribers keep the silent-skip behavior.
- **Wire evolution without a proto bump.** New frame tags (`State`, `Lagged`)
  are sent only to connections that requested them (the decoder fatals on
  unknown tags, so opt-in is the compatibility mechanism); `Hello` gains an
  additive `features` field. `PROTO_VERSION` stays 1.
- **Stable error codes.** HTTP error bodies become
  `{"error": "<message>", "code": "<code>"}` — the `code` set is the
  `ControlError` vocabulary (`not_found`, `no_scope`, `conflict`,
  `unimplemented`, `internal`, plus `unauthorized`/`bad_request` from the
  adapters). Additive: the `error` string is unchanged. SSE frames set the
  `event:` field to the frame kind for browser `EventSource` ergonomics.
- **A CLI tail.** `thegn events tail [--kinds …] [--session …] [--json]`
  consumes the feed over the control socket; the `events.subscribe` catalog
  row gains the `Cli` surface (implemented in the same change — no new gap).

## Impact

- **Roadmap:** group **A 6** (front-door completeness — feed ergonomics for
  thin clients and supervisors).
- **Specs:** `control-plane` (ADDED: feed subscriptions, bootstrap snapshot,
  lag signaling, stable error codes, CLI tail).
- **Code:** `thegn-core/src/control_wire.rs` (new frames + a pure
  `FeedFilter` — unit-tested under the 95% gate),
  `thegn-svc/src/control/{http,grpc,client}.rs` (pump filtering, params,
  error bodies), the proto (additive fields), `thegn-host/src/cmd/` (the tail
  verb), `docs/api/control-v1.json` regeneration (snapshot-test pinned).
- **Related changes:** `complete-control-surface-coverage` (THE-39, sibling)
  owns the coverage ratchet the new `Cli` cell must satisfy and the plugin
  feed bridge that will consume these filters; `add-fleet-view` and any
  supervisor UI are the natural beneficiaries (not built on).
  `add-cli-namespaces-and-remote-open` owns the CLI grammar `events tail`
  must follow.
- **No DB change, no render-path change**; all work is daemon/svc/CLI side.
