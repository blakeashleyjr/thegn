# Complete remote merge enqueue modes

Linear: THE-99

## Problem addressed

`[merge_queue] remote_mode` and both high-level modes exist, but the default
`route_to_host` path is not end-to-end for a true off-host worktree. The client
can forward only when endpoint/token variables happen to exist, provisioning
does not mint/inject them, and the host handler still shells out against the
remote path as if it were local.

## Shipped baseline

- `RemoteMode` selects `route_to_host` (default) or `push`.
- Push mode queues/drains the sprite's local clone, advances its target, pushes
  to `origin`, and surfaces a rejected push without false upstream success.
- `ControlClient::merge_add` and `POST /v1/merge/add` exist.
- When `THEGN_CONTROL_URL` and `THEGN_CONTROL_TOKEN` are present, remote add
  forwards the host-canonical `$THEGN_WORKTREE`; failure does not silently
  create a local row.
- Cross-host rows carry location, and target-host drain can ingest an off-host
  branch tip.

## Delivered route-to-host implementation

- Provision a reachable host endpoint and a revocable token scoped only to
  `MergeAdd`, with defined lifetime/ownership/rotation/destruction behavior and
  no secret exposure through argv, logs, audit rows, or generated artifacts.
- Make host merge-add resolve repository membership, branch, and location from
  registered DB/provider metadata without local filesystem/Git access to the
  remote path.
- Cover missing/unreachable endpoint, invalid scope, unknown/stale worktree or
  location, and remote branch lookup failure without local fallback.
- Exercise true distinct host/remote filesystem paths through enqueue, host-only
  ownership, remote-tip drain, and final disposition; document prerequisites,
  recovery, security boundary, and push-mode alternative.
- Enforce THE-101's shipped confidentiality topology: route credentials are
  issued only for a live TLS-terminated or trusted-tunnel endpoint.
- Give user-managed SSH worktrees the same least-privilege lifecycle: one
  opaque per-worktree owner-only file streamed over SSH stdin, account/host/
  worktree ownership, verified reuse across reattach, atomic rotation, and
  delete-time revocation/removal.

## Non-goals

- Transparently dispatching all drain work to an arbitrary target host.
- Replacing push mode or granting a remote environment scopes broader than
  `MergeAdd`.
