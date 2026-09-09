# Tasks — remote enqueue modes

## 1. Shipped mode and client baseline

- [x] 1.1 Add `RemoteMode` with default `route_to_host` and alternative `push`.
- [x] 1.2 Complete push-mode local fold/advance/push and test successful plus
      non-fast-forward outcomes without false success.
- [x] 1.3 Add `ControlClient::merge_add` and conditional route-to-host forwarding
      using host-canonical `$THEGN_WORKTREE`, with no local fallback on failure.
- [x] 1.4 Make missing local `$THEGN_WORKTREE` fall through to normal resolution
      so worktree-scoped commands run inside a sprite (`0fc2ac66`).

## 2. Remaining provisioning and security (THE-99)

- [ ] 2.1 Mint a revocable `MergeAdd`-only token during remote/provider
      provisioning and inject it with the reachable host URL without exposing
      secret material.
- [ ] 2.2 Define token ownership, lifetime, rotation/reprovision, and destroy
      revocation; document the THE-101 confidentiality dependency.

## 3. Remaining host enqueue (THE-99)

- [ ] 3.1 Resolve remote worktree membership, branch, and location from
      registered DB/provider metadata without host-local path/Git access.
- [ ] 3.2 Fail explicitly for missing/unreachable endpoint, invalid scope,
      unknown worktree, stale location, and remote branch lookup errors; never
      create a sprite-local fallback row.

## 4. Remaining verification and docs

- [ ] 4.1 Add a true distinct-filesystem end-to-end test: remote enqueue,
      host-only row, remote-tip ingest/drain, and final disposition.
- [ ] 4.2 Document configuration, secure serving prerequisites, token boundary,
      failure recovery, and push-mode alternative.
- [x] 4.3 Reconcile the change with THE-99 and distinguish shipped push/client
      baseline from incomplete route-to-host behavior.
- [ ] 4.4 Validate the completed change strictly before archive.
