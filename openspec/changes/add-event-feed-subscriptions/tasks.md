# Tasks — event-feed subscriptions

## 1. Pure wire + filter logic (thegn-core, coverage-gated)

- [ ] 1.1 `EventFrame::State { sessions, worktrees }` and
      `EventFrame::Lagged { missed }`: tags, encode/decode, round-trip +
      malformed-payload tests beside the existing wire tests.
- [ ] 1.2 `Hello.features: Vec<String>` (serde-default) + a test that a
      featureless `Hello` still decodes (old peer compatibility).
- [ ] 1.3 `FeedFilter`: parse (`kinds`, `session`, `snapshot`, `lag`) from
      key/value pairs; `matches(&EventFrame) -> bool`; unknown kind names are
      an error; exhaustive unit tests (every frame kind × filter).
- [ ] 1.4 Frame-kind naming: one function shared by `frame_json`, the SSE
      `event:` field and `FeedFilter` so the vocabularies cannot drift
      (pinned by a test).

## 2. HTTP surface (thegn-svc)

- [ ] 2.1 `events_ws` / `events_sse`: parse `FeedFilter` from query params
      (`bad_request` with a clear message on an invalid filter); apply in the
      pump; send `State` after `Hello` when `snapshot=1`; send `Lagged` on
      broadcast lag when opted in, silent-skip otherwise.
- [ ] 2.2 SSE: set the `event:` field to the frame kind.
- [ ] 2.3 Error bodies: add `code` beside `error` in `error_json` /
      `ControlError` mapping; update control tests; regenerate
      `docs/api/control-v1.json` (snapshot test).
- [ ] 2.4 Integration tests: filtered WS sees only requested kinds; legacy
      (param-less) connection is byte-identical to today; a lagging opt-in
      consumer receives `Lagged`.

## 3. gRPC mirror (feature `control-grpc`)

- [ ] 3.1 Additive request fields on the events RPC (kinds, session,
      snapshot, lag) mapped to `FeedFilter`; same pump semantics.
- [ ] 3.2 Mirror test for filter + snapshot behavior.

## 4. CLI tail (thegn-host)

- [ ] 4.1 `thegn events tail [--kinds …] [--session …] [--snapshot] [--json]`
      over the control socket (SSE consumer path in the control client);
      graceful no-daemon message; `--json` through the one emitter.
- [ ] 4.2 Catalog: add `Cli` to the `events.subscribe` row's surfaces; grow
      `cli_control_caps()`; per-surface coverage tests stay green with no new
      `SURFACE_GAPS` entry (coordinate with the
      `complete-control-surface-coverage` ratchet if it has landed).
- [ ] 4.3 Smoke: `thegn events tail --json` against a live daemon emits the
      hello line.

## 5. Finish

- [ ] 5.1 Typed control client (`control/client.rs`): expose the filter
      options so thin clients and the plugin feed bridge (sibling change) can
      request them.
- [ ] 5.2 Docs: note the feed features in the control-plane section of
      `docs/ARCHITECTURE.md`'s capability table if it names the feed.
- [ ] 5.3 Run `just ci` once (includes openspec-validate) as the pre-PR gate.
