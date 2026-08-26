# Tasks

> **Implementation status (phase 1 landed on `worktree-agent-*`).** The
> validated substrate — data model, store (incl. ordering), pure batched-create
> planner, `thegn project` CLI, `thegn wt new --project`, capability catalog +
> scopes, help page, smoke test — is DONE. Deferred to a phase-2 follow-up (each
> a large, separately-validated piece): the **sidebar** project-header rendering
> (§4 — ~13 `build_rows` call sites + hydration/model_eq/render plumbing on the
> load-bearing render path, unsafe to ship without the full test+e2e suite),
> **feature-set derivation** (§2.1), **project-scoped aggregation** (§5), and
> **batched merge enqueue** (§3.3). `set_project_order` already ships as the
> sidebar reorder's persistence substrate.

## 1. Data model (thegn-core)

- [x] 1.1 `projects` table (unique name, `position`) + nullable
      `workspaces.project_id` — additive migration (`user_version` 54) —
      **migration tests** (pre-migration rows survive; idempotent re-open).
- [x] 1.2 `ProjectStore` (`db_projects.rs`): `create`/`rename`/`list` (member
      counts)/`delete(force)` (refuses non-empty unless forced; force
      unassigns members)/`assign`/`project_of_workspace`/`project_members`/
      `set_project_order` — **unit tests** (zones' `db_zones.rs` template).

## 2. Feature-set model (thegn-core, pure)

- [ ] 2.1 `project::feature_sets(members, worktrees)` — group worktrees by
      exact branch-name equality across member repos; deterministic order;
      sparse sets — **table tests** (95% gate). **DEFERRED (phase 2).**
- [x] 2.2 Batched-create plan (`project::plan_batched_create`): resolve ONE
      branch name (prefix + slug, no per-repo prefix re-application), classify
      members create/exists-attach/subset(`--repos`) — pure planner — **unit
      tests**.

## 3. CLI + capability catalog (thegn-host)

- [x] 3.1 `thegn project list|create|rename|rm [--force]|assign` mirroring
      `thegn zone` grammar; `--json` via the one emitter.
- [x] 3.2 `thegn wt new --project [--repos]` executes the plan per member via
      the existing `thegn_core::worktree` pipeline; per-member outcome
      report; re-run attaches (`exists`) — no rollback of siblings; non-zero
      exit on any member failure.
- [ ] 3.3 `thegn merge add --project <p> --feature <branch>`: enqueue the
      feature set's branches as independent per-repo queue rows; per-member
      report. No drain changes. **DEFERRED (phase 2 — depends on §2.1).**
- [x] 3.4 CATALOG rows (`project.list|create|rename|rm|assign|new-feature`)
      gated by `required_scope` (list read-scoped, rest write-scoped);
      OPERATOR surface (CLI implemented, HTTP/gRPC excused in SURFACE_GAPS;
      MCP/plugin await the in-flight write-tool scope-gating branch).
- [x] 3.5 `test/smoke.sh`: batched create across the `alpha`+`beta` fixture
      repos (gpgsign off via isolated HOME, isolated `XDG_STATE_HOME`),
      re-run attaches, `--repos` subset, delete-refused/emptied paths.

## 4. Sidebar (thegn-host) — DEFERRED (phase 2)

Depends on `stabilize-sidebar-internals` + `add-sidebar-folder-ordering`
(both code-complete on main). Large render/hydration change — deferred to a
pass with the full test + e2e suite (see status note above).

- [ ] 4.1 Project header rows grouping member workspaces (name, member
      count, tier-granular attention rollup); unprojected after the groups.
- [ ] 4.2 Collapse (tombstone-free `ui_state`, prefix-pruned on project
      delete) + project reorder (keyboard + drag, `set_project_order`) —
      **row-partition unit tests**. (`set_project_order` already shipped.)
- [ ] 4.3 Glyphs via `caps::active_glyphs()` only; damage channel `Full`.
- [ ] 4.4 e2e: re-record affected muse baselines if a frame changes.

## 5. Project-scoped aggregation — DEFERRED (phase 2)

- [ ] 5.1 `cross-worktree-aggregation` model accepts a project scope; labels
      repo-qualified; deterministic order — **pure unit tests**.
- [ ] 5.2 Populate off the loop; channel + `TerminalWaker`; no new polling.

## 6. Help + docs

- [x] 6.1 `docs/help/projects.md` (registered in `pages.rs`) documents the
      `thegn project` CLI + `wt new --project`, disambiguates from tracker
      `project_key`/`project_id`, and notes the deferred sidebar + land_order.
      (No TUI action ids yet — sidebar interactions ship with §4.)
- [x] 6.2 CLI reference/completions regenerate from clap automatically (the
      new `Project` command + `wt new` flags appear with no hand-edit); no new
      config key ships.

## 7. Validation

- [x] 7.1 `openspec validate add-multi-repo-projects --strict` clean.
- [ ] 7.2 Run `just ci` once, when the implementation is complete (pre-PR
      gate — not per-edit). Deferred to the land gate.
