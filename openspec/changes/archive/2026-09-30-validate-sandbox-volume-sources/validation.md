# Reviewed verification — 2026-09-15

The final source `72d161b2e1868561fd2f5bce9facde8b0345512a` passed strict
workspace all-target Clippy and the shared 145-test Linux matrix (77 core,
16 svc, 52 host). See [the durable audit](../../../docs/audits/maintenance07-2026-09-15.md)
and its hashed manifest for exact source, selectors, raw logs, historical
failures, independent reviews, and platform/scope limitations. No additional
release or per-issue compilation was used.

## Historical source checkpoint

# Validation checkpoint — 2026-09-15

Private source candidate: `/tmp/thegn-THE247-private-luna-20260915-b`.

The candidate makes `thegn_core::sandbox::enter_argv` fallible, removes the
legacy `util::shell -lc` refusal shim, and updates every repository callsite
found by the source search. `prepare_sandbox_env` validates effective
configured sources before resolution, then orders Ready overlay, remote
finalization, final admission, VPN, and ensure. Terminal pane construction
performs the same pre-resolution check. New pure/core tests check bounded
redaction, pair index, direct entry, and OCI option refusal; a callback
sentinel proves the final gate blocks effects. Host tests exercise configured
source refusal before agent fallback, typed `compose_spec` propagation,
terminal no-spec refusal, and explicit `none` preservation. The former
source-text ordering guard was removed.

This is a source-only candidate checkpoint. No Cargo build, native runtime,
provider, or benchmark result is claimed here. Root must review the private
commit and run the applicable gates before any landing or delivery completion.
