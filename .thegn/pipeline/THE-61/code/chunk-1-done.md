# Chunk 1 completion — dependency adoption ADRs

Implemented Chunk 1 for THE-61. The checked-out branch now has an index and
one decision record for each requested crate/family:

- `rustix` — defer broad adoption; retain the existing `nix`/`libc` seams.
- `windows-rs` — adopt/keep the `windows-sys` Win32 and `windows` WinRT split;
  version alignment is deferred to a separate cross-target update.
- `whoami` — reject; existing hostname and environment identity behavior is
  sufficient.
- `sysinfo` — adopt/keep as the selective off-loop sampler.
- `zerocopy` — defer until a measured fixed-layout format exists.
- `tokio-tungstenite`/`tungstenite` — adopt/keep aligned with axum at the
  service edge.

The records include current manifest, lockfile, and source citations; status,
target implications, build/binary cost, MSRV 1.89, unsafe/maintenance impact,
replacement-vs-addition treatment, and bounded reopen conditions. No source,
manifest, lockfile, OpenSpec, config, help, capability, ratchet, or runtime
files were changed.

## Validation

- `git diff --check HEAD^ HEAD` passed.
- Manually verified all six relative links in `docs/adr/index.md` and the
  cited repository paths/line locations against this checkout.
- `XDG_RUNTIME_DIR=/tmp RUSTC_WRAPPER= just quick thegn-core` passed.
- `XDG_RUNTIME_DIR=/tmp RUSTC_WRAPPER= cargo nextest run -p thegn-core dependency_adoption`
  compiled successfully but found no matching tests (exit 4; 0 run, 3623
  skipped).

## Unverified

- Full-workspace gates, cross-target builds, OpenSpec validation, and e2e were
  not run because this is a documentation-only chunk and the chunk policy
  excludes those checks.
- The first unmodified `just quick thegn-core` attempt could not create
  `/run/user/1000/just` in the sandbox; the scoped check passed with the
  writable runtime directory and compiler-wrapper override recorded above.
