# Tasks

## 1. Data model (thegn-core)

- [ ] 1.1 `projects` table (unique name, `position`) + nullable
      `workspaces.project_id` — additive migration, next `user_version` slot
      taken at land time (collision-prone; do not pin early) — **migration
      tests** (pre-migration rows survive; idempotent re-open).
- [ ] 1.2 `ProjectStore` (`db_projects.rs`): `create`/`rename`/`list` (member
      counts)/`delete(force)` (refuses non-empty unless forced; force
      unassigns members)/`assign`/`project_of_workspace`/`set_project_order`
      — **unit tests** (zones' `db_zones.rs` suite is the template).

## 2. Feature-set model (thegn-core, pure)

- [ ] 2.1 `project::feature_sets(members, worktrees)` — group worktrees by
      exact branch-name equality across member repos; deterministic order;
      sparse sets — **table tests** (95% gate).
- [ ] 2.2 Batched-create plan: resolve ONE branch name (prefix + slug, no
      per-repo prefix re-application), classify members
      create/exists-attach/subset(`--repos`) — pure planner — **unit tests**.

## 3. CLI + capability catalog (thegn-host)

- [ ] 3.1 `thegn project list|create|rename|rm [--force]|assign` mirroring
      `thegn zone` grammar; `--json` via the one emitter.
- [ ] 3.2 `thegn wt new --project [--repos]` executes the plan per member via
      the existing `thegn_core::worktree` pipeline; per-member outcome
      report; re-run attaches (`exists`) — no rollback of siblings.
- [ ] 3.3 `thegn merge add --project <p> --feature <branch>`: enqueue the
      feature set's branches as independent per-repo queue rows; per-member
      report. No drain changes.
- [ ] 3.4 CATALOG rows for every new verb/parameterization, gated by
      `required_scope` (reads read-scoped, writes write-scoped); note the
      in-flight MCP scope-gating branch as the MCP projection dependency.
- [ ] 3.5 `test/smoke.sh`: batched create across ≥2 fixture repos
      (gpgsign off, isolated `XDG_STATE_HOME`), partial-failure retry
      attaches.

## 4. Sidebar (thegn-host) — after/rebased on

`stabilize-sidebar-internals` + `add-sidebar-folder-ordering`

- [ ] 4.1 Project header rows grouping member workspaces (name, member
      count, tier-granular attention rollup); unprojected workspaces render
      unchanged after the groups; rail mode keeps identity.
- [ ] 4.2 Collapse (tombstone-free `ui_state`, prefix-pruned on project
      delete) + project reorder (keyboard + drag, `set_project_order`
      exact-order persistence) — **row-partition unit tests**.
- [ ] 4.3 Glyphs via `caps::active_glyphs()` only (literal ratchets stay
      shrink-only); damage channel `Full` for header/model changes — keep
      `render_plan` invariant tests green.
- [ ] 4.4 e2e: re-record affected muse baselines with `just e2e-update` if a
      frame changes (review the diff).

## 5. Project-scoped aggregation

- [ ] 5.1 `cross-worktree-aggregation` model accepts a project scope; labels
      repo-qualified; deterministic order — **pure unit tests**.
- [ ] 5.2 Populate off the loop from existing caches; channel +
      `TerminalWaker` delivery; no new polling (0%-idle preserved).

## 6. Help + docs

- [ ] 6.1 `docs/help/projects.md` claims every new action id (help ratchet +
      prose ratchet); sidebar header interactions under `zone:sidebar`;
      disambiguate from tracker `project_key`/`project_id`.
- [ ] 6.2 CLI reference/completions regenerate; no new config key ships
      (note the deferred `[project.<name>] land_order` in the help page's
      roadmap note).

## 7. Validation

- [ ] 7.1 `openspec validate add-multi-repo-projects --strict` clean.
- [ ] 7.2 Run `just ci` once, when the implementation is complete (pre-PR
      gate — not per-edit).
