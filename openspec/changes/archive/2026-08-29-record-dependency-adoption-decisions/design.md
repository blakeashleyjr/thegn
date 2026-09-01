# Design — dependency adoption records for THE-61

The deliverable is the six-record set under `docs/adr/`, indexed by
`docs/adr/index.md`, with detailed evidence in
`.thegn/pipeline/THE-61/architect/design.md`.

## Verified decisions

- `rustix`: defer. Existing `nix`/`libc` seams cover the current Unix
  pane/PTY/signal/fd work; a migration would add churn without removing the
  libc-only macOS and FFI sites.
- `windows-rs`: adopt / keep the existing split. `windows-sys` owns raw Win32
  host APIs and `windows` owns WinRT media APIs. Version alignment is a
  separate, target-tested follow-up.
- `whoami`: reject. Hostname and session account values already use tested,
  degrading platform/environment paths.
- `sysinfo`: adopt / keep. It is the feature-trimmed, off-loop periodic
  sampler; targeted process deduplication and direct macOS libproc remain
  deliberate boundaries.
- `zerocopy`: defer. Current unsafe sites are foreign ABI calls and there is
  no fixed-layout Rust-owned wire format.
- `tokio-tungstenite`/`tungstenite`: adopt / keep. The service uses the client
  for control and provider WebSockets, and the direct pin shares axum's
  resolved cohort.

## Architecture and audit constraints

The records must cite current manifests, lock entries, call sites, MSRV 1.89,
musl/macOS/mingw target implications, binary/build cost, unsafe surface,
maintenance, and whether a future change replaces or adds a dependency. They
must preserve platform ownership, the substrate-free core, background metric
sampling, and service-edge transport ownership.

The existing gate is `just deps-audit` (`justfile:455-462`), which runs
`cargo deny check` and `cargo machete`; it is included by `just ci`
(`justfile:394-397`) and the dedicated CI job (`.github/workflows/ci.yml:121-138`).
It is not invoked by `just lint`. `deny.toml:8-97` remains unchanged: advisory
exceptions need reasons and exit conditions, licenses and sources are gated,
and duplicate versions warn under the documented known-splits policy.

## Explicitly pruned draft work

The draft proposed changing `windows-sys` 0.59 and `windows` 0.58 to newer
versions and editing the duplicate-version comment. That is substantive
cross-target migration work, not a documentation-only or trivially safe
adoption, so it is removed from this change. No source, manifest, lockfile,
deny policy, config, catalog, help, or ratchet change is designed here.
