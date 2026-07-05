# Add zones: per-profile workspace groups with credential/egress/budget sub-scoping

## Summary

A **zone** is a named group of workspaces _inside one profile_ providing a soft,
concurrent firewall — the motivating case is two freelance clients open side by
side in the same window, each worktree's panes and agents scoped to only that
client's credentials, egress, and budget. This is distinct from process profiles
(a hard, separate-process firewall) and from subprofiles (a serial, single-
subsystem identity flip): zones are concurrent and workspace-scoped.

Zones split **policy** (config `[zone.<name>]`: bound bundles, egress/budget
ceilings, sandbox floor) from **membership** (DB-tracked `workspaces.zone_id`,
assigned by an explicit action — never inferred from a filesystem path). A zone
_clamps_ its members: egress intersects down, blocks union, sandbox hardening
only rises, budget rolls up to a `zone:<name>` proxy scope. A zone-owned bundle
is a credential sub-vault that only its members may compose.

## Impact

- **workspace** — adds a nullable `workspaces.zone_id` (exclusive membership) and
  a `[zone.<name>]` policy table; a `superzej zone` CLI manages both.
- **env-bundles** — bundles gain an owning `zone`; the compose fold denies a
  zone-owned bundle to a foreign/unzoned worktree (covering direct, workspace,
  global, and `extends`-reachable bindings) and surfaces the denial.
- **llm-proxy** — the per-request budget rollup gains a `zone:<name>` scope
  between the worktree scope and global; the proxy resolves the mapping per
  request from the shared per-profile DB (no push/sync).
- **state-db** — adds a `zones` table + `workspaces.zone_id` (schema v33).
- **AB / sandbox** — zone egress/floor ceilings apply at the launch resolve seam,
  consuming the config-trust-resolution clamp engine's `Zone` trust slot.

Extends the `workspace`, `env-bundles`, `llm-proxy`, and `state-db` capabilities.

## Rationale

Profiles are a hard, whole-process firewall — too heavy for "two clients in one
sitting". Subprofiles re-scope a single subsystem serially and workspace never
opts in. Neither models _concurrent_ per-workspace credential/egress/budget
isolation. Zones fill that gap on top of the existing env-bundle + proxy-budget +
DNS-filter machinery: the enforcement points already exist; a zone is the policy
that parameterizes them for a group of workspaces. Membership must be
daemon-tracked because a security tool cannot let a spoofable filesystem path
decide credential scope.

## Non-goals

- **Replacing process profiles** — a zone is a _soft_ firewall; secrets that must
  be kernel-isolated belong in a separate profile.
- **Cutting global bundles from zoned workspaces** — global (unzoned) bundles
  stay usable inside a zone; a stricter `isolate` mode is a future follow-up.
- **The general clamp engine** — zone ceilings plug into the `Zone` trust slot
  reserved by `add-config-trust-resolution`; an interim `apply_zone_ceilings`
  applies them until that engine absorbs the slot.
