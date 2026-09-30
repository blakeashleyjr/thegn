# Design

## Admission policy

For every nonempty named-volume source, Thegn requires at least two bytes, an
ASCII alphanumeric first byte, and only ASCII letters, digits, `_`, `-`, or
`.` thereafter. This is an application admission policy for the existing
named-volume field. It rejects Unix and Windows path forms, colon and tilde
syntax, option separators, NUL/control bytes, and Unicode aliases before any
filesystem or runtime interpretation. The policy preserves ordinary existing
names such as `ok-volume` and `cache_01`; it does not claim that every backend
accepts every admitted name.

## Boundary and propagation

Effective configured sources are admitted before backend/provider resolution
when the sandbox is enabled and not explicitly `none`, so a resolver `None`
cannot turn malformed input into a host fallback. `apply_ready` and
`finalize_spec_before_ensure` then complete each resolved spec; the fully
composed spec is admitted again before isolation-floor handling, VPN
attachment, secrets, or `sandbox::ensure`. `enter_argv`, both OCI option
builders, `compose_spec`, terminal pane construction, agent dispatch, wizard
creation, and compiler-cache probing propagate the typed refusal. Invalid
input cannot become a host shell or a refusal shell process. Existing `None`
and disabled/host behavior remains unchanged.

The error carries pair index, byte length, and a fixed enum reason only.
Context added by callers is static; callers do not append raw worktree or
volume source values.

## Verification

Pure source tests cover empty, one-byte, path-like, Windows, option,
control-byte, and Unicode forms, valid names, pair indexing, redaction, and
direct/OCI entry refusal. Host tests cover configured-source refusal before
agent and terminal host fallback, the final admission callback sentinel,
`compose_spec` propagation, and explicit `none` preservation. Runtime and
Cargo gates are root-owned and are not claimed by this source review.
