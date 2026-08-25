# Design

Everything here layers on `add-generic-tracker-model`'s substrate
(`TrackerBackend` + `TrackerCaps`, `WorkItem`/`status_raw`, transitions,
`<instance>:<key>` routing, `tracker_meta`/`issue_detail_cache`). Where that
change has not landed at implementation time, the affected phase blocks on it —
nothing here forks the substrate.

## 1. Conformance: caps ⇔ ops agreement, offline

The provider-seams spec already gives us probe-shape conformance
(`thegn_svc::conformance::assert_report_invariants`, per-account factory
coverage). What it cannot see is **caps honesty**: a provider claiming
`comments: true` while inheriting the unsupported default, or claiming
`false` while implementing the op (the chrome would hide a working action).

Mechanism — a table test per provider, no network:

- Construct each backend **unconfigured** (empty token/base URL — construction
  is pure; only op calls do I/O).
- For every `(cap, op)` pair in a single shared table: if the backend's
  `caps()` flag is false, the op MUST return an error classifying as
  `Unsupported` **immediately** (the default impls short-circuit before any
  I/O); if true, the op MUST return anything _but_ `Unsupported` (an
  unconfigured backend fails `NotConfigured`/`Auth` — proving an
  implementation exists behind the flag).
- The same harness runs against `PluginIssueBackend` with a scripted plugin
  (the existing shell-script bridge test fixture) so bridged providers obey
  the identical contract.

Prereqs this creates:

- `IssueError` gains an `Unsupported` variant and `impl SeamError for
IssueError` (`Unsupported`/`NotConfigured`/`Auth`/`Api`→`Other`/
  `Network`→`Transient|Other`/`Subprocess`→`NotInstalled|Other` by content,
  `Parse`→`Other`). `is_transient()` collapses into `class()`. The optional-op
  default impls stop returning stringly `Api` errors.
  (`add-generic-tracker-model` already specs "a typed `Unsupported` error";
  the `SeamError` impl and classification table are this change's delta.)
- `TrackerCaps` gains `labels: bool` — `attach_label`/`detach_label` exist on
  the seam today but the in-flight caps struct cannot declare them, so the
  chrome could never offer label actions without probing by error.

## 2. Provider parity (per-backend honest ceilings)

Resulting caps matrix (after this change; Linear per the in-flight change):

| cap           | linear | github | jira | kaneo | notion | plane |
| ------------- | ------ | ------ | ---- | ----- | ------ | ----- |
| projects      | T      | F      | T    | T     | F      | T     |
| cycles        | T      | F      | F    | F     | F      | T     |
| subtasks      | T      | F      | T    | F     | F      | T     |
| transitions   | T      | F      | T    | F     | F      | F     |
| custom_fields | F      | F      | F    | F     | F      | F     |
| comments      | T      | T      | T    | T     | T      | T     |
| labels        | T      | T      | T    | T     | T      | T     |
| boards        | F      | F      | F    | T     | F      | F     |
| create        | T      | T      | T    | T     | T      | T     |

- **Jira** (native REST, `crates/thegn-svc/src/issue/jira.rs`):
  `available_transitions` = GET `/rest/api/3/issue/{key}/transitions`,
  `transition` = POST same (only legal transitions ever applied — the
  workflow-constrained case the transitions model exists for); comments =
  POST `/rest/api/3/issue/{key}/comment` with a minimal ADF paragraph (the
  ADF text-walker already exists for reads); labels = PUT
  `/rest/api/3/issue/{key}` with `update.labels` add/remove; projects = GET
  `/rest/api/3/project/search`; subtask kind from `issuetype.subtask` +
  `fields.parent` → `parent_id`. Cycles stay false (see Non-goals).
  The `jira-cli` project linked from THE-50 was considered and rejected as a
  backend: the seam rule is native client first, vendor CLIs only inside impl
  files when they _are_ the integration (`gh`) — Jira already has a native
  REST impl.
- **GitHub Issues** (`gh` subprocess, dir-anchored): comments = `gh issue
comment <n> --body`; labels = `gh issue edit <n> --add-label/--remove-label`.
  Everything tier-shaped stays false — GitHub Projects v2 is a _different
  provider_ (deferred), not a cap on this one.
- **Kaneo**: already implements comments + labels; they get declared via caps.
  Its board/project browsing (today reachable only through
  `IssueBackend::as_kaneo()` + the `thegn kaneo` CLI) moves onto the generic
  tier ops (`list_projects`/`project_items`; `boards: true`), and **the
  `as_kaneo()` downcast is deleted from the trait** — the last vendor leak in
  the seam. `thegn kaneo project/board/task` verbs re-route through the
  router's generic ops; `thegn kaneo login` (device flow) moves behind
  provider-generic `thegn tracker login kaneo` with the old verb kept as an
  alias (both project the same catalog row).

## 3. New providers

### Notion (`crates/thegn-svc/src/issue/notion.rs`)

- API version pinned `2025-09-03` (`Notion-Version` header; bearer
  integration token). Items are pages of one **data source**:
  `[issues.notion] data_source_id` (a `database_id` is accepted and resolved
  to its single data source once, cached in `tracker_meta`).
- **Property mapping** is config, with defaults: `status_property`
  ("Status"), `assignee_property` ("Assignee"), `labels_property` ("Tags"),
  `priority_property` ("Priority"), title from the title property, URL from
  the page. Status canonicalizes by the status property's **group**
  (To-do → Todo, In progress → InProgress, Complete → Done) with the option
  name preserved as `status_raw`; a select property (no groups) maps by name
  heuristic with `status_raw` intact.
- Ops: list = `PATCH /v1/data_sources/{id}/query` (filter compiled from
  `IssueFilter`); get = page retrieve + `GET /v1/comments`; create/update =
  pages API writing mapped properties; search = `POST /v1/search` filtered to
  the data source; comments = `POST /v1/comments`. Labels = multi-select
  option add/remove on `labels_property`.
- `filter_assignee_me` needs `[issues.notion] user_id` (integration tokens
  are bots — there is no "me"); unset ⇒ the assignee filter is skipped with a
  one-time warning, never silently empty.
- Caps: comments/labels/create true; projects/cycles/subtasks/transitions
  false (status is a free-set property ⇒ the simple status-update fallback).

### Plane (`crates/thegn-svc/src/issue/plane.rs`)

- REST, `X-API-Key`; `base_url` default `https://api.plane.so` (self-hosted
  overrides), `workspace_slug` required, `project_id` optional — empty means
  list the workspace's projects (cached in `tracker_meta`, TTL) and fan in.
  Rate limit is 60 req/min: the refresh batches per-project calls and honors
  429 with backoff; the multi-project fan-in is bounded by `max_issues`.
- Endpoints: `/api/v1/workspaces/{slug}/projects/{id}/work-items/` (older
  deployments serve `/issues/` — the probe detects which and the impl follows;
  one path, chosen once per account). States per project carry **groups**
  (`backlog`/`unstarted`/`started`/`completed`/`cancelled`) mapping 1:1 onto
  `IssueStatus` — `status_raw` keeps the state name. Cycles, labels, comments,
  sub-issues (`parent`) are first-class endpoints.
- Caps: projects/cycles/subtasks/comments/labels/create true; transitions
  false (any state is settable — free-set update path); boards false.

Both: `IssueProviderKind` gains `Notion`/`Plane` (implemented, not reserved),
`backend_from_account` arms, `IssueAccount` fields (`data_source_id`,
`user_id`, `workspace_slug` reused for Plane), `IssuesOverlay` arms
(`[issues.notion] data_source_id`, `[issues.plane] project_id` per-repo pins),
probe rows in `thegn doctor` (missing token/base ⇒ `Unavailable` with reason),
and conformance-registry coverage (the per-account factory test iterates
`IssueProviderKind::ALL` and picks these up mechanically).

## 4. Plugin tracker capabilities (plugin-runtime delta)

- The `IssueProvider` contribution gains an optional `caps` object (same
  field names as `TrackerCaps`; omitted ⇒ all false). Declared **statically in
  the manifest** rather than via a `caps` op: caps are needed at router build
  and chrome render — a per-build RPC round-trip to every plugin would put
  plugin latency on the hydration path for a value that is config-lifetime
  constant.
- `PluginIssueBackend::caps()` returns the declaration. Ops whose cap is
  false are refused **locally** with `Unsupported` (no round-trip), mirroring
  native default impls; ops whose cap is true ride `provider.call` with the
  existing `unsupported`-reply fall-through kept as a second net (a plugin
  that overclaims degrades instead of erroring the panel).
- The op vocabulary extends with the tracker-tier ops
  (`available_transitions`, `transition`, `list_projects`, `project_items`,
  `list_cycles`) — additive strings on the same `{"seam":"issues","op":…}`
  wire; old plugins are untouched (their caps default false, so the new ops
  are never sent).
- The conformance harness (§1) runs against a scripted plugin, so bridged
  backends satisfy the same caps⇔ops contract as native ones.
- `docs/extending/` gains the "tracker provider as a plugin" recipe row.

## 5. Spec-DD linking (`spec-linking`, new capability)

**Product framing**: this works on any workspace repo that carries a
supported spec-change layout. thegn's own repo is one instance, not the
target. Nothing here parses thegn-specific conventions beyond the formats'
own documented layouts.

- **Format seam**: `config_enum! SpecFormatKind { OpenSpec = "openspec",
SpecKit = "spec-kit", Bmad = "bmad" (reserved) }` — implemented-or-reserved
  like every kind. Detection markers, checked per workspace root:
  - `openspec`: `openspec/changes/<id>/proposal.md` (skipping `archive/`).
  - `spec-kit`: `.specify/` present and `specs/<NNN>-<slug>/spec.md` dirs.
  - `bmad`: reserved until its artifact conventions are pinned (docs/stories
    layouts vary across BMAD versions; strict validation names it reserved).
- **Pure core** (`thegn_core::spec_link`, 95% gate): parse
  `SpecChangeMeta { dir, format, id, title, issue_refs, tasks_done,
tasks_total }` from bounded reads (first 4 KiB of `proposal.md`/`spec.md`;
  checkbox counts from `tasks.md`), and resolve links:
  - **Declared refs**: `PROVIDER-KEY` tokens (`[A-Z][A-Z0-9]+-\d+` — Linear/
    Jira style), `#<n>` (GitHub, when the workspace repo maps to a configured
    account), and full issue URLs matching a configured provider host.
    Candidates match against the **cached issue set** (`number`/`url`), never
    free-floating — a stray `ABC-123` in prose that matches no known issue
    links nothing.
  - **Branch association** (spec-kit's native convention): a worktree whose
    branch equals a spec dir name (`NNN-slug`) links that spec to the
    worktree; if that worktree is issue-linked (`issue_links`), the issue and
    spec connect transitively.
  - `resolve_links(changes, issues, worktree_branches) → Vec<SpecLink>` is a
    deterministic pure function, table-tested.
- **Host**: the scan runs off-thread inside the existing model hydration
  (`Utility` QoS on macOS), cached by directory mtime — one `stat` when
  nothing changed; results land on the model via the existing channel +
  `TerminalWaker` pulse. **Damage: chrome (`Full`) only**; an idle wake with
  no spec delta stays `Skip`. No new tick, no watcher (the diff panel's
  fs-watcher is not extended — mtime-on-hydrate is enough for slowly-changing
  spec folders).
- **Panel**: linked Issues/Mine rows carry a spec badge; the detail view adds
  a "Spec" block (change id, title, `tasks_done/tasks_total`); a new action
  (`issues.open_spec`) opens the proposal via the existing viewer/editor
  seam. Help: the work-panel help page claims the action id and badge
  (`panel:issues` context; help + prose ratchets).
- **Dispatch seeding** (AI-additive): when a dispatch target issue has a
  resolved spec link, the launch env gains `THEGN_ISSUE_SPEC_DIR` (absolute
  path) and `THEGN_ISSUE_SPEC_FORMAT`, alongside the existing
  `THEGN_ISSUE_*` vars; if `add-issue-autopilot`'s `TaskKind::IssueImplement`
  has landed, its prompt vars gain optional `spec_dir` (empty when no link).
  Seeding passes **paths, not content** — the agent reads the spec itself
  under whatever sandbox it runs in.
- **Config**: `[specs] enabled = true`, `formats = []` (empty = auto-detect
  all implemented kinds; naming a reserved kind fails
  `config validate --strict` per the seam rule). Disabled ⇒ no scan, no
  badge, no env vars.
- **No DB change**: links are derived state (repo scan × issue cache),
  recomputed on hydration. A persisted manual-override link is an open
  question below, not scoped.

## Rendering & event loop

All new work is off-loop: provider calls stay on `spawn_blocking` seam threads
(the existing hydrate_tracker path), the spec scan rides model hydration, and
every completion is channel + `TerminalWaker` pulse. Damage is chrome-only
(`Full` on panel-content change); idle wakes stay `Skip`. No new tick or
timeout anywhere in this change.

## Security

- **Credentials**: Notion/Plane tokens enter only as `env:`/`file:`/
  `keyring:` refs on `IssueAccount.token` / the sub-tables (the existing
  `expand_env_ref` + secret-store path); `config.toml.example` documents refs
  only, never raw keys. Stored logins (Kaneo device flow, future
  `tracker login` keys) live under `$XDG_STATE_HOME/thegn/…` mode 0600.
  Probes never print token material — availability + reason only.
- **New write surfaces**: Jira transitions/comments/labels, GitHub
  comments/labels, Notion/Plane writes. Blast radius is bounded: no delete
  ops exist on the seam; writes are user-invoked panel/CLI actions, and
  agent-reachable writes go only through HouseTracker, which is gated by
  `[issues] agent_write = false` (default) per `add-generic-tracker-model`
  and the in-flight MCP scope-gating work — this change adds **no new
  agent-reachable door**.
- **Plugin caps are claims, not proof**: a plugin's declared caps gate the
  chrome; the write path into plugin code carries whatever the plugin does
  with it. That trust decision is the plugin-accept step (existing
  plugin-runtime contract), unchanged here; false-cap local refusal actually
  _narrows_ what reaches a plugin.
- **Spec-linking reads untrusted repo content**: proposal/spec files are
  repo-controlled input. Parsing is bounded (4 KiB head reads, entry caps) and
  pure (no exec, no network); ref-matching against the cached issue set
  prevents link injection to arbitrary ids' actions. Seeding passes paths
  only, so no repo-controlled prose is injected into prompts by thegn itself
  (the agent reading repo files is the same trust decision as running it in
  the repo at all — same class as the existing `THEGN_ISSUE_BODY` injection,
  which remains the riskier surface).
- **Rate/abuse**: Plane's 60 req/min is honored with batching + 429 backoff so
  a misconfigured refresh cannot hammer a self-hosted instance.

## Alternatives considered

- **`jira-cli` as the Jira backend** — rejected; native REST exists and the
  seam rule keeps vendor CLIs to impls where the CLI _is_ the integration.
- **Caps as a plugin RPC (`caps` op)** — rejected for a manifest field;
  build-time RPC puts plugin latency on hydration for a config-constant value.
- **Notion databases as a Project tier** — rejected; a data source is the
  _scope_ of one account (like a Jira project key), not a tier inside it. Two
  Notion databases = two accounts.
- **Persisting spec links in the state DB** — rejected for derived scan;
  git is the source of truth for repo content, the DB is a cache, and the
  scan is one mtime-guarded read per hydration.
- **Synthesizing transitions for Plane from its state list** — rejected;
  free-set providers take the honest `transitions: false` + simple-update
  path the model already defines. Fabricated transitions are exactly what
  caps honesty forbids.

## Open questions

- **Manual spec-link override**: when ref parsing misses (issue tracked in a
  system the change doesn't name), is an explicit link action (persisted
  where?) worth a table, or is "add the ref line to the proposal" the answer?
  Leaning: the latter — the spec folder is the source of truth.
- **Jira sprints**: worth the Agile-API board-discovery cost later, or leave
  `cycles: false` permanently for Jira and let boards arrive with
  `add-tracker-board-view`?
- **Notion sub-items**: Notion models sub-items as a self-relation property;
  mapping it to `parent_id` needs another property-name config key. Deferred
  until someone asks.
- **`thegn issue` (forge) vs the tracker seam**: `cmd/issue.rs` talks to the
  _forge_ (`gh`) independently of `IssueRouter`. Out of scope here, but the
  audit flags the eventual convergence question: should forge-issue verbs
  route through the tracker router when a GitHub tracker account is
  configured?
