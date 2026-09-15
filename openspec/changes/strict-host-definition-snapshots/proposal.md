# Strict persisted host-definition snapshots

THE-602 adds the bounded storage prerequisite for THE-592 launch configuration
admission. Existing host_defs is intentionally display-oriented: malformed
persisted JSON is omitted and permissive enum decoding can erase invalid input.
It must not become the authoritative source by relabeling its typed output.

Add a separate object-safe HostStore operation on an already-authorized SQLite
connection. Capture version, ordinary table schema and bounded raw rows in one
read transaction; validate duplicates and raw schema before HostConfig decoding.
Return typed source errors instead of an empty or partial successful registry.

## Impact

Roadmap O188 (configuration validation/error surfacing), A6 (shared core/store
seams), and AC363 through parent THE-592 (network-policy launch admission).
Core storage/decoder modules, tests and reciprocal THE-602 delivery metadata.
Related THE-598 preserves effective declarative host precedence. THE-603 owns
the separate opener/absence contract. No new dependencies, schema migration,
host startup wiring, provider execution or positive containment admission.
