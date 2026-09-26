# Primary review + greenlight — THE-281

Reviewing row 574's investigation. **APPROVED to implement.**

Confirmed on this branch: `origin_authority` at `client.rs:1195-1207` still
brackets `url.host_str()` for IPv6 — which is already bracketed — and both
`subscribe_events_opts` and `attach_opts` use it for the WebSocket `Host`
header, while unary HTTP goes through `origin_request_url` and is therefore
unaffected. That split is exactly why this reached production.

## Your finding — `url` 2.5.8 rejects scoped IPv6 zone IDs

Accepted, and it settles the scope: if the crate will not parse
`https://[fe80::1%25eth0]/`, a scoped address can never reach this function, so
there is nothing to serialize and nothing to test.

Do **not** add zone-id support, a second parser, or a dependency. Instead:

- Drop the "scoped IPv6 forms where supported" acceptance item as **not
  applicable at this `url` version**, and say so in your artifact with the
  version pinned.
- Add a test asserting the _parse_ is rejected, so if a future `url` starts
  accepting zone IDs this lane's assumption fails loudly instead of silently
  producing a wrong authority.

## Restated decisions

- Derive the authority from the typed host (`Url::host()` → the `Host` enum), not
  a `contains(':')` heuristic. The heuristic is the bug; do not make it more
  careful.
- Append the port only when explicit / non-default for the scheme (`wss`/`https`
  443, `ws`/`http` 80). No `:443` on a default-port `wss` origin.
- Unary and WebSocket must resolve to the same endpoint — assert it, since the
  divergence is what hid this.

## Tests

`https://[::1]:8443`, `wss://[::1]` (default port), IPv4 with and without a
port, DNS with and without a port, plus the zone-id rejection above. Assert the
exact authority string, not just "contains".

## Scope

`crates/thegn-svc/src/control/client.rs` and its tests. No protocol change, no
new dependency.
