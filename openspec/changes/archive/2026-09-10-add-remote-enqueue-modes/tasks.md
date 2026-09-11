# Tasks — remote enqueue modes

## 1. Shipped mode and client baseline

- [x] 1.1 Add `RemoteMode` with default `route_to_host` and alternative `push`.
- [x] 1.2 Complete push-mode local fold/advance/push and test successful plus
      non-fast-forward outcomes without false success.
- [x] 1.3 Add `ControlClient::merge_add` and conditional route-to-host forwarding
      using host-canonical `$THEGN_WORKTREE`, with no local fallback on failure.
- [x] 1.4 Make missing local `$THEGN_WORKTREE` fall through to normal resolution
      so worktree-scoped commands run inside a sprite (`0fc2ac66`).

## 2. Delivered provisioning and security (THE-99)

- [x] 2.1 Mint a revocable `MergeAdd`-only token during remote/provider
      provisioning and inject it with the reachable host URL without exposing
      secret material.
- [x] 2.2 Define token ownership, lifetime, rotation/reprovision, and destroy
      revocation; document the THE-101 confidentiality dependency.
- [x] 2.3 Supply the same off-argv, worktree-bound credential lifecycle for a
      user-managed SSH worktree, including per-account/host/worktree ownership,
      verified reuse, atomic rotation, and delete-time revocation/removal.

## 3. Delivered host enqueue (THE-99)

- [x] 3.1 Resolve remote worktree membership, branch, and location from
      registered DB/provider metadata without host-local path/Git access.
- [x] 3.2 Fail explicitly for missing/unreachable endpoint, invalid scope,
      unknown worktree, stale location, and remote branch lookup errors; never
      create a sprite-local fallback row.

## 4. Remaining verification and docs

- [x] 4.1 Add a true distinct-filesystem end-to-end test: remote enqueue,
      host-only row, remote-tip ingest/drain, and final disposition.
- [x] 4.2 Document configuration, secure serving prerequisites, token boundary,
      failure recovery, and push-mode alternative.
- [x] 4.3 Reconcile the completed push and route-to-host behavior with THE-99.
- [x] 4.4 Validate the completed change strictly before archive after 2.3.
      `env RUSTC_WRAPPER= just ci` passed on 2026-09-10.
