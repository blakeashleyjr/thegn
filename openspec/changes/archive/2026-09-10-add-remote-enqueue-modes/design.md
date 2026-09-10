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

Provisioning/launch mints one token limited to `MergeAdd` for the remote
worktree and installs it with a reachable host URL. Provider environments use
the provider's secret environment after the reusable checkpoint. User-managed
SSH targets stream the two raw data lines over stdin to a fixed remote script;
neither secret is in argv, ordinary logs, audit summaries, transcripts, or
generated artifacts. Runtime files are owner-only and keyed by an opaque stable
worktree digest, so several worktrees may safely share one SSH account/home.

Provider ownership includes provider, non-secret account-ref identity, and
sandbox id. SSH ownership includes account/host, a digest of the connection
identity, and worktree. Reattach first verifies the installed token's hash,
exact `MergeAdd` scope, owner/worktree binding, expiry, and current origin; a
match is reused so live panes are not invalidated, while duplicates are
revoked. A mismatch rotates through an atomic file replacement under an
owner-scoped host lock. Destroy/recycle and SSH worktree deletion revoke before
owner metadata disappears. The pane reads the file with `read -r` and never
evaluates credential text as shell input. The endpoint must satisfy THE-101's
remote confidentiality contract: only a declared TLS-terminated or
trusted-tunnel topology is eligible.

## Host-side resolution

The request carries the host-canonical worktree identifier, not a branch name
chosen by the caller. The host resolves membership, target repository, branch,
and location from registered DB/provider metadata. Any target operation needed
to resolve the current branch uses the location adapter; it never treats the
remote absolute path as host-local. Exact registry lookup happens before local
path confinement/canonicalization; only aliases of rows explicitly marked
local reach the host filesystem. The row is committed only after these facts
agree and contains the remote location needed by cross-host drain.

## Failure and verification

Missing endpoint/secret, connection failure, invalid scope, unknown membership,
stale location, or remote branch lookup all return explicit errors and create
no local fallback row. End-to-end coverage uses genuinely distinct filesystem
paths, proves host-only ownership, then drains by remote-tip ingest through the
existing cross-host queue path.
