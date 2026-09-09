# Design — remote merge enqueue ownership

## Two authority models

`push` treats the remote clone as the landing authority: its local DB orders
work, its local target advances after fold/gate, and origin is the convergence
point. `route_to_host` treats the target host as authority: the remote worktree
forwards only an enqueue request, the host DB owns ordering/state, and host
drain ingests the registered remote tip.

Mode selection never falls back across authority models. In particular, a
failed route-to-host request cannot create a local row, because doing so would
silently change queue ordering and landing authority.

## Return credential

Provisioning mints one token limited to `MergeAdd` for the remote worktree and
injects it with a reachable host URL through the provider's secret environment
path. The token is owned by that provisioned environment, is rotated on
reprovision, and revoked on destroy. It never appears in argv, ordinary logs,
audit summaries, or generated images/artifacts. The endpoint must satisfy
THE-101's remote confidentiality contract.

## Host-side resolution

The request carries the host-canonical worktree identifier, not a branch name
chosen by the caller. The host resolves membership, target repository, branch,
and location from registered DB/provider metadata. Any target operation needed
to resolve the current branch uses the location adapter; it never treats the
remote absolute path as host-local. The row is committed only after these facts
agree and contains the remote location needed by cross-host drain.

## Failure and verification

Missing endpoint/secret, connection failure, invalid scope, unknown membership,
stale location, or remote branch lookup all return explicit errors and create
no local fallback row. End-to-end coverage uses genuinely distinct filesystem
paths, proves host-only ownership, then drains by remote-tip ingest through the
existing cross-host queue path.
