# THE-20 — Embedded skills architecture

## Decision

Add a versioned skill registry to `thegn-core` and a host-side, marker-aware
worktree seeder. The registry has two sources:

1. reviewed built-ins compiled with `include_str!` from `extensions/skills`, and
2. user-authored skill packages discovered from `[skills].user_dirs`.

Each package is one path-safe skill name plus a `SKILL.md`. Its YAML-style
frontmatter is the contract for `name`, `description`, `harnesses`, `gate`, and
`when`. The core parses and validates bytes, evaluates gates, and produces a
pure seed plan. Only the host reads directories, resolves harness layouts, and
applies that plan.

The public command is `thegn skills list|show|seed`. `seed` targets one
worktree (the current worktree by default), all configured harnesses, and the
configured gate state. It does not synchronize agent home directories. Existing
worktree creation and persisted-worktree seeding call the same host seeder, so
`mq`, `pipeline`, and `supervise` keep their current gates and Claude paths;
they are merely registry entries now. `tui-check` remains a development-only
extension and is not shipped.

`skills.list` is also a read-only HTTP control capability and is included in
the control-wire schema snapshot. Its response is metadata only. `skills.show`
and `skills.seed` remain local CLI operations: a remote control caller must not
turn the API into an arbitrary filesystem writer. The list implementation in
the daemon may include valid configured user directories; malformed external
packages are reported and skipped at that edge.

No database migration, network registry, request-time prompt injection, skill
execution, or TUI panel is part of this change.

## Verified current state and draft reconciliation

The existing implementation is deliberately small but has the wrong ownership
and safety boundary:

| Evidence                                                                                                                       | Finding                                                                                                         | Design consequence                                                                                                                                                                                                |
| ------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-host/src/mq_assets.rs:1-20,46-81`                                                                                | The host owns a hand-written `ASSETS` table containing `mq`, `pipeline`, `supervise`, and two MQ command files. | Move skill metadata and gate logic to core; preserve the two command files as legacy MQ assets in the host seeder. There must be one skill registry, not a second host table.                                     |
| `crates/thegn-host/src/mq_assets.rs:98-110`                                                                                    | `seed` calls `std::fs::write` unconditionally.                                                                  | Replace it with survey → pure plan → apply. An unmarked file or a marked file with a changed hash is always preserved.                                                                                            |
| `crates/thegn-host/src/mq_assets.rs:113-139`                                                                                   | Startup and worktree creation seed persisted/local worktrees through this module.                               | Keep all call sites, but delegate to the new seeder and retain best-effort behavior for background work.                                                                                                          |
| `crates/thegn-host/src/wizard.rs:1065-1074`, `crates/thegn-host/src/cmd/wt.rs:261-269`, `crates/thegn-host/src/run.rs:677-682` | There are three existing seed entry points.                                                                     | The new API must be usable from explicit CLI work and from the existing background scheduling path; no new polling tick or render state is introduced.                                                            |
| `crates/thegn-core/src/harness.rs:207-279,281-309`                                                                             | `Harness` is the object-safe closed seam and `HARNESSES` is the registry.                                       | Add a pure skill-layout capability to each harness implementation. Vendor path literals live only in those implementations.                                                                                       |
| `crates/thegn-core/src/harness.rs:315-334,367-390,475-513`                                                                     | Codex, Claude, and Pi already have distinct identity/home facts.                                                | Project layouts are `Claude: .claude/skills/<name>`, `Codex: .agents/skills/<name>`, and `Pi: .pi/skills/<name>`. Unsupported harnesses return no project skill layout and doctor says so.                        |
| `crates/thegn-core/src/config.rs:5176-5418,5556-5763`                                                                          | `Config` is the schema and `ConfigOverlay` is the env layer; there is no skills table.                          | Add a small `config_skills.rs` module, flatten it into `Config`, expose shallow fields through the existing env overlay, and document every key in the example config. Do not grow `config.rs` with parser logic. |
| `crates/thegn-core/src/capability.rs:184-704,1320-1360` and `crates/thegn-core/src/control.rs:241-490`                         | The catalog has exactly one row per `Verb`, with centralized scope policy.                                      | Add `SkillsList` and `SkillsSeed` verbs, rows `skills.list` and `skills.seed`, and one scope decision in `required_scope`. `skills.list` claims CLI+HTTP; `skills.seed` claims CLI only.                          |
| `crates/thegn-svc/src/control/routes.rs:29-148,148-205` and `crates/thegn-svc/tests/control_schema.rs:11-82`                   | HTTP routes and `API_CALLS` are mirrored, and the committed JSON schema includes both routes and wire types.    | Add `GET /v1/skills`, its `SkillInfo` wire row, route/API row, daemon implementation, and regenerate `docs/api/control-v1.json` additively. Do not add a seed route.                                              |
| `crates/thegn-host/src/cli_help.rs:15-66,133-160`                                                                              | Every visible top-level command must be assigned to exactly one help group.                                     | Add `skills` to Meta and let the existing exact-coverage test fail until it is present.                                                                                                                           |
| `crates/thegn-host/src/help/pages.rs:9-39,105-145`                                                                             | Authored help pages are explicitly embedded and checked against `docs/help`.                                    | Add one skills help page and its `include_str!`; keep its prose free of unregistered bindable-action claims.                                                                                                      |
| `crates/thegn-core/src/completion/catalog.rs:230-505` and `crates/thegn-host/src/complete.rs:496-545`                          | Value-taking clap slots must be classified or pinned; the slot ratchet is shrink-only.                          | Add a `Skill` completion source for `skills show <name>` and mark `skills seed --worktree` structural. Regenerate the slot ratchet; do not add an unexplained filename-completion debt line.                      |

The openspec draft was checked against that evidence. Its embedded/versioned
frontmatter and marker/hash ideas are useful (`proposal.md:28-52`,
`specs/skills/spec.md:5-24,55-70`). Its `skills sync`, home-directory adapters,
`auto_sync`, `--remove`, and startup-home-write requirements are cut: they
conflict with THE-20's per-worktree seed framing and the repo's host boundary
(`proposal.md:35-47,72-81`, `specs/skills/spec.md:32-42,72-91`). The draft's
“closed, reviewed-only” custom-package non-goal (`proposal.md:76-79`) is also
replaced by the issue's required user-defined directories, with duplicate-name
and path-safety rules below. The draft's `tui-check` exclusion is retained
(`design.md:8-12`). No existing task is assumed complete merely because it is
listed in `tasks.md:3-50`; the current code still has the hand-written seeder
and no `skills` CLI.

## Layered design

```text
Config + HARNESSES
        │
        ▼
thegn-core::skills
  parse/validate → registry → gate evaluation → pure seed plan
        │                         │
        │                         └─ CLI/catalog/control metadata
        ▼
thegn-host::skill_seed
  discover user dirs + survey worktree + map harness layout + apply plan
        │
        ▼
<worktree>/.claude/skills, .agents/skills, .pi/skills
```

### Core registry and frontmatter

Create `crates/thegn-core/src/skills.rs`. Keep it substrate-free: no filesystem,
tokio, clap, termwiz, process spawning, or harness vendor SDK. The module
should expose plain owned data and pure functions roughly equivalent to:

- `SkillDocument { name, description, harnesses, gate, when, body, source }`;
- `SkillRegistry { skills }`, with deterministic name ordering and duplicate
  rejection;
- `parse_document(bytes, source)` and `validate_registry(registry)`; and
- `plan_seed(registry, target, existing, gate_state)` returning writes,
  unchanged entries, skipped-unmarked entries, skipped-adopted entries,
  removed-managed entries, and diagnostics.

The accepted frontmatter is bounded and intentionally smaller than general
YAML:

```yaml
---
name: mq
description: Drive the thegn merge queue.
harnesses: claude,codex,pi
gate: merge_queue
when: create,startup,explicit
---
```

`name` must equal the package directory name for user packages, be a single
non-empty path segment, and reject separators, `..`, control characters, and
platform-specific absolute/prefix forms. `description` must be non-empty and
bounded. `harnesses` contains known `HARNESSES` ids; an unknown id is a
validation diagnostic, not a path guess. `gate` is `always`, `merge_queue`, or
`pipeline`; `when` is a non-empty subset of `create`, `startup`, and `explicit`.
Unknown keys and malformed delimiters fail that document. The body is opaque
prose after the closing delimiter.

The registry manifest uses `include_str!` for the reviewed built-ins. The
existing `extensions/skills/mq/SKILL.md`, `pipeline/SKILL.md`, and
`supervise/SKILL.md` become the three built-in sources with the same body and
effective gate as today: MQ enabled, pipeline stages non-empty, and always,
respectively. Their frontmatter adds harness and `when` metadata. The registry
does not include `tui-check`.

External discovery is host-owned. It reads only immediate child directories of
each configured `user_dirs` entry and accepts `<child>/SKILL.md`; it does not
execute, source, or recursively crawl arbitrary files. Built-ins win on a
duplicate name; the duplicate external package is skipped and reported. A
failed directory or document is a doctor/seed diagnostic, not a process-fatal
error. This is the required degrade-at-the-edge behavior.

### Gates, markers, and the pure plan

`gate_state` is a small core value (`merge_queue_open`, `pipeline_open`) built
by the host from existing config/runtime state. `when` is selected by the
caller: worktree creation and persisted-worktree reconciliation use their
existing `create`/`startup` phases; `skills seed` uses `explicit`. A skill must
pass both its harness target and gate/phase before it enters the plan.

The canonical managed file is the skill body wrapped with a reserved marker
frontmatter block containing `thegn_managed: true`, shipping version, and
`thegn_hash: sha256:<digest>`. Hash the canonical unmarked document, not the
destination path. Use the workspace's existing digest dependency if present;
do not add a crypto implementation. The core plan consumes an abstract
`ExistingFile { relative, bytes, managed_marker, recorded_hash }`, so tests do
not touch a filesystem.

Apply rules are strict:

- absent target → write the managed rendering;
- managed target whose recorded hash matches the canonical current hash → no-op;
- managed target with a mismatching actual hash → preserve and report adopted;
- unmarked target → preserve and report user-owned;
- excluded or no-longer-shipped target → remove only when its marker and hash
  prove it is still an unmodified thegn file;
- all operations are deterministic and a second plan after a successful apply
  is empty.

For migration, the host may recognize an existing exact legacy `mq`/
`pipeline`/`supervise` body as pristine and replace it once with the managed
rendering. Any other existing content is user-owned. Legacy MQ command files
are separate managed assets and retain their exact command paths and gate;
they use the same marker-aware write policy, with exact old content accepted as
the one-time pristine migration case.

### Harness seam

Extend `thegn_core::harness::Harness` with a pure project skill-layout result,
not a path-building helper that accepts arbitrary vendor names. Each harness
implementation returns its own relative root (or `None`). The host combines
that root with the worktree path and validated skill name. No `.claude`,
`.agents`, `.pi`, or future vendor path appears in a generic registry or
config parser. The existing harness registry remains closed; config selects
configured harness ids but cannot invent a harness (`docs/extending/harness.md:3-9,48-68`).

Configured targets are the distinct harness ids referenced by resolved
`[[agents]]`/pipeline entries, with the current default behavior continuing to
select Claude. A configured harness with no skill layout is listed by doctor
as unsupported and is skipped without making the seed fail.

### Host seeding and lifecycle integration

Create `crates/thegn-host/src/skill_seed.rs` as the only filesystem adapter.
It owns user-dir discovery, path expansion, harness target resolution,
worktree survey, managed-marker parsing, bounded writes/removals, and result
formatting. It also carries the two legacy MQ command assets while
`mq_assets.rs` is removed; no second `ASSETS`/gate registry survives.

The explicit `skills seed` command runs outside the compositor loop and returns
non-zero only for an invalid explicit target or an unrecoverable target access
failure. Per-file conflicts and malformed user packages are reported while
other targets continue. Existing automatic call sites remain best-effort. The
persisted-worktree startup path and the wizard path must submit the bounded
filesystem job through the repository's existing background/spawn-blocking
mechanism after the first frame where applicable; they must not add a tick,
block render, or alter render-plan state. `wt new`'s non-interactive command
path may run the same adapter directly.

### CLI, doctor, and control catalog

Add `cmd::skills::Action` with:

- `list [--json]`: deterministic metadata, including source, description,
  harness targets, gate, and seed phases;
- `show <name>`: the canonical embedded or configured document, with no write;
- `seed [--worktree <path>] [--json]`: apply the per-worktree plan for all
  configured harnesses.

`show` and `seed` use the `Skill` completion source and structural worktree
completion. JSON output must have stable field names and sort order.

Add a sibling `cmd::skills_doctor` rather than growing `cmd::doctor.rs` into
another probe god-file. `thegn doctor` reports each configured/known harness:
project root, supported layout, target directory found, managed current,
managed stale, managed user-modified, absent, and any discovery diagnostics.
It is read-only, bounded to the selected/current worktree plus configured user
directories, and never “repairs” a target. Text and JSON must report the same
state model.

Add `Verb::SkillsList` (`Read`) and `Verb::SkillsSeed` (`Write`) to the single
core catalog. `skills.list` claims `Cli|Http`; `skills.seed` claims `Cli` only.
Add `GET /v1/skills` to the existing routes/API_CALLS spine and a
`SkillInfo` wire type. The route returns metadata and diagnostics for the
daemon's configured registry; it cannot write a worktree. Implement the
default/daemon `ControlApi` method in the existing seam, update the fake, and
regenerate `docs/api/control-v1.json` with the additive snapshot test. No
gRPC/MCP/plugin gap is added because those surfaces are deliberately excluded
from the catalog row.

### Config and documentation ratchets

Add a small `SkillsConfig` in `crates/thegn-core/src/config_skills.rs` and a
`skills` field on `Config` with compatibility-preserving defaults:

```toml
[skills]
enabled = true
user_dirs = []
exclude = []
```

`enabled = true` preserves today's automatic worktree seeding. `user_dirs` is
empty by default. `exclude` is a deterministic list of validated skill names.
There is intentionally no `auto_sync`, home sync, or `--remove` setting.
Add shallow env overlay keys using the repository's established naming (for
example `THEGN_SKILLS_ENABLED`, `THEGN_SKILLS_USER_DIRS`, and
`THEGN_SKILLS_EXCLUDE`), exercise them in the config tests, and run the
env-overlay ratchet. The example config is schema documentation, not a copied
implementation detail.

Update `config/config.toml.example`, `docs/cli.md`, a new embedded
`docs/help/skills.md`, the configuration help, and the current pipeline-board
help that describes the three legacy seeds. Register the page in
`crates/thegn-host/src/help/pages.rs`. Run the completion-slot, help, and
control-schema ratchets in the same implementation chunks. The expected
completion/help ratchet result is classification/documentation of the new
surface, not an unexplained debt increase; commit generated changes if the
ratchet tooling changes bytes.

## Tests and invariants

Core unit tests must cover: frontmatter grammar and path safety; every embedded
manifest; unknown harness/gate/phase; gate selection; duplicate precedence;
managed rendering and digest; absent/current/upgrade/deprecated targets;
unmarked and hash-mismatched preservation; exclusion; harness filtering; and
second-plan idempotence. Keep these tests in core with byte fixtures and no
filesystem.

Host tests must use temporary directories only and cover: user-dir discovery,
all three native layouts, configured-harness resolution, exact legacy seed
compatibility, marker-aware apply, malformed external package degradation,
doctor state parity, and the three existing lifecycle call paths. CLI/help,
catalog, route/API mirror, and control-schema tests cover their respective
ratchets. Do not run e2e or a full-workspace build for this issue.

## Delivery order

Use the three chunks below in order. Chunk 2 consumes the core API and chunk 3
documents the final CLI/help surface, so they are intentionally serial even
where their paths are disjoint. Each coder commits exactly the subject stated
in their chunk. The architect commit containing only this design and the three
chunk files is `docs(the-20): architect design + chunk specs`.
