# Make sandboxed compiler caching explicit and fail-soft

Linear: THE-90

## Why

The dev shell can inject `RUSTC_WRAPPER=sccache` independently of modeled
`[disk].sccache` config. Existing cache-directory mounts do not guarantee the
daemon/socket works inside bwrap, so contained pipeline gates can fail for an
optional accelerator rather than compile uncached.

## What Changes

- Give sandbox compiler-cache use one explicit opt-in authority under
  `[sandbox]`; inherited dev-shell variables are inputs, never authority.
- Probe the wrapper and its transport in the effective containment, then fall
  back to an uncached compiler when the optional cache cannot work.
- Expose host availability, contained reachability/writeability, winning config
  provenance, and remediation separately in doctor.
- Add real bwrap compile smoke tests for a reachable cache and for transparent
  unreachable-cache fallback.

## Non-goals

- Requiring sccache for correctness.
- Exposing a host-wide arbitrary socket to a sandbox.
- Making the dev shell silently override an explicit off setting.
- Treating a host-side executable/socket check as proof that the contained
  process can use it.
