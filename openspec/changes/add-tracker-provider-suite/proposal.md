# Tracker / issue-provider seam audit and cheap gaps

Linear: THE-50

## Scope

Audit the tracker seam against `CLAUDE.md`, `docs/ARCHITECTURE.md`, and
`openspec/specs/provider-seams`, then close only the local gaps that the audit
can prove are cheap and independently testable:

- record the live Linear, GitHub Issues, Jira, and Kaneo implementation matrix;
- make optional issue operations typed and caps-driven;
- extend offline conformance to issue capabilities and the plugin bridge;
- expose configured issue caps in doctor without network probing;
- document how to add a native or plugin tracker provider.

The branch contains `IssueBackend`/`IssueRouter`, not the draft's proposed
`TrackerBackend`/`TrackerCaps` tier model. Existing plugin provider composition
already works for the core issue operations. The implementation must preserve
that path and must not add a new external door.

## Explicit non-goals

Notion, Plane, generic tracker tiers, Kaneo's board/project downcast removal,
Jira parity expansion, GitHub comment/label expansion, generic tracker login,
spec-linking, dispatch seeding, and extra plugin wire operations are follow-ups.
No generic user-configured command provider is added. A `jira-cli` adapter is
not a one-file addition because Jira already has a native REST backend and a
selectable CLI implementation would need config, factory, probe, error/output
contracts, and tests.

BMAD-METHOD, OpenSpec, and spec-kit are documentation/skills concerns only.
THE-20's embedded-skills design is the home for reviewed SDD recipes; this
change adds no scanner, parser, agent dependency, or code integration.

## Impact

No new config key, environment overlay, database schema, capability-catalog
row, completion slot, or help action. The existing generic contribution
`caps` field is interpreted for issue providers; the plugin wire mechanism is
unchanged and requires no API version bump.

The detailed file-level design and coder chunks are in
`.thegn/pipeline/THE-50/architect/` and `.thegn/pipeline/THE-50/code/`.
