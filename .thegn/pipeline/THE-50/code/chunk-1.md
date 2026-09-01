# Chunk 1 — typed tracker caps/errors and offline conformance

## Files touched

- `crates/thegn-svc/src/issue/capabilities.rs` (new)
- `crates/thegn-svc/src/issue/mod.rs`
- `crates/thegn-svc/src/issue/linear.rs`
- `crates/thegn-svc/src/issue/github.rs`
- `crates/thegn-svc/src/issue/jira.rs`
- `crates/thegn-svc/src/issue/kaneo.rs`
- `crates/thegn-svc/src/conformance.rs`
- `crates/thegn-svc/src/seam/registry.rs`
- `crates/thegn-svc/src/plugin/provider.rs`
- `crates/thegn-host/src/plugin_providers.rs`
- `crates/thegn-host/src/handlers/plugins.rs`

No other files are in scope. In particular, do not add config keys, a new
control/catalog row, a DB migration, a tracker tier, Notion/Plane, spec-linking,
or a generic command-execution config.

## Approach

1. Create a small `issue::capabilities` module with serde/schema-visible
   `IssueCaps { comments, labels }`, defaulting both to false. Add `caps()` to
   the object-safe `IssueBackend` trait. Keep the required five operations as
   they are; only current optional methods participate in this cap catalog.
2. Add `IssueError::Unsupported(&'static str)` plus the
   `thegn_core::seam::SeamError` implementation. Preserve the existing
   connect/timeout transient distinction, classify missing subprocess binary
   errors as `NotInstalled` only when unambiguous, and leave ordinary API/parse
   failures final/`Other`. Make `is_transient()` use the shared classification.
   Default comment/label methods must construct the typed error immediately
   and perform no client, network, or subprocess work.
3. Add provider `caps()` declarations: Linear, GitHub, and Jira have no
   current optional capabilities; Kaneo declares comments and labels true.
   Do not expose Kaneo's board/project/move methods through this partial cap
   model and do not retain or expand the `as_kaneo()` API in this chunk.
4. Extend `thegn_svc::conformance` with one operation table and a test that
   walks `IssueProviderKind::ALL`. The test must be hermetic: it exercises
   false-cap defaults locally and uses a deliberately overclaiming and
   underclaiming test backend to prove both failure directions. Provider-local
   tests assert the declared positive bits correspond to real methods; no
   conformance test calls a real HTTP endpoint or vendor binary. Preserve the
   existing probe-shape, reserved, determinism, and factory tests.
5. Parse the existing `Contribution.caps` JSON for issue providers into
   `IssueCaps` with unknown fields rejected and omitted/null values all false.
   Carry caps in the host plugin-provider registry row and construct
   `PluginIssueBackend` with them. False-cap optional calls return typed
   Unsupported locally; true-cap calls use the existing `provider.call`; an
   RPC `unsupported` response maps to typed Unsupported. The five existing
   core calls and timeout behavior remain backward-compatible.
6. Add caps to configured native issue `ProbeReport`s. Do not make
   `thegn doctor` start resident plugins or add network probing; standalone
   plugin discovery is a follow-up.

Respect the architecture gates: all plugin/network work remains off-loop,
`thegn-core` stays substrate-free, vendor CLI calls remain in
`issue/github.rs`, and there is no new god-file module. Update only tests that
are directly required by the new typed seam contract.

## Overlap and dependency

This chunk is independent of Chunk 2 and touches no Chunk 2 file. Within this
chunk the files are intentionally one serial change because trait, provider
implementations, conformance, plugin construction, and probe serialization
must agree atomically. No coder dependency on another chunk.

## Tests to run

Scoped only:

- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc conformance`
- `cargo nextest run -p thegn-svc issue`
- `cargo nextest run -p thegn-svc plugin`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host plugin_providers`

Do not run `just test`, `just ci`, a full-workspace compile, e2e, or any
`thegn` invocation against the live state DB. If an invocation of `thegn` is
needed for a focused check, set `XDG_STATE_HOME` to a newly created temporary
directory first.

## Done criteria

- `IssueBackend` is object-safe and has a typed, serde/schema-visible optional
  capability catalog; every optional default returns typed Unsupported with no
  I/O.
- `IssueError` implements the shared seam classification and all relevant
  tests cover Unsupported, NotConfigured, Auth, Transient, Other, and missing
  binary behavior.
- Every current `IssueProviderKind::ALL` entry is covered by the offline
  conformance ledger; overclaims and underclaims fail the test; native caps and
  the Kaneo positive operations agree.
- Plugin providers honor declared/omitted caps locally, preserve the existing
  five-op wire behavior, map RPC unsupported correctly, and remain router
  participants without any `thegn-core` provider implementation.
- Doctor's configured issue rows serialize caps, still probe offline, never
  print credentials, and retain deterministic output.
- No config/help/catalog/completion/env-overlay/control-schema ratchet needs a
  change because no user config or external surface was added; existing
  ratchets remain green.
- Commit exactly as: `feat(the-50): close tracker seam capability gaps`
