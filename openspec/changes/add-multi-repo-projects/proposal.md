# Sub-repos under a single project (multi-repo workspace groups)

Linear: THE-33

## Why

Microservice work spans repos: one feature is a branch in `api`, a branch in
`web`, and a branch in `shared-lib`, reviewed and landed together-ish. thegn's
core model — workspace = repo, tab = worktree — is exactly right _per repo_,
but today three-service work means three unrelated workspaces, three
hand-created branches the user must name identically by discipline, and no
surface that shows "this feature, across its repos" at once.

The research seed (orca `stablyai/orca#1099`) asks for VS Code-style
multi-root workspaces: one workspace holding several repository roots with
unified source-control/diff views. thegn should NOT copy that shape — every
per-repo subsystem here (diff/CI/commit caches, merge queue, PR queue, repo
trust, sandbox prep, env compose) keys on the one-repo workspace, and
dissolving that invariant would touch all of them for no correctness gain.
thegn already has the right precedent one shelf up: **zones** group workspaces
above the workspace layer (`zones` table + nullable exclusive
`workspaces.zone_id`, explicit assignment, never path-inferred). A **project**
is the same grouping shape with _workflow_ semantics instead of zone's
_policy_ semantics: batched worktree creation with linked branch naming,
grouped navigation, and cross-repo status aggregation — while git remains the
sole source of truth in each member repo and no super-repo state is invented
(the cross-repo link IS branch-name equality).

## What Changes

- **Data model**: a `projects` table (unique name) plus a nullable, exclusive
  `workspaces.project_id` — the zones shape verbatim, one additive migration
  (next `user_version` bump on the ladder). Membership is assigned by explicit
  action, never inferred from a filesystem path. Projects are orthogonal to
  zones: assigning a project never changes credential/egress/budget scope.
- **CLI**: a `thegn project` namespace (`list`, `create`, `rename`,
  `rm [--force]`, `assign`) mirroring the `thegn zone` grammar, honoring the
  `--json` convention, with every verb projected as
  `thegn_core::capability::CATALOG` rows gated by `required_scope(verb)`.
- **Batched feature creation (linked naming)**: `thegn wt new <name>
--project <p> [--repos a,b]` resolves ONE branch name (configured prefix +
  slug) and creates that exact branch + worktree in every member repo (or the
  subset), reporting per-member outcomes; a re-run attaches to members that
  already have the branch, giving retry semantics after partial failure.
- **Feature sets**: a pure `thegn-core` model that derives, from a project's
  member repos and their worktree/branch lists, the groups of same-named
  branches ("feature sets"). Derived, never persisted — git stays the source
  of truth per repo; the DB only caches what it already caches.
- **Sidebar**: member workspaces render grouped under a collapsible project
  header row; unprojected workspaces render exactly as today. Header rows are
  orderable and their collapse state persists tombstone-free.
- **Cross-repo aggregation**: the Across aggregation model gains a project
  scope — excerpts from a feature set's worktrees across member repos, with
  repo-qualified labels, populated off the event loop.
- **Merge queue (batched enqueue only)**: `thegn merge add --project <p>
--feature <branch>` enqueues the feature's branch in each member repo's
  per-repo queue as independent rows. Queues stay strictly per-repo; ordered
  or atomic cross-repo draining is explicitly out of scope (see Non-goals).
- **Help**: a `docs/help/projects.md` page claims the new action ids (help
  ratchet); sidebar header interactions covered under `zone:sidebar`.

## Impact

- Roadmap: **Z 340** (multi-repo PR dashboard) gets its grouping substrate —
  a project is the "which repos belong together" fact that dashboard needs;
  **D 41** (create worktree) gains the batched cross-repo form. Adds a new
  roadmap item for multi-repo projects (audit phase wires it into tasks.md).
- Specs: deltas to `workspace` (project membership + feature semantics),
  `sidebar` (project grouping rows), `state-db` (projects table),
  `cli` (`project` namespace + `wt new --project`),
  `cross-worktree-aggregation` (project scope), `merge-queue` (batched
  project enqueue).
- **DB schema change**: additive `projects` table + `workspaces.project_id`
  column; one `user_version` bump on the migration ladder.
- Capability catalog: new `project.*` verbs (and the `wt new --project` /
  `merge add --project` parameterizations) are CATALOG rows projected across
  CLI/control/MCP surfaces, gated by `required_scope` — never a second policy
  table.
- In-flight changes reconciled:
  - `stabilize-sidebar-internals` + `add-sidebar-folder-ordering` — both
    rework sidebar row building/ordering; project header rows build on their
    post-change row model (runs + tombstone-free ui_state) and must land
    after or rebase onto them.
  - `add-workspace-zones` (landed core) — projects reuse its
    membership shape but carry zero policy; a workspace can be in one zone
    AND one project.
  - `add-cli-namespaces-and-remote-open` — the `project` namespace follows
    its noun-verb grammar, `--json` emitter, and grouped help.
  - `add-issue-driven-worktrees` — its issue→worktree start action can later
    target a project (start a feature set from an issue); not scoped here.
  - `add-pr-queue` — untouched; its per-repo rows are what a future
    project-grouped PR view would group.
- No AI-layer dependency anywhere in this change.

## Non-goals

- **Multi-root workspaces.** A workspace remains exactly one repo; a project
  is a grouping above workspaces, not a container of repository roots.
- **Atomic cross-repo landing.** Two git repos (or two forges) share no
  transaction; thegn will never pretend a cross-repo land is atomic.
  Ordered draining (`land_order`, stop-on-failure) is a designed follow-up,
  not part of this change.
- **Cross-repo PR dashboard (Z 340 proper).** This change provides the
  grouping substrate only.
- **Super-repo state.** No manifest file, no meta-repo, no submodule
  management; the cross-repo link is branch-name equality, derived from git.
- **Policy semantics.** Projects never clamp credentials, egress, budget, or
  sandbox — that is zones' job, and the two remain orthogonal.
