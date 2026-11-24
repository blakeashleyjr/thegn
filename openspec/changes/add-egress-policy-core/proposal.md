# Egress policy core

Linear: THE-2112

## Why

Egress host rules are written in several places (repo clamp, zones, proxy,
providers, credentials). They need one grammar, because once users write lists
in it, changing what an entry means silently narrows or widens their policy.

## What changes

- Extend `thegn_core::egress::pattern` into the one host-pattern grammar: bare,
  `*.x`, `=x` and `*` entries with optional port suffixes, IDNA normalisation
  and IP-literal refusal.
- Classify and validate names after percent-decoding and IDNA mapping, so a
  pattern keeps its meaning across a write-then-parse round trip.
- A block entry without a port suffix blocks every port.
