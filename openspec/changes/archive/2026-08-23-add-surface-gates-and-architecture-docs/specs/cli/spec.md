## ADDED Requirements

### Requirement: Machine-readable output goes through one emitter

`--json` output SHALL be printed through `cmd::emit_json` (one compact document per invocation); files that print JSON any other way are pinned in `test/json-emit-ratchet.txt` (shrink-only) until routed through it or through a deliberate pretty emitter.

#### Scenario: New hand-rolled JSON

- **WHEN** a command prints `serde_json::to_string_pretty(..)` directly and its file is not pinned
- **THEN** `just lint`'s `json-emit` ratchet fails
