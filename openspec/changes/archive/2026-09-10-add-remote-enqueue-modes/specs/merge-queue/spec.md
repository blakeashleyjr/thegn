# Merge queue

## ADDED Requirements

### Requirement: Push mode lands locally and pushes origin

When `[merge_queue] remote_mode = "push"`, a remote/provider worktree SHALL
enqueue into its local queue, fold/gate/advance its local target clone, and push
the advanced target to `origin`. A rejected/non-fast-forward push MUST surface
the failure and MUST NOT report the branch landed upstream.

#### Scenario: Push converges through origin

- **WHEN** a conflict-free sprite queue drains in push mode
- **THEN** its local target advances and is pushed to origin

#### Scenario: Rejection is not success

- **WHEN** origin rejects the target update
- **THEN** drain exits with the push reason and origin remains unchanged

### Requirement: Route-to-host writes only the target host queue

When a worktree is off-host and `remote_mode = "route_to_host"`, merge add
SHALL call the target host's control plane using the host-canonical worktree ID
and SHALL create the row only in the host-owned queue with the registered
remote location. Missing/unreachable endpoint, invalid authorization, unknown
worktree, stale location, or remote branch failure MUST be explicit and MUST
NOT fall back to a sprite-local row. On-host worktrees remain local.

#### Scenario: A true remote enqueue is host-owned

- **WHEN** merge add runs from a provisioned off-host worktree
- **THEN** only the host DB receives a row containing its remote location

#### Scenario: Forwarding failure has no fallback

- **WHEN** the host endpoint cannot be reached
- **THEN** add fails with recovery guidance and neither DB gains a fallback row

### Requirement: Provisioning supplies least-privilege return credentials

Provider provisioning or user-managed SSH launch SHALL supply a revocable token
scoped only to `MergeAdd` plus the reachable host control URL. For SSH, the
secret SHALL stream off argv into an isolated owner-only per-worktree file;
ownership SHALL bind account, host, connection identity, and worktree. A valid
installed token for the same origin and binding SHALL be reused so opening a
second pane does not invalidate the first. Mismatch or expiry SHALL rotate it
atomically, and provider destroy/recycle or SSH worktree deletion SHALL revoke
it before owner metadata disappears. The secret MUST NOT appear in argv, logs,
DB audit text, transcripts, or generated artifacts, and the endpoint MUST
satisfy the remote control confidentiality policy.

#### Scenario: Destroy revokes the sprite token

- **WHEN** the remote worktree/provider environment is destroyed
- **THEN** its return token can no longer enqueue and no broader scope was ever
  granted

#### Scenario: Reopening a user-managed SSH worktree preserves live panes

- **WHEN** another pane opens for an SSH worktree whose installed credential is
  still valid for the same origin, account/host, and worktree
- **THEN** the credential is reused without exposing it in argv or revoking the
  token already exported by a live pane

### Requirement: Host enqueue resolves registered remote metadata

The host merge-add handler SHALL resolve target repository membership, current
branch, and remote location from registered DB/provider metadata for a
non-local worktree. It MUST NOT invoke local filesystem or Git operations on
the remote path. Branch lookup failures and stale metadata SHALL fail before a
queue row is committed.

#### Scenario: The remote path is absent on the host

- **WHEN** the registered worktree path exists only in the provider environment
- **THEN** host enqueue uses registered/remote metadata and never stats or runs
  Git against that host-local path

### Requirement: Route-to-host is proven end to end

Tests SHALL use distinct host and remote filesystem paths to enqueue through a
serving host, observe the row only in the host DB, ingest the remote tip during
drain, and verify final disposition. Documentation SHALL cover configuration,
serving/TLS prerequisites, token boundary/recovery, explicit failures, and the
push alternative.

#### Scenario: Full route-to-host lifecycle

- **WHEN** a true remote branch is enqueued and drained through the host
- **THEN** host ownership, remote tip ingestion, gate/advance, and final row
  disposition are verified without a bind-mounted fake-local path
