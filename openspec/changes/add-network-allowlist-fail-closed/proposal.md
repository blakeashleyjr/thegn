## Why

The egress programme needs `network = "allowlist"` to exist in config before a
packet-enforced datapath does. A new network variant must fail closed so it can
never mean unfiltered direct networking (THE-2119, WS04-P1-04).

## What Changes

- `[sandbox] network` accepts `allowlist` (aliases `filtered`, `egress-proxy`).
- Until a datapath exists, every resolved launch with `allowlist` behaves as
  `network = "none"`; the spec records `egress_requested`, fingerprinted only
  when true, so existing fingerprints are unchanged.
- Config-level readers use `Network::resolved()`, and the repo overlay ranks
  `allowlist` between `nat` and `none` when deciding whether network changes
  weaken isolation.
- Out of scope: the egress config schema, presets, and datapath.
