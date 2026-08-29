# ADR-0005: Defer zerocopy until a fixed-layout format exists

- Status: Deferred
- Date: 2026-08-29
- Scope: FFI layouts, control wire formats, and byte parsing

## Context

There is no direct `zerocopy` dependency. The transitive lock entry at
`Cargo.lock:10349-10354` is not an adoption. Current unsafe code is foreign
ABI work: macOS libproc uses `MaybeUninit` and a raw slice for kernel-filled C
out-parameters (`crates/thegn-core/src/activity.rs:730-765`), thermal code
casts `dlsym` function pointers (`crates/thegn-metrics/src/thermal.rs:110-166`),
and rlimit/Win32 calls use their platform APIs
(`crates/thegn-host/src/fd_limit.rs:21-40`,
`crates/thegn-host/src/platform/windows.rs:217-241`).

The control plane uses JSON text for attach input and SSE, encoded event frames
for daemon output, and `prost` for gRPC (`crates/thegn-svc/src/control/http.rs:1449-1558`).
There is no Rust-owned fixed-layout wire struct to replace.

## Decision

Defer. `zerocopy` would not make these FFI contracts safe, remove an existing
unsafe block, or improve a current wire format. Adding it would increase
derive/proc-macro maintenance and direct-dependency review without a binary
or runtime benefit. Thegn-core remains independent of substrate codecs.

## Reopen condition

If a measured hot fixed-layout wire or storage format is introduced, evaluate a
small codec module at the owning service edge. Specify endian/alignment,
fuzz malformed input, and keep the domain model independent. This would be a
new dependency, not a cleanup of the current FFI sites.
