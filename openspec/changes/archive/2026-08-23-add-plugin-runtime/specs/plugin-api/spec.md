## ADDED Requirements

### Requirement: The runtime honours the wire contract

The shipped runtime SHALL speak exactly the v0.2 wire shapes (`RpcMessage`, `RpcResponse`, `RpcError`, callback method names) pinned by the schema snapshot, and the bundled example plugin (`examples/plugins/hello.sh`) SHALL load and register through the real loader + apply path in a test.

#### Scenario: The example plugin round-trips

- **WHEN** the golden test runs `examples/plugins/hello.sh` through `spawn_ndjson` and applies its messages
- **THEN** its `register` and `update` land a renderable view on its statusbar surface
