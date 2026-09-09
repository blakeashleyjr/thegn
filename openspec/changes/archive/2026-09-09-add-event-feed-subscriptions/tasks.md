# Tasks — event-feed subscriptions (THE-34)

- [x] Add kind and session filters to WebSocket and SSE subscriptions.
- [x] Project equivalent filters to gRPC.
- [x] Reject unknown filter kinds at subscription time.
- [x] Preserve unfiltered legacy behavior and authorization narrowing.
- [x] Add opt-in lag frames without sending new tags to non-opted clients.
- [x] Add stable machine-readable HTTP error codes from the shared taxonomy.
- [x] Add `thegn events tail` with human/NDJSON output and no-daemon handling.
- [x] Cover filter, lag, error-code, transport, and CLI behavior with tests.
- [x] Remove the rejected synthetic state-snapshot promise.
- [x] Separate observer vocabulary/bootstrap truthfulness into THE-105.
- [x] Reconcile and strictly validate the accepted delta.
