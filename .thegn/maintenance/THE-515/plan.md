# THE-515 plan — bind trusted workspace overlays to canonical repository identity

Base: `main` @ `1ffcd9f7`. Binding guidance: `coordination.md` (this dir).
Status: investigation complete; **slice 1 implemented on this branch** (see
§4 slice 1); slices 2–4 are blocked on THE-516
(`RepositoryId`) and THE-505 (`AdmittedConfig`/`ConfigRevision`).

## 1. Findings (re-verified on current main)

The trusted `[workspace.<key>]` layer (`WorkspaceConfig`, config.rs ~2348:
editor, keybinds, accounts, hooks, sandbox_mounts, env_bundle, merge_queue,
pr_queue, ci, autopilot, git, mcp_serve) is selected by **two different lossy
schemes**, not one:

- **Basename key** — `config::workspace_slug(root)` = `slugify(repo_name(root))`
  with an `"repo"` fallback for an empty slug. `repo_name` runs
  `git rev-parse --show-toplevel` (a subprocess), so every selector call is
  also blocking git I/O — including on the UI loop (see below).
- **DB tab slug** — `db.slug_for_repo(path, base)` (`db_workspace.rs:120`), the
  sidebar/tab namespace: the _first registered_ repo claims the bare name and
  later same-basename repos get `-2`, `-3`. `agent::repo_slug` (agent.rs:3861)
  even passes the raw, unslugified basename as `base` when no row exists.

So for two clones named `foo`, merge queue / hooks / mounts / git / CI /
autopilot select `[workspace.foo]` for **both** (the reported collision), while
accounts / env bundle / editor / keybinds select `[workspace.foo]` for the
first-registered one and `[workspace.foo-2]` for the second — a _hybrid_
policy, and one granted by "first DB row", which coordination.md forbids.

Additional defects found beyond the issue text:

1. `review_handoff.rs:92` forges a `Path` from the DB tab slug
   (`Path::new(slug)`) and calls `repo_pr_queue` on it. `repo_name` then runs
   `git rev-parse --show-toplevel` **relative to the process cwd** (a
   directory literally named like the slug under the cwd changes the answer)
   and does so on the compositor loop, contrary to its own doc comment.
2. `repo_git(worktree_path)` in `actions.rs:1155`, `run.rs:23007` and
   `cmd/diff.rs:52` keys by the _linked worktree directory's_ basename
   (`--show-toplevel` of a linked worktree is the worktree itself), not the
   repository — so a worktree dir named like another repo selects that repo's
   `[workspace.*.git]`, and the configured repo's own block never applies to
   its linked worktrees. The two UI-loop sites also spawn git on the loop.
3. `ide_handoff.rs:222` claims `workspace_slug` is "pure path-derived"; it
   spawns git on the UI loop.
4. A `[workspace.<key>]` whose key is not a `slugify` fixed point
   (`Foo`, `my_repo`, `My.Repo`) is unreachable for basename consumers but
   reachable for DB-slug consumers when the raw basename became a DB slug —
   i.e. the same table governs different authorities depending on path.
   Two table keys normalizing to one slug (`Foo` + `foo`) are silently
   ambiguous. Nothing validates either.
5. The `"repo"` fallback makes `[workspace.repo]` govern every repository
   whose basename slugifies to empty (`.git`, `___`, non-ASCII-only names)
   _and_ a repository literally named `repo`.

## 2. Consumer inventory (current main, file:line)

Selector definitions (all basename key): `config.rs:996` `workspace_slug`;
`config.rs:6408` `effective_keybinds` (slug param); `config.rs:6560`
`repo_merge_queue`; `:6576` `repo_git`; `:6590` `repo_pr_queue`; `:6604`
`repo_ci`; `:6616` `repo_autopilot`.

Direct table reads:

| Authority                | Site                                                                       | Key today                                                |
| ------------------------ | -------------------------------------------------------------------------- | -------------------------------------------------------- |
| sandbox mounts (ungated) | core `config_resolve.rs:1181`                                              | basename                                                 |
| trusted hooks            | host `worktree_lifecycle.rs:289`                                           | basename                                                 |
| hook trust review        | host `cmd/repos.rs:124`                                                    | basename                                                 |
| config explain           | host `cmd/config.rs:263`                                                   | basename                                                 |
| coding-agent accounts    | core `account.rs:265` ← `bundle.rs:437`, host `agent_configs.rs:674`       | DB tab slug                                              |
| env bundle / HOME        | core `bundle.rs:101` ← host `agent.rs:3531,3544`, `palette.rs:902`         | DB tab slug                                              |
| editor                   | core `editor.rs:604` ← host `ide_handoff.rs:90` (slug from `:224`, `:298`) | basename (via git on loop) / sidebar DB slug for intents |
| keybinds                 | core `config.rs:6408` ← host `keymap.rs:510,1723`                          | caller slug                                              |
| MCP scope ceiling        | host `cmd/mcp.rs:241`, `cmd/doctor.rs:1245` pass `workspace = None`        | **unwired** — must route through the resolver when wired |
| doctor aggregate         | host `cmd/doctor.rs:1639` (`values()`, count only)                         | n/a, non-authority                                       |

`repo_*` selector call sites:

- merge queue: `integrate.rs:557`, `merge_ops.rs:141,261,296,323`,
  `merge_sweep.rs:81,121`, `remote_enqueue_auth.rs:369,580`,
  `hydrate.rs:2122`, `handlers/merge_queue.rs:131,740,951,982,1030,1057`,
  `cmd/integrate.rs:44`, `cmd/land.rs:42,55`,
  `cmd/merge.rs:108,422,461,530,572,780,796`, `cmd/config.rs:272`,
  `cmd/doctor.rs:2933,2983`.
- git: `integrate.rs:559`, `cmd/integrate.rs:66`, `git_worktree.rs:68,121`,
  `hydrate.rs:1538`, `run.rs:9986`; **worktree-path keyed (defect 2)**:
  `actions.rs:1155`, `run.rs:23007`, `cmd/diff.rs:52`.
- PR queue: `ci_autofix.rs:136`, `review_task_handoff.rs:68`,
  `autopilot_driver.rs:488`, `handlers/pr_queue.rs:147`, `cmd/pr_queue.rs:65,211`,
  `cmd/config.rs:282`; **slug-as-path (defect 1)**: `review_handoff.rs:92`.
- CI autofix: `ci_autofix.rs:86`.
- autopilot: `autopilot_driver.rs:31,703`, `cmd/autopilot.rs:39`.

Write surfaces: `config_write.rs` has no `[workspace.*]` writer today;
`cmd/config.rs` `set`/`edit` write arbitrary dotted keys (so
`workspace.<key>.…` is writable with no ambiguity check).

Tests that codify basename slugging: `config_tests.rs` (workspace overlay
block), `cmd/config.rs:596`, `cmd/merge.rs:903-920`, `review_handoff.rs:275`,
`keymap.rs:2699`, `account.rs:437,456,637`, `bundle.rs:903`, `editor.rs:713`.

## 3. Target design (from coordination.md; supersedes issue wording)

- **Identity** = THE-516 `RepositoryId`: full SHA-256 of the domain-separated,
  length-delimited canonical Git common-dir bytes (`identity.rs`,
  `repo::repository_id`). Linked worktrees share it; it is _not_ origin- or
  basename-derived. Deviation from issue item 1 ("path identity plus verified
  Git origin"): origin is **not** part of the ID. Origin is mutable and two
  clones of one origin are different repositories, so origin is a separately
  verified _authority revision_ bound to the admitted snapshot; an origin
  change invalidates overlay/launch authority rather than changing the ID.
- **Binding**: a trusted block is authority-bearing only when it names its
  repository explicitly: `[workspace.<label>] repository = "<RepositoryId hex>"`
  (and optionally `origin = "<url>"` as the pinned authority revision). The
  table key becomes a _label/alias_: uniqueness-checked, never authority by
  itself. Moving the common dir changes the ID → the block stops matching and
  `thegn config explain` shows the unmapped block; remapping is explicit.
- **Legacy (unbound) blocks**: selected only while _provably unambiguous_ —
  exactly one known repository (THE-516 identity ledger enumeration, not "first
  DB row", not cwd) maps to the legacy key, and no alias collision exists.
  Otherwise refused, visibly, with the block preserved. Migration command:
  extend `thegn repos` / `thegn config explain` to list every legacy block,
  every known repository mapping to it, and to write the `repository = …`
  binding for a user-selected repository (`thegn repos bind-workspace <key>
[path]` or `config set workspace.<key>.repository` with a verifier).
- **Resolution** happens once per effect, off the UI loop, into an immutable
  `ResolvedWorkspaceOverlay { repository: RepositoryId, origin_revision,
config: ConfigRevision, key, overlay }`; queue/launch/agent records store
  `(RepositoryId, origin revision, ConfigRevision)` and re-verify before the
  first side effect and at publication (THE-505 / THE-268 contract).
- Core stays substrate-free: the resolver in core is pure over
  `(config table, captured identity, known-repository index)`; the Git/FS
  capture is the host's (THE-516 seam). Unsupported platform identity refuses
  before effects (THE-516 `IdentityError::UnsupportedEncoding`).

## 4. Slices and dependencies

### Slice 1 — single chokepoint + config-level refusal (THIS BRANCH, no deps)

- `thegn_core::workspace_overlay`: the one pure resolver. Typed result
  `WorkspaceOverlay::{Unconfigured, Selected{key, overlay}, Refused(OverlayRefusal)}`.
  Refusals implemented now: `AliasCollision` (two table keys normalize to the
  requested key), `NonCanonicalKey` (key is not a slug fixed point — never
  selectable, reported), `EmptyKey`. The empty-basename `"repo"` fallback no
  longer selects authority (`Unconfigured`, and `[workspace.repo]` is only a
  repository literally named `repo`).
- Every consumer in §2 routes through `Config::workspace_overlay(root)` /
  `Config::workspace_overlay_for_key(key)`; no direct `cfg.workspace.get` in
  production code (test-only direct inserts remain).
- Fail-closed semantics on `Refused`: no overlay fields apply, and the
  automatic supervisors that would otherwise run under the _global_ policy are
  disabled for that repo (`merge_queue.enabled/auto_land`, `pr_queue.enabled`,
  `autopilot.enabled`, `ci.autofix.mode = off`); manual `thegn land` /
  `thegn integrate` bail with the refusal (a stricter overlay gate must never
  silently degrade to a weaker global one). Git falls back to the global table
  (documented; it is the user's own policy, not another repo's).
- Global validation: `typed_semantic_errors` reports every non-canonical /
  colliding / empty key. `thegn config validate` fails on them now; once
  THE-505 lands, admission (`config_admission.rs` runs `typed_semantic_errors`)
  rejects the config before any repo-influenced effect.
- Visible surfaces: `thegn config explain` and `thegn repos trust` print the
  refusal instead of silently skipping.
- Defect fixes: review_handoff keys by the session's repo root (no forged
  path, no cwd dependence); the worktree-keyed `repo_git` callers key by the
  repository root (session id on the loop, `main_worktree` in the CLI); the
  legacy key derivation for known repo roots is pure (`repo_name_from_path`),
  removing git-on-loop from the selector path. `workspace_slug` remains as the
  compat derivation for callers that may pass a subdirectory.

Not done in slice 1 (deliberately): the DB-tab-slug consumers (accounts,
bundle, keybinds, ide intents) still present the tab slug as the key. Switching
them to the basename key would _widen_ the collision for the `-2` repo's
credentials; switching the queues to the tab slug would grant authority by
first DB row. Both converge on `RepositoryId` in slice 2. They do now go
through the chokepoint, so config-level refusals apply uniformly.

### Slice 2 — RepositoryId binding (blocked on THE-516 chunks 1–2 landing)

Consumes `thegn_core::identity::RepositoryId` + `repo::repository_id` (both
committed on `blake/the-516-…` at `6e647a18` but under revision-4 review).
Add `repository` (+ optional `origin`) to `WorkspaceConfig` (three config-key
ratchets: schema, example, env overlay), extend the resolver input with
`Option<&RepositoryId>` and a known-repository index, switch accounts/bundle/
keybinds/editor/review_handoff to the same resolution as the queues, and add
the legacy-block migration surface. The known-repository index must come from
THE-516's identity ledger (chunk 2) captured off-loop — until then no
per-lookup ambiguity detection across same-basename repos is possible without
the forbidden first-row inference.

### Slice 3 — admitted-config binding (blocked on THE-505)

`ResolvedWorkspaceOverlay` carries `ConfigRevision`; queue rows
(merge/PR queue, autopilot, agent-run) persist `(RepositoryId, origin
revision, ConfigRevision)` and re-verify before the first side effect and at
publication. Needs THE-505's `AdmittedConfig` to be the live config handle.

### Slice 4 — acceptance matrix (needs 2+3)

Same-basename different parents/origins, punctuation/case aliases,
Unicode/non-UTF-8 paths, symlink/relative aliases, mirrors, worktrees,
common-dir moves, origin replacement, case-insensitive FS; queue/agent record
retention; migration preserving every legacy block.

## 5. Acceptance criteria mapping

| AC                                                                                           | Status after slice 1                                                                                                                  |
| -------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| Tests for same basename / aliases / Unicode / symlink / mirrors / worktrees / moves / origin | **Partial**: alias (punctuation/case/empty) tests only; the rest need RepositoryId                                                    |
| No colliding repo receives another's hooks/mounts/creds/…                                    | **Open** for same-basename repos (needs slice 2); closed for config-alias collisions and worktree-dir aliasing of `[workspace.*.git]` |
| Load/explain/edit fails visibly on ambiguous legacy aliases                                  | **Partial**: validate + explain + repos trust + land/integrate refuse; load-time rejection arrives with THE-505 admission             |
| Identity stable / generation-invalidated                                                     | **Open** (slices 2–3)                                                                                                                 |
| Queue/agent records retain repo/config identity                                              | **Open** (slice 3)                                                                                                                    |
| Migration preserves every legacy block, explicit mapping                                     | **Partial**: no block is deleted or rewritten; refusal preserves source; mapping command is slice 2                                   |
| Aliases possible but uniqueness-checked, not authority by themselves                         | **Partial**: uniqueness/normalization checked; "not authority by themselves" needs the binding (slice 2)                              |
