# Design

## Policy in config, membership in the DB

A `[zone.<name>]` table holds declarative, git-diffable policy (bound bundles,
egress/budget ceilings, sandbox floor). Membership is a nullable
`workspaces.zone_id` — a single column, not a join table, so membership is
exclusive by construction (a workspace in two zones would dissolve the firewall).
Membership is set by an explicit action (`superzej zone assign`), never inferred
from a path; a filesystem path may only _suggest_ a zone in the UI. The DB is
already per-profile (profiles reroot `XDG_STATE_HOME`), so zone rows are
profile-scoped for free. Deleting a zone with members is refused unless forced.

## Ceilings

A zone clamps its members (never widens), plugging into the `Zone` trust slot
between profile and repo in the config-trust-resolution engine. Egress
`network_allow` intersects down (member entries not covered by the zone ceiling
are dropped and reported); `network_block` unions; the sandbox profile floor
raises hardening but never lowers it. An interim `apply_zone_ceilings` applies
these at the launch resolve seam until the general engine absorbs the slot.

## Bundle sub-vault

The compose chain gains a zone layer (global → zone → workspace → worktree). The
deny is enforced at **fold time**, so it covers every path a bundle can enter the
chain — direct, workspace, global, and `extends` reachability: a bundle whose
owning `zone` differs from the worktree's zone (including an unzoned worktree) is
skipped and recorded in `ResolvedEnv.denied`; the launch continues without it.
Global (unzoned) bundles remain composable everywhere.

## Budget rollup — no sync protocol

szproxy opens the _same_ per-profile `superzej.db` as the host, so the
worktree→zone mapping needs no push or periodic sync: `resolve_identity` derives
the worktree from a `worktree:<path>` scope and looks up its zone under the
already-held DB lock. `check_budget` and `record_spend` iterate `scope → zone →
global`, so a member request is refused by the zone cap even when under its own
cap, and reassignment takes effect on the next request. Config caps reach the
proxy as data (`sync_budget_caps` upserts `zone:<name>` budget rows without
touching spend).

## Concurrency

Two zones open side by side render correctly with no extra work: egress is
resolved per worktree (each gets its own DNS filter set), per-pane env composes
per worktree, and the budget lookup is per request. The only sharp edge is a
global bundle binding that names a zone-owned bundle — it is fold-denied per pane
(correct, if surprising); a config-issue warning is a follow-up.
