# Design — event-feed subscriptions

## Decisions

1. The daemon retains one broadcast source. Each transport narrows a
   subscription in its connection pump.
2. Filters may only remove frames. Authorization remains the catalog/scope
   boundary.
3. Unknown filter names fail at subscription time rather than producing a
   silently empty stream.
4. Lag signaling is opt-in so older decoders do not receive a new tag.
5. HTTP error bodies add a stable code while retaining the human message; gRPC
   and plugin projections map from the same closed taxonomy.
6. No `State` frame or `snapshot=1` protocol was delivered. Observer clients
   bootstrap with existing list calls and re-list on coarse state-change events.

## Known boundary

The shared frame vocabulary currently includes pane-attach-only names that a
generic observer stream cannot emit. THE-105 owns splitting or documenting
those vocabularies and specifying the bootstrap contract. It is not hidden in
this completed change.

## Runtime

All pumps wait on broadcast/socket readiness. Filtering, lag mapping, and CLI
formatting add no polling and no UI-loop work.
