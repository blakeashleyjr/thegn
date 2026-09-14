# THE-545 interactive headless review admission

The interactive review handoff's headless fallback launched a PR-review agent
without the shared own-PR admission used by the queue, CI and durable review
paths. A prior confirmation to send review text did not establish current
authorship, repository identity, selected head or execution location. Sandbox
resolution also ran on the compositor thread before dispatch.

Primary review approved this bounded direct-path repair. A proposed blanket hold
on all own-only automation was rejected: it would remove existing behavior
without implementing account-bound dispatch. No such global hold is included.

## Implemented boundary

The UI captures the selected review snapshot, displayed URL, active worktree,
resolved repository queue policy and command, then queues a blocking worker. It
reports verification queued rather than claiming the agent has already received
feedback. The existing live-pane path still pastes without submitting.

For own-only policy, the worker opens one workspace DB and checks persisted local
execution before resolving/querying a forge. Missing location is the existing
supported local case; lookup errors, malformed metadata and remote/provider
placements hold. The selected snapshot's key uses `GitLoc::worktree_cache_key`.
The fresh PR number, head and branch must match the selected snapshot. Shared
authorship admission binds the proof to the checkout's origin and current HEAD;
the selected view URL must match that proof's structured repository and exact PR
number. Foreign authorities, userinfo, extra paths, fragments and queries are
not normalized into acceptance.

Only admitted requests reach sandbox/prompt preparation. After preparation the
existing shared gate revalidates author, viewer, origin, PR/head and persisted
execution location immediately before the agent launch seam. Prompt metadata
uses the admitted fresh PR. Holds do not consume queue attempts or change durable
dispatch rows. Explicit `own_prs_only = false` retains its broader behavior and
has no new DB/provider prerequisite.

## Regression and review gates

Seven new tests invoke the production blocking admission helper and its actual
launch callback boundary with synthetic local Git/DB fixtures and a recording
forge. They cover an owned organization PR reaching one launch callback; foreign
and unknown author, mixed PR number, stale head/branch/worktree; hostile selected
URLs; remote/malformed placement and a genuine failed SQL location lookup; late
viewer/origin/local-head/placement changes; preservation of an existing Running
dispatch row and zero attempt consumption; preparation failure; and explicit
broader policy without DB or proof. These tests do not execute a coding agent,
contact a provider, resolve real credentials or demonstrate actual credential
isolation. Preparation and launch counters assert the ordering in production
control flow, including proof before sandbox preparation.

At the initial review checkpoint, rustfmt and diff checks pass. Production host
compilation, focused regression execution and independent source review are
pending coordinated gates. The intended test selection is `review_handoff::`
(existing selection/overlay cases plus the seven new cases), `pr_authorship::`,
the existing PR queue/CI denial cases and durable review cases. Root owns the
final combined graph, lint and local-main landing.

## Remaining THE-545 acceptance

THE-545 remains In Progress for account-generation binding at real dispatch.
Repeated proof is not a credential freeze. The current login-shell runner may
read mutable CLI configuration, Git helpers and SSH/home credentials separately
from the proof request. The forge ladder can also select a different credential
source for a subsequent mutation. This patch makes no stronger claim.

Positive binding needs an execution/credential capability retained from proof
through each permitted action: canonical provider/host/repository, authenticated
viewer, immutable credential or broker authority, admitted configuration
generation and operation scope. A generation number, CLI config fingerprint or
`GH_TOKEN` override alone cannot enforce that against the current shell and
reachable credential channels. THE-541 defines provider credential/transport
binding (with THE-540/THE-538 prerequisites); THE-233 defines the worker boundary
and trusted mutation brokers. Their architecture plan is separate work.
