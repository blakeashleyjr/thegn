# Chunk 2 — tracker-provider extension documentation

## Files touched

- `docs/extending/tracker-provider.md` (new)
- `docs/extending/README.md`
- `docs/extending/provider-impl.md`
- `docs/extending/plugin.md`
- `docs/help/plugins.md`

No Rust, config, OpenSpec spec, or generated API file is touched by this
chunk. The docs must describe the live `IssueBackend` seam, not the draft
`TrackerBackend` model.

## Approach

1. Add a self-contained tracker-provider recipe covering account-shaped
   registration, factory wiring, object-safe `BoxFuture` methods, the new
   `IssueCaps` bits and typed `Unsupported` errors, `Probe`/doctor, offline
   conformance, provider-local tests, secret refs, and off-loop I/O.
2. Include a plugin-provider recipe with `IssueProvider`, the existing
   contribution `caps` object, `provider.call` request shape, account labels,
   timeout/degradation behavior, and `thegn plugin list/check`. State that
   omitted caps are all false and false-cap operations do not round-trip.
3. Include a CLI-backed-provider subsection: use a concrete implementation
   module, explicit argv (never shell interpolation), bounded output/timeouts,
   directory anchoring, provider-local vendor binary calls, typed errors,
   probe, factory, and tests. Explain why a future `jira-cli` adapter is not
   added now: native Jira REST already exists, and a selectable adapter needs
   more than one file. Do not document a generic user-configured command key.
4. Link the recipe from the existing extending index and cross-link the
   generic provider and plugin recipes. Update the user-facing plugin help only
   to accurately mention provider caps/wire behavior; do not invent a new help
   action or config key.

Keep the docs aligned with `CLAUDE.md`, `docs/ARCHITECTURE.md`,
`docs/extending/provider-impl.md`, and the actual source citations in the
architect design. Do not claim doctor probes unconfigured or live plugin
providers. Do not promise Notion, Plane, Kaneo generic tiers, spec-linking, or
SDD code in this issue.

## Overlap and dependency

This chunk is independent of Chunk 1 and touches no Chunk 1 file. It may run in
parallel. It should describe the API that Chunk 1 lands, but does not require
Chunk 1 to be merged to edit the documentation; if Chunk 1 changes a public
name, update this chunk's docs in the same commit before committing.

## Tests to run

Documentation-focused checks plus the required scoped consumer checks:

- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc conformance`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host plugin_providers`

Also run the repository's focused docs/help ratchet test if its filter is
available in the crate. Do not run `just test`, `just ci`, e2e, or a full
workspace build. No `thegn` invocation is expected; if one is used, set
`XDG_STATE_HOME` to a newly created temporary directory.

## Done criteria

- `docs/extending/tracker-provider.md` gives a complete native and plugin
  tracker recipe with exact live paths and no speculative tracker model.
- Existing extending and plugin pages link to it and accurately describe caps,
  typed unsupported behavior, provider-call boundaries, doctor scope, and
  CLI-vendor placement.
- No help action, config key, API wire, completion slot, or ratchet exception
  was introduced; docs remain consistent with the architecture standards.
- Follow-ups are clearly left to Notion, Plane, Kaneo tier cleanup, a concrete
  CLI adapter, plugin doctor inventory, and THE-20 SDD skills.
- Commit exactly as: `docs(the-50): document tracker provider extension`
