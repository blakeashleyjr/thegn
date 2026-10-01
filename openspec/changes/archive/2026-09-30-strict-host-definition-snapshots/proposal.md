# Strict persisted host-definition snapshots

THE-602 adds the bounded storage and checked-composition prerequisite for THE-592
launch configuration admission. Existing host_defs is best-effort: malformed
persisted JSON is omitted and permissive enum decoding can erase invalid input.
It must not become the authoritative source by relabeling its typed output.

Add a separate object-safe HostStore operation on an already-authorized SQLite
connection. Capture version, ordinary table schema and bounded raw rows in one
read transaction; validate duplicates and raw schema before HostConfig decoding.
Return typed source errors instead of an empty or partial successful registry.

Add a checked composition operation over the caller's already-layered Config and
the strict snapshot. Capture before composition, use THE-598's winning-host merge,
then enforce additive data/work limits and the existing shared schema/semantic
rules before returning immutable configuration data. Preserve explicit envs,
undefined-host fallback and disabled-section behavior. Fixed errors and redacted
Debug never expose config contents. The existing launch adapter remains THE-592.

## Impact

Roadmap O188 (configuration validation/error surfacing), A6 (shared core/store
seams), and AC363 through parent THE-592 (network-policy launch admission).
Core storage/decoder/composition modules, shared semantic checks, tests and
reciprocal THE-602 delivery metadata.
Related THE-598 preserves effective declarative host precedence. THE-603 owns
the separate opener/absence contract. No new dependencies, schema migration,
host startup wiring, provider execution or positive containment admission.
