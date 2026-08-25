# Tasks — tracker provider suite

> Phases 1–5 layer on `add-generic-tracker-model` (`TrackerBackend`/
> `TrackerCaps`, `WorkItem`, transitions, instance routing, `tracker_meta`) —
> where that change has not landed, the affected phase blocks on it. Phase 6
> (spec-linking) depends only on the existing issue cache and can proceed in
> parallel.

## 1. Seam vocabulary (thegn-svc + thegn-core)

- [ ] 1.1 Add `IssueError::Unsupported(&'static str)` and
      `impl thegn_core::seam::SeamError for IssueError` in
      `crates/thegn-svc/src/issue/mod.rs`: `Unsupported`→`Unsupported`,
      `NotConfigured`→`NotConfigured`, `Auth`→`Auth`,
      `Network`→`Transient|Other` (connect/timeout/5xx vs the rest),
      `Subprocess`→`NotInstalled|Other` (binary-missing by content),
      `Api`/`Parse`→`Other`; collapse `is_transient()` into the trait's
      default — **unit tests** for the full classification table.
- [ ] 1.2 Rewrite the optional-op default impls (`add_comment`,
      `attach_label`, `detach_label`, and the tracker-tier ops) to return
      `IssueError::unsupported(op)` — no more stringly `Api("… not
supported …")` — and assert the defaults return **without any I/O**
      (no client construction, no network).
- [ ] 1.3 Add `labels: bool` to `TrackerCaps`; declare it true on Kaneo
      (already implements attach/detach) and false elsewhere until phase 3 —
      **unit test** that the caps struct and the label ops agree per provider.

## 2. Offline conformance: caps ⇔ ops agreement (thegn-svc)

- [ ] 2.1 A shared `(cap, op)` table covering every optional op
      (`add_comment`, `attach_label`, `detach_label`, `list_projects`,
      `project_items`, `list_cycles`, `available_transitions`, `transition`)
      in `crates/thegn-svc/src/conformance.rs`, plus a harness that
      constructs each backend **unconfigured** (construction is pure) and
      asserts: false cap ⇒ the op returns `ErrorClass::Unsupported`
      immediately; true cap ⇒ the op returns anything **but** `Unsupported`
      (`NotConfigured`/`Auth`/`Transient` prove an implementation exists) —
      one test iterating `IssueProviderKind::ALL` so new providers are
      covered mechanically.
- [ ] 2.2 Run the same harness against `PluginIssueBackend` with a scripted
      plugin fixture (the existing shell-script bridge test), so bridged
      providers obey the identical contract.

## 3. Provider parity to honest ceilings (thegn-svc + thegn-host)

- [ ] 3.1 **Jira** (`crates/thegn-svc/src/issue/jira.rs`):
      `available_transitions` = GET `/rest/api/3/issue/{key}/transitions`,
      `transition` = POST same; `add_comment` = POST `…/comment` with a
      minimal ADF paragraph (reuse the ADF walker); `attach_label`/
      `detach_label` = PUT `…/issue/{key}` `update.labels`; `list_projects` =
      GET `/rest/api/3/project/search`; map `issuetype.subtask` +
      `fields.parent` → `kind`/`parent_id`. Caps: transitions/comments/
      labels/projects/subtasks true; cycles stays false (Agile-API board
      discovery deferred) — parse/serialize **unit tests** on canned
      payloads; conformance suite green.
- [ ] 3.2 **GitHub Issues** (`crates/thegn-svc/src/issue/github.rs`):
      `add_comment` = `gh issue comment <n> --body`; labels = `gh issue edit
<n> --add-label/--remove-label` (dir-anchored, `gh` stays inside this
      impl file per the forge-CLI ratchet). Caps: comments/labels true;
      projects/cycles/subtasks/transitions stay false — argv-construction
      unit tests; conformance suite green.
- [ ] 3.3 **Kaneo** (`crates/thegn-svc/src/issue/kaneo.rs`): move board/
      project browsing onto the generic tier ops (`list_projects`,
      `project_items`; `boards: true`), **delete `IssueBackend::as_kaneo()`
      from the trait and its router accessor**, and re-route the `thegn
kaneo project/board/task` verbs through the router's generic ops —
      behaviour-preserving CLI output asserted by the existing smoke
      coverage.
- [ ] 3.4 Make `thegn tracker login` provider-generic: Kaneo's device flow
      moves behind `thegn tracker login kaneo`; `thegn kaneo login` stays as
      an alias projecting the **same** capability-catalog row (no new door).
      Update the CLI help text; smoke-test both spellings parse.

## 4. New providers: Notion and Plane (thegn-core config + thegn-svc)

- [ ] 4.1 Config (`crates/thegn-core/src/config_issues.rs`):
      `IssueProviderKind::{Notion, Plane}` (implemented, not reserved);
      `[issues.notion]` (`api_key` secrets-ref, `data_source_id` with
      `database_id` accepted alias, `user_id`, `status_property`,
      `assignee_property`, `labels_property`, `priority_property`);
      `[issues.plane]` (`api_key` secrets-ref, `base_url` default
      `https://api.plane.so`, `workspace_slug`, `project_id`); matching
      `IssueAccount` fields and `IssuesOverlay` arms — **unit tests** for
      layering, alias resolution, and `env:`/`file:` expansion (95% gate).
- [ ] 4.2 Pure mapping logic in `thegn-core` (95% gate): Notion
      status-group → `IssueStatus` canonicalization (group when present,
      name heuristic for plain selects; `status_raw` always preserved) and
      property-name mapping; Plane state-group (`backlog`/`unstarted`/
      `started`/`completed`/`cancelled`) → `IssueStatus` mapping — **table
      unit tests** for both.
- [ ] 4.3 **Notion backend** (`crates/thegn-svc/src/issue/notion.rs`):
      pinned `Notion-Version: 2025-09-03`; list = data-source query with
      filters compiled from `IssueFilter`; get = page retrieve + comments;
      create/update = pages API over mapped properties; search = `/v1/search`
      scoped to the data source; comments = `/v1/comments`; labels =
      multi-select option add/remove; `database_id` → data-source resolution
      cached in `tracker_meta`; `filter_assignee_me` requires `user_id`
      (unset ⇒ skip the filter with a one-time warning). Caps:
      comments/labels/create true, tiers/transitions false — payload parse
      unit tests on canned JSON; conformance green.
- [ ] 4.4 **Plane backend** (`crates/thegn-svc/src/issue/plane.rs`):
      `X-API-Key`; work-items endpoints with the `/work-items/` vs
      `/issues/` path probed once per account; projects/cycles/labels/
      comments/sub-issues first-class; empty `project_id` ⇒ workspace
      project fan-in bounded by `max_issues`; 60 req/min honored with
      batching + 429 backoff. Caps: projects/cycles/subtasks/comments/
      labels/create true, transitions/boards false — parse + backoff unit
      tests; conformance green.
- [ ] 4.5 `backend_from_account` arms, `thegn doctor` probe rows (missing
      token/base ⇒ `Unavailable` with reason, never token material), and
      confirmation that the per-account conformance/factory tests pick both
      up via `IssueProviderKind::ALL`.

## 5. Plugin tracker capabilities (thegn-svc plugin bridge)

- [ ] 5.1 Add an optional `caps` object (same field names as `TrackerCaps`;
      omitted ⇒ all false) to the `IssueProvider` contribution manifest;
      `PluginIssueBackend::caps()` returns the declaration — manifest parse
      unit tests (unknown fields rejected, omitted defaults false).
- [ ] 5.2 Refuse false-cap ops **locally** with `Unsupported` (no
      round-trip); extend the `provider.call` op vocabulary with the
      tracker-tier ops (`available_transitions`, `transition`,
      `list_projects`, `project_items`, `list_cycles`) — additive strings on
      the same `{"seam":"issues","op":…}` wire; keep the existing
      `unsupported`-reply fall-through as the second net — bridge unit tests
      for local refusal + wire round-trip.
- [ ] 5.3 Add the "tracker provider as a plugin" row/recipe to
      `docs/extending/` (declaring caps, the op vocabulary, the conformance
      contract).

## 6. Spec-DD linking (thegn-core + thegn-host)

- [ ] 6.1 `crates/thegn-core/src/spec_link.rs` (95% gate): `config_enum!
SpecFormatKind { OpenSpec, SpecKit, Bmad (reserved) }`; per-format
      detection markers (openspec: `openspec/changes/<id>/proposal.md`
      skipping `archive/`; spec-kit: `.specify/` + `specs/<NNN>-<slug>/`);
      `SpecChangeMeta` parsing from bounded reads (4 KiB heads, checkbox
      counts from tasks.md); ref extraction (`PROVIDER-KEY` tokens, `#<n>`,
      full issue URLs) — **unit tests** for detection, parsing, bounds, and
      ref extraction.
- [ ] 6.2 Pure `resolve_links(changes, issues, worktree_branches) →
Vec<SpecLink>`: declared refs match only against the **cached issue
      set**; spec-kit branch-name association links through `issue_links`
      transitively — **table unit tests** incl. the no-match (stray token)
      and transitive cases (95% gate).
- [ ] 6.3 `[specs]` config table (`enabled = true`, `formats = []` = detect
      all implemented; naming a reserved kind fails `config validate
--strict`) — unit tests for the strict-reject path.
- [ ] 6.4 Host scan: run detection + parsing off-thread inside model
      hydration (`Utility` QoS), cached by directory mtime; deliver over the
      existing channel + `TerminalWaker` pulse; damage is chrome-only
      (`Full`), idle wake stays `Skip` — extend the `render_plan::plan` unit
      tests for the new model field.
- [ ] 6.5 Panel: spec badge on linked Issues/Mine rows; a "Spec" block
      (change id, title, `tasks_done/tasks_total`) in the detail view; new
      `issues.open_spec` action opening the proposal via the viewer/editor
      seam — handlers in `crates/thegn-host/src/handlers/tracker.rs` (pinned
      files may only shrink).
- [ ] 6.6 Dispatch seeding (AI-additive): when the dispatch target issue has
      a resolved link, add `THEGN_ISSUE_SPEC_DIR` (absolute path) and
      `THEGN_ISSUE_SPEC_FORMAT` to the launch env beside the existing
      `THEGN_ISSUE_*` vars; if `add-issue-autopilot`'s `TaskKind::
IssueImplement` has landed, add the optional `spec_dir` prompt var
      (empty when unlinked) — unit test the env/var assembly; everything
      works with zero agents configured.

## 7. Docs, help, and validation

- [ ] 7.1 Document every new key in `config/config.toml.example`:
      `[issues.notion]`, `[issues.plane]` (secrets as `env:`/`file:`/
      `keyring:` refs only), `[specs]`, and the plugin `caps` manifest field
      in the plugin docs.
- [ ] 7.2 Update `docs/help/`: the work-panel page claims `issues.open_spec`
      and mentions the spec badge and label actions by name (`panel:issues`
      context; help + prose ratchets stay green — `test/help-ratchet.txt`
      may only shrink).
- [ ] 7.3 Run `just ci` once at the end (includes `openspec-validate`,
      coverage on the new core logic, and the conformance suite).
