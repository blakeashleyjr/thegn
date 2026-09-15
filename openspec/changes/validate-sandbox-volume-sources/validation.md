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
