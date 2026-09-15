# THE-247 named-volume admission — source checkpoint

Date: 2026-09-15

Candidate: `/tmp/thegn-THE247-private-luna-20260915-b` (private review clone;
root owns integration and landing).

The source candidate changes the core sandbox entry boundary to a fallible
`enter_argv` and removes the legacy `util::shell -lc` refusal shim. Named-volume
sources are admitted by a typed `VolumeAdmissionError` containing only the
pair index, byte length, and fixed reason. The policy requires at least two
ASCII bytes, an ASCII alphanumeric first byte, and ASCII alphanumeric,
underscore, hyphen, or dot thereafter. It rejects path, option, control, and
Unicode source forms without echoing the source.

The direct entry API and both OCI option builders admit before argv creation.
Host propagation covers agent composition and dispatch, terminal panes and
the run loop, wizard creation, materialize, and compiler-cache probing. An
invalid cache probe disables that cache decision after restoring its temporary
mount state; it does not execute a host fallback. `compose_spec` and terminal
construction return `anyhow::Result`, preserving the typed error in the source
chain with static bounded context. No caller launches a refusal shell.

`prepare_sandbox_env` admits effective configured sources before backend
resolution (while preserving explicit `none`/disabled host policy), then
applies the Ready overlay and remote finalization and admits the final spec
before isolation effects, VPN attachment, or `sandbox::ensure`. A callback
sentinel test exercises the final production gate; host tests cover configured
source refusal before agent host fallback, terminal no-spec refusal, typed
`compose_spec` propagation, and explicit `none` preservation. Pure/core tests
cover lexical rejection, valid names, bounded redaction, pair indexing, direct
entry, and OCI refusal.

This checkpoint records source inspection only. No Cargo build, test execution,
native runtime, provider, benchmark, or external tracker mutation was run for
this candidate. Root must review the private commit and execute the applicable
gates before landing; delivery remains verification-gated.
