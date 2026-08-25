# Spec Linking

Issue ↔ spec-change linking for workspace repos that carry a supported
spec-driven-development layout (OpenSpec, spec-kit; BMAD reserved). A product
feature for any user repo — thegn's own repo is one instance, not the target.

## ADDED Requirements

### Requirement: Spec formats are a seam with detection markers

Spec formats SHALL be a `config_enum!` kind (`openspec`, `spec-kit`
implemented; `bmad` reserved until its artifact conventions are pinned),
detected per workspace root by each format's documented layout — OpenSpec:
`openspec/changes/<id>/proposal.md` excluding `archive/`; spec-kit:
`.specify/` present with `specs/<NNN>-<slug>/` change dirs. Detection and
parsing SHALL be pure, bounded reads (head-limited file reads, capped entry
counts) with no execution and no network, and `SpecChangeMeta` SHALL carry the
change's dir, format, id, title, declared issue refs, and task checkbox
progress. A `[specs]` config table (`enabled`, `formats` — empty meaning all
implemented kinds) SHALL gate the feature; naming a reserved kind MUST fail
`config validate --strict` per the seam rule, and `enabled = false` MUST mean
no scan, no badges, and no seeded variables.

#### Scenario: OpenSpec change folders are detected and parsed

- **WHEN** a workspace repo contains `openspec/changes/add-foo/proposal.md` and `tasks.md` with 3 of 9 checkboxes done
- **THEN** the scan yields a `SpecChangeMeta` with format `openspec`, id `add-foo`, its declared issue refs, and progress 3/9 — while folders under `openspec/changes/archive/` are skipped

#### Scenario: Reserved format is refused strictly

- **WHEN** `[specs] formats = ["bmad"]` and the user runs `config validate --strict`
- **THEN** validation fails naming `bmad` as reserved, rather than silently scanning nothing

### Requirement: Issues link to spec changes deterministically

Link resolution SHALL be a pure function over the scanned changes, the cached
issue set, and the workspace's worktree branches. Declared refs
(`PROVIDER-KEY` tokens, `#<n>` numbers when the repo maps to a configured
account, and full issue URLs of configured provider hosts) MUST match only
against the cached issue set — a token matching no known issue links nothing.
A worktree whose branch equals a spec-kit change dir name SHALL link that spec
to the worktree, and transitively to the worktree's linked issue via
`issue_links`. Links are derived state: recomputed from the scan and the issue
cache, never persisted to the state DB.

#### Scenario: Declared ref links to a cached issue

- **WHEN** a proposal contains `Linear: ENG-123` and the issue cache holds an item with that key
- **THEN** resolution yields a link between that issue and the change, and a stray `XYZ-999` matching no cached issue yields no link

#### Scenario: Branch association links transitively

- **WHEN** a worktree on branch `004-user-auth` matches a spec-kit dir `specs/004-user-auth/` and that worktree is linked to an issue in `issue_links`
- **THEN** the spec change links to that issue through the worktree association

### Requirement: Linked specs surface in the work panel

Issues with a resolved spec link SHALL carry a spec badge in the Issues/Mine
rows, and the issue detail view SHALL show the linked change (id, title, and
task progress `tasks_done/tasks_total`). An `issues.open_spec` action SHALL
open the linked proposal through the existing viewer/editor seam. The action
and badge belong to the work panel's help context (`panel:issues`) and MUST be
claimed and described by its `docs/help/` page per the help and prose
ratchets.

#### Scenario: Badge and detail block render for a linked issue

- **WHEN** an issue in the panel has a resolved spec link with 3 of 9 tasks done
- **THEN** its row carries the spec badge and its detail view shows the change id, title, and "3/9" progress

#### Scenario: Opening the spec from the panel

- **WHEN** the user invokes `issues.open_spec` on a linked issue
- **THEN** the change's proposal opens via the viewer/editor seam; on an unlinked issue the action is absent

### Requirement: Spec scanning stays off the event loop

The spec scan SHALL run off-thread within existing model hydration (Utility
QoS where applicable), guarded by directory mtime so an unchanged tree costs
one stat, delivering results over the existing channel with a `TerminalWaker`
pulse. Damage MUST be chrome-only (`Full` when panel content changed); an idle
wake with no spec delta stays `Skip`. No new tick, timeout, or filesystem
watcher is introduced.

#### Scenario: Unchanged spec tree costs one stat

- **WHEN** hydration runs and no spec directory mtime changed since the last scan
- **THEN** the cached scan result is reused after a single stat and no parsing occurs

#### Scenario: Idle stays idle

- **WHEN** the loop wakes with no spec, issue, or chrome delta
- **THEN** the render decision is `Skip` — spec linking adds no periodic work

### Requirement: Dispatch seeding is additive and path-only

When an issue with a resolved spec link is dispatched to an agent, the launch
environment SHALL gain `THEGN_ISSUE_SPEC_DIR` (absolute path) and
`THEGN_ISSUE_SPEC_FORMAT` beside the existing `THEGN_ISSUE_*` variables, and
any issue-implement prompt template SHALL gain an optional `spec_dir` variable
(empty when unlinked). Seeding MUST pass paths, never file content, and every
spec-linking behaviour — detection, linking, badges, open-spec — MUST function
with no agent configured.

#### Scenario: Dispatch env carries the spec location

- **WHEN** an agent is dispatched for an issue linked to `openspec/changes/add-foo/`
- **THEN** the launch env contains `THEGN_ISSUE_SPEC_DIR` with that absolute path and `THEGN_ISSUE_SPEC_FORMAT=openspec`, and no spec file content is injected into the prompt by thegn

#### Scenario: No agent, full feature

- **WHEN** thegn runs with zero agents configured
- **THEN** spec detection, linking, badges, and `issues.open_spec` all work; only the seeding variables have no consumer
