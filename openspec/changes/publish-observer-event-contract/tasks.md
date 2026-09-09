# Tasks — observer event contract (THE-105)

- [ ] Inventory every frame kind and identify its actual producer/transport.
- [ ] Define separate canonical observer and pane-attach kind sets in core.
- [ ] Validate observer filters against only monitor-emittable kinds on HTTP,
      SSE, WebSocket, gRPC, and CLI paths.
- [ ] Return stable bad-request codes and the supported set for attach-only or
      unknown observer kinds.
- [ ] Document list/subscribe/re-list bootstrap, race, stable-id reconciliation,
      lag recovery, and attach snapshot/delta separately.
- [ ] Regenerate control schemas and client-facing API references.
- [ ] Add cross-transport tests proving every accepted observer kind is
      producible and attach-only names are rejected.
- [ ] Verify unfiltered observers and pane attach streams remain compatible.
- [ ] Run strict OpenSpec, schema, transport, CLI, and full repository gates.
