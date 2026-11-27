# Tasks — network allowlist, fail closed

- [x] 1. Add `Network::Allowlist` with aliases and `Network::resolved()`.
- [x] 2. Lower to `none` during resolution and in `lower_universal_deny`; record
     `SandboxSpec.egress_requested` and fingerprint it only when true.
- [x] 3. Fail-closed arms in exhaustive network matches and network ranking.
- [x] 4. Resolve config-level network readers through `Network::resolved()`.
- [x] 5. Update the config example, Home Manager module, tests, and help pages.
