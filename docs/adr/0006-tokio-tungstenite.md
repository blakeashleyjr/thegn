# ADR-0006: Keep tokio-tungstenite aligned with axum

- Status: Adopted / keep
- Date: 2026-08-29
- Scope: Control-plane WebSocket clients and provider WebSockets

## Context

`thegn-svc` directly declares `tokio-tungstenite 0.29`
(`crates/thegn-svc/Cargo.toml:53-55`). The control client uses
`client_async` for event subscription and warm attach over Unix/TCP
(`crates/thegn-svc/src/control/client.rs:470-517,526-600`), while the Sprites
provider uses WSS for native exec and a TCP-over-WebSocket proxy
(`crates/thegn-svc/src/provider.rs:1311-1405`). The axum control server supplies
the WebSocket routes alongside SSE (`crates/thegn-svc/src/control/http.rs:1406-1468,1554-1570`).
The lock shows axum and the direct client on one `tokio-tungstenite 0.29`
cohort (`Cargo.lock:428-451,8439-8476,8675-8688,9060-9073`).

The workspace pin disables defaults and selects `connect`, `handshake`, and
`rustls-tls-webpki-roots` (`Cargo.toml:153-160`) to stay with the existing
rustls stack. This is a service-edge dependency; it never belongs in
`thegn-core`.

## Decision

Adopt / keep. Tungstenite is required for the existing async client and keeps
the server adapter compatible with axum. Replacing it with another WebSocket
stack would add an adapter and risk a second implementation. Binary size and
MSRV 1.89 are already paid costs; the main maintenance risk is version skew.
The Linux service and musl bridge, macOS service builds, and mingw/Windows
workspace must retain the current static-rustls and target behavior. No new
platform-specific binary cost is justified by changing an already shared
service dependency.

## Reopen condition

Track axum's tungstenite major on every update. Change the direct pin and axum
cohort together, inspect `cargo tree --target all -i tokio-tungstenite` and
`-i tungstenite`, and preserve service-task ownership of all network work.
No transport behavior changes are part of this record.
