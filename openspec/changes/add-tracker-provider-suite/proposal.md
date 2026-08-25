# Tracker provider suite — parity, Notion + Plane, plugin trackers, spec-DD links

Linear: THE-50

## Why

THE-50 asks for a full audit of the project-management/issue provider surface:
Jira, Linear, GitHub Issues, Notion, Plane, Kaneo, "any with easy extension via
plugin", plus a spec-driven-development integration question (BMAD-METHOD,
OpenSpec, spec-kit). The audit against today's code
(`crates/thegn-svc/src/issue/`, `crates/thegn-core/src/config_issues.rs`,
`crates/thegn-host/src/handlers/tracker.rs`) and the in-flight
`add-generic-tracker-model` change found:

- **The seam exists and is healthy** — `IssueBackend` (object-safe,
  `BoxFuture`), `IssueRouter` with per-account fan-out and
  `[[issues.issue_accounts]]` (multiple accounts per provider), probe +
  conformance coverage (`thegn_svc::conformance` asserts one issues report per
  account and factory/reserved agreement), and a plugin bridge
  (`PluginIssueBackend` over `provider.call`) already registered per the
  `plugin-runtime` spec.
- **Parity is uneven and undeclared.** Linear/GitHub/Jira implement only the
  five core ops (list/get/create/update/search); Kaneo alone implements
  `add_comment`/`attach_label`/`detach_label` — and those are exercised only by
  the vendor-specific `thegn kaneo` CLI, never by a generic surface. There is
  no caps struct today: "capability discovery" is calling the op and reading an
  `IssueError::Api("… not supported …")` string. `IssueError` has no
  `Unsupported` class and does not implement `thegn_core::seam::SeamError`, so
  the seam's own degradation vocabulary cannot classify it.
- **One vendor leaks through the seam**: `IssueBackend::as_kaneo()` downcasts
  to the concrete Kaneo backend for board/project browsing — precisely the
  per-vendor special case the caps model exists to eliminate.
- **`add-generic-tracker-model` (in flight) owns the substrate** —
  `TrackerBackend` + `TrackerCaps`, `WorkItem`, project/cycle tiers,
  transitions, multi-account instance routing, HouseTracker MCP tools — with
  Linear as the reference provider, and explicitly defers "completing Jira",
  further providers, and everything else to follow-up changes. **This change is
  that follow-up**: it scopes only the delta on top of that model.
- **Plugin trackers can't declare capabilities.** A bridged plugin provider
  has no way to advertise what it supports, so under the caps-gated model the
  chrome would either probe-by-error or assume all-false for every plugin.
- **Notion and Plane fit the model well.** Plane's workflow-state groups
  (backlog / unstarted / started / completed / cancelled) map 1:1 onto the
  canonical `IssueStatus`; Notion's status-property groups (To-do / In
  progress / Complete) canonicalize cleanly, with the option name preserved as
  `status_raw`.
- **Spec-DD linking is a real product feature, not thegn-repo tooling.** Repos
  using OpenSpec or spec-kit carry machine-readable change folders that
  reference tracker issues (OpenSpec proposals carry `Linear: THE-n`-style
  lines; spec-kit's `/speckit.taskstoissues` creates GitHub issues from spec
  tasks and names spec dirs after branches). Surfacing "this issue has a spec
  change in this repo" on the issue row — and seeding a dispatched agent with
  the spec's location — closes the loop for any repo using these formats.

## What Changes

1. **Tracker conformance suite** (`work-tracking`): a shared, offline
   caps⇔ops-agreement check every `TrackerBackend` passes — ops behind a false
   cap return `Unsupported` without I/O; ops behind a true cap are actually
   implemented (they fail with anything _but_ `Unsupported` when
   unconfigured). `IssueError` gains an `Unsupported` variant classification
   via `impl SeamError` so ladders and the conformance registry speak the seam
   vocabulary. `TrackerCaps` gains a `labels` capability (attach/detach_label
   already exist as optional ops but the in-flight caps struct cannot declare
   them).
2. **Provider parity to each backend's honest ceiling**: Jira gains
   transitions, comments, labels, projects, and subtask mapping (cycles stay
   `false` — Jira sprints need Agile-API board discovery, deferred); GitHub
   Issues gains comments and labels via `gh` (projects/cycles/transitions stay
   `false`); Kaneo migrates onto the generic project/board tier and **the
   `as_kaneo()` downcast is deleted**; `thegn tracker login` becomes
   provider-generic (Kaneo's device flow moves behind it; `thegn kaneo login`
   stays as an alias).
3. **Two new providers**: Notion (data-source query, property mapping,
   comments; honest mostly-false caps) and Plane (workspaces/projects/cycles/
   states/labels/comments; rich caps), each an `IssueProviderKind` value with
   an `[issues.<provider>]` sub-table, `IssueAccount` fields, overlay arms, a
   doctor probe, and conformance coverage.
4. **Plugin tracker capabilities** (`plugin-runtime`, MODIFIED): the
   `IssueProvider` contribution declares its `TrackerCaps`; the bridge refuses
   false-cap ops locally (mirroring native default impls) and extends the
   `provider.call` op vocabulary with the tracker-tier ops; caps gate the
   chrome for plugin providers exactly as for native ones.
5. **Spec-DD linking** (`spec-linking`, new capability): a spec-format seam
   (OpenSpec and spec-kit implemented, BMAD reserved) detects spec-change
   folders in any workspace repo, a pure core function links issues to changes
   (declared refs + branch-name association), the work panel badges linked
   issues and shows the change (with task progress) in the detail view, and
   issue dispatch seeds the agent with the spec's location. AI-additive:
   linking and display work with no agent configured.

## Impact

- **Roadmap**: AA 345 (comment on issues from the TUI — becomes generic, not
  Kaneo-only), AA 348 (generic tracker adapter — Jira etc. completed to
  ceiling), AT 648 (groundwork for cross-provider triage; the GitHub/GitLab/
  Gitea/Forgejo _forge-tracker_ expansion itself stays in AT), AA 341–344
  (substrate consumed, unchanged). Spec-linking feeds the Q 211/212 pipeline
  entry (issue → worktree → agent) with spec context.
- **Specs**: `work-tracking` (ADDED — extends the capability introduced by the
  in-flight `add-generic-tracker-model`; **depends on that change landing
  first**), `plugin-runtime` (MODIFIED: "A plugin can be an issue provider"),
  `spec-linking` (new capability, ADDED).
- **In-flight reconciliation**: depends on `add-generic-tracker-model`
  (TrackerBackend/TrackerCaps/WorkItem/transitions/instances — this change is
  the "completing Jira / more providers" follow-up its Non-goals name);
  composes with `add-issue-driven-worktrees` and `add-issue-autopilot` (their
  dispatch/`TaskKind::IssueImplement` surfaces gain the spec-seed variables —
  whichever lands, the vars are additive); the in-flight MCP write-tools
  scope-gating branch owns MCP write policy — HouseTracker writes ride its
  gate, nothing re-scoped here. Does not touch `add-fleet-view`.
- **Config**: `[issues.notion]` (`api_key`, `data_source_id` with
  `database_id` accepted alias, `user_id`, property-name mapping keys),
  `[issues.plane]` (`api_key`, `base_url` default `https://api.plane.so`,
  `workspace_slug`, `project_id`), matching `IssueAccount` fields +
  `IssuesOverlay` arms, and a `[specs]` table (`enabled`, `formats`). Every
  key documented in `config/config.toml.example`; secrets only as
  `env:`/`file:`/`keyring:` refs.
- **DB**: **no schema change.** Provider caches ride
  `add-generic-tracker-model`'s tables (`issue_cache`, `tracker_meta`,
  `issue_detail_cache`); spec links are derived from a repo scan + the issue
  cache, never persisted.
- **Capability catalog**: no new external door. `tracker login` (catalog row
  claimed by `add-generic-tracker-model`) widens its provider argument; panel
  actions (labels, spec-open) are in-process chrome, claimed by `docs/help/`
  pages per the help + prose ratchets.
- **Coverage**: pure logic (status/property mapping, spec-ref parsing,
  link resolution, caps tables) lands in `thegn-core` under the 95% gate;
  network/subprocess provider impls live in `thegn-svc` behind the seam and
  are exercised by the offline conformance suite + smoke.

## Non-goals

- **GitHub Projects v2, GitLab, Gitea, Forgejo tracker providers** — AT 648's
  own follow-up change, as `add-generic-tracker-model` already records.
- **Jira Agile sprints as cycles** — board discovery via the Agile REST API is
  deferred; Jira honestly advertises `cycles: false`.
- **Kanban board view** — deferred to `add-tracker-board-view` (per the
  in-flight change).
- **OAuth flows and webhook/push updates** — polling + pasted/stored keys only,
  behind the same seam.
- **Writing or scaffolding spec changes from thegn** — spec-linking reads
  spec folders; it never creates or edits them.
- **A BMAD adapter implementation** — `bmad` is a reserved format kind until
  its artifact conventions are pinned against a real repo.
- **Any AI dependency** — parity, new providers, plugin caps, and spec links
  all function with zero AI layers; agent seeding is strictly additive.
