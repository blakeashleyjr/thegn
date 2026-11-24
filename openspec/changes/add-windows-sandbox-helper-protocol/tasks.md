# Tasks — Windows sandbox helper protocol

## 1. Crate

- [x] 1.1 Serde-only request, probe, status and peer wire types (protocol
      version 1, `deny_unknown_fields` on the request).
- [x] 1.2 Bounded UTF-8 environment chunking and a decoder that scrubs every
      request prefix before validating.
- [x] 1.3 Pure Windows argv `join`/`split` fixtures.
- [x] 1.4 Workspace wiring: member, workspace dependency, `check-cross` leaf,
      `crate_boundaries` edge, ARCHITECTURE row, crate README.

## 2. Windows evidence (other issues)

- [ ] 2.1 WS06-P6-01: compare the argv fixtures with Wine's real
      `CommandLineToArgvW`.
- [ ] 2.2 WS06-P1-06: repeat the comparison on Windows CI and verify the helper
      contract (version rejection exits 3, `.bat`/`.cmd` refusal exits 5).
