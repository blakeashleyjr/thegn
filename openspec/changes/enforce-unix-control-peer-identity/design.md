# Design — authenticated Unix peers

## Accept-time identity

The Unix listener retrieves the connected peer's effective UID using the
platform-native credential API (`SO_PEERCRED`, `getpeereid`, or the supported
equivalent) immediately after accept. The result is immutable request context:
`Verified { euid }`, `Mismatch { euid }`, or `Unavailable { reason }`. HTTP,
WebSocket, and gRPC adapters consume this context; they do not re-infer locality
from a socket address or listener flag.

## Authorization rule

`local_admin` means “same effective UID may authenticate implicitly,” not “all
connections on this listener are admin.” The router grants implicit Admin only
for a verified UID equal to the daemon effective UID. Mismatch/unavailable
peers follow ordinary scoped-token authorization; absent/invalid tokens are
rejected. No platform fallback may convert unknown identity into Admin.

## Filesystem precondition

When `local_admin` is enabled, ownership and restrictive modes on the run
directory and socket are defense in depth but mandatory startup invariants.
Failure is surfaced and implicit Admin is not advertised. A token-required
configuration allows deployment on a platform/filesystem that cannot provide
or maintain reliable peer identity/modes.

## Portability and tests

Platform adapters isolate credential retrieval. Supported Unix targets test
same-UID success and inject mismatched/unavailable facts to test fail-closed
authorization. Integration coverage includes local_admin off and hardening
failure. Comments/docs say “same UID” only where the runtime actually verifies
it.
