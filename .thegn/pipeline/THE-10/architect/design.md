# THE-10 architecture design: workspaces → projects

Status: design-only. This document is the implementation contract for the
later coder chunks; it does not authorize running e2e or changing the live
state database.

## Decision

There are two concepts, and the rename must not make them share one unqualified
CLI meaning:

| Concept                                                      | Canonical product vocabulary                       | Compatibility vocabulary | Machine boundary                                                                                       |
| ------------------------------------------------------------ | -------------------------------------------------- | ------------------------ | ------------------------------------------------------------------------------------------------------ |
| one repo, one resident/sidebar item                          | **project**                                        | workspace                | existing `Workspace*` types, DB table, cache keys, and internal scope names remain                     |
| a named group of repos used by one cross-repo feature branch | **program** (also described as “multi-repo group”) | project                  | existing `Project*` code and `projects` DB table remain the storage implementation for now             |
| Linear/Kaneo account scope                                   | tracker workspace/project                          | unchanged                | provider-owned config keys such as `workspace_id`, `workspace_slug`, and `project_id` remain unchanged |

The existing `thegn project` command is the multi-repo concept, verified in
`crates/thegn-host/src/main.rs:461-466` and
`crates/thegn-host/src/cmd/project.rs:1-9`. It must therefore be renamed in
the public CLI to `thegn program`; `thegn project` remains a deprecated,
behavior-identical alias for three releases. Likewise, `thegn wt new
--program` becomes canonical and `--project` remains a deprecated alias for
the existing cross-repo feature operation (`crates/thegn-host/src/cmd/wt.rs:94-99`).
The alias warning must state that `project` is the old name for a multi-repo
program and name the replacement. The implementation must not reinterpret the
old command as the one-repo project concept.

This change does not add a new one-repo `thegn project` verb: no such
`thegn workspace` verb exists on this branch. The sidebar, palette, help, and
configuration use “project” for one repo; a future one-repo CLI namespace must
be designed separately after the collision is resolved.

## Why the existing OpenSpec draft needs pruning

The draft in `openspec/changes/rename-workspaces-to-projects/` was read in
full: `proposal.md`, `design.md`, `tasks.md`, and both delta specs. Its UI-only
boundary is a useful starting point, and these parts are already satisfied by
the current branch: there is no top-level `thegn workspace` command; the
existing `thegn project` command is the multi-repo operation; filenames/page
ids can remain stable; and tracker fields are distinct provider concepts.

The following draft claims are not accepted:

- It says aliases remain indefinitely and have no release warning. The binding
  requirement is a bounded compatibility window with a deprecation warning;
  this design chooses N = 3 stable releases.
- It says no workspace-named environment variable exists. The current loader
  reads `THEGN_WORKSPACES_DIR` at `crates/thegn-core/src/config.rs:5298-5300`.
  `THEGN_PROJECTS_DIR` must be canonical, with the old variable accepted and
  warned during the same window.
- It proposes `projects_dir` while the current public key is
  `workspaces_dir` (`crates/thegn-core/src/config.rs:4707-4713`). The former
  is a new canonical spelling, not an already-landed key.
- It retains capability ids wholesale. The binding requirement requires old
  ids to remain deprecated catalog aliases, projected identically, with the
  control-schema check in the same landing.
- It retains old action ids wholesale. The keymap contract instead needs
  canonical project action ids plus accepted workspace aliases, while keeping
  old user keybindings working.
- It asks for broad e2e/CI execution. This lane records the exact baseline
  inventory and future commands only; it will not run e2e, `just ci`, or a
  full-workspace build.

## Public naming and compatibility contract

### Config

Canonical documented keys are:

| Canonical key            | Accepted legacy key        | Rule                                         |
| ------------------------ | -------------------------- | -------------------------------------------- |
| `projects_dir`           | `workspaces_dir`           | canonical wins if both appear                |
| `[project.<slug>]`       | `[workspace.<slug>]`       | canonical table wins per slug if both appear |
| `confirm_delete_project` | `confirm_delete_workspace` | canonical wins                               |
| `sidebar_project_sort`   | `sidebar_workspace_sort`   | canonical wins                               |
| `THEGN_PROJECTS_DIR`     | `THEGN_WORKSPACES_DIR`     | canonical env wins                           |

The old spelling is accepted for N = 3 stable releases and every warning names
the exact legacy key/path and its replacement. If both spellings occur, the
loader uses the canonical value deterministically and emits one duplicate-key
warning naming both locations. A serde alias alone is insufficient because it
cannot provide this duplicate diagnostic reliably; put pure raw-TOML
normalization/diagnostics in a new `thegn-core::config_compat` module. The
loader remains tolerant at the edge, while `config validate` reports the
compatibility warning without turning a recognized legacy key into an unknown
key. The compatibility window and removal release must be a named constant or
documented policy, not an unexplained magic number.

Keep Rust field names and internal `Config::workspace` accessors initially to
avoid an unnecessary core-wide rename; expose the canonical serde/schema names
and normalize both table spellings before deserialization. Canonical writes,
`config get`, schema output, config-reference help, and `config.toml.example`
must use project spellings. The example must include a concise legacy-key table
and removal release. `nix/hm-module.nix` is a third schema copy, so add a
canonical `projectsDir` option that renders `projects_dir`, retain
`workspacesDir` as a deprecated compatibility option, and update its drift
test. Do not claim the home-manager output is unchanged.

Do not rename these provider-owned keys: Linear/Kaneo `workspace_slug`,
`workspace_id`, and `project_id` under tracker/account config. Qualify their
human messages as “tracker workspace”, “Linear project”, etc. where needed.

### CLI

`thegn program {list,create,rename,rm,assign}` is the canonical spelling for
the existing multi-repo group command. The old `thegn project ...` spelling is
a visible alias that warns once per invocation (or once per process, but never
silently). `wt new --program NAME` is canonical; `--project NAME` is a visible
deprecated alias with the same warning and exact old behavior. Keep old JSON
field names and output shape stable unless an existing snapshot proves a
human-only string changed. The warning must be emitted at the host edge, not
from core, and must not affect machine-readable `--json` output.

Because clap aliases do not by themselves provide a useful deprecation
diagnostic, centralize raw-argv compatibility detection in a small host
compatibility module. It must recognize only the exact old command/flag forms,
avoid false positives in values, and have unit tests. Help presents canonical
program wording and explicitly documents the old aliases.

### Capability catalog

The current catalog documents stable ids as never renamed and already has a
`deprecated` replacement field (`crates/thegn-core/src/capability.rs:106-141`).
The six existing multi-repo rows are `project.list`, `project.create`,
`project.rename`, `project.rm`, `project.assign`, and `project.new_feature`
(`crates/thegn-core/src/capability.rs:504-547`). Add canonical `program.*`
rows and retain each old `project.*` id as a deprecated alias whose `verb`,
summary, `since`, scope, and surface set are identical. The alias’s
`deprecated` field names its `program.*` replacement. This is an intentional
exception to the current one-row-per-verb test at
`crates/thegn-core/src/capability.rs:1326-1334`: change that invariant to one
non-deprecated canonical row per verb; `for_verb` must select that row.

`lookup` must resolve both ids. Surface projections and implementation ledgers
must expose both ids identically, and coverage/gap handling must not silently
double-count or hide the alias. Add tests asserting exact projection parity,
deprecation targets, lookup, and coverage. The CLI projection must advertise
canonical program ids and accept legacy project ids. Do not invent HTTP/gRPC,
MCP, or plugin routes for these currently CLI-only rows. Run
`crates/thegn-svc/tests/control_schema.rs` against `docs/api/control-v1.json` in
the chunk; the expected result is a byte-for-byte unchanged control wire
snapshot because these rows are not routed. If implementation changes the
catalog metadata included by the schema generator, update the snapshot in the
same chunk and explain the exact generated diff—never hand-edit it.

### Keymaps and help

Canonical action ids are `new-project`, `delete-project`, `switch-project`,
`next-project`, `prev-project`, and `summon-project`, corresponding to the
existing workspace action variants. `Action::key()` and generated keybinding
help use the canonical ids; `Action::from_key()` accepts both canonical ids and
the old `*-workspace` ids. Existing chords do not change. Keep one
`ACTION_SPECS` entry per action so the palette does not duplicate rows; expose
workspace words as compatibility search keywords and test every alias
round-trip. Generated keybindings/config-reference pages remain generated,
never hand-written. Help frontmatter claims canonical ids and prose names the
legacy ids once.

## State, architecture, and non-goals

Do not rename DB tables, columns, migrations, SQL, cache keys, or core
`Workspace*`/`Project*` types in this issue. The DB contract explicitly says
`projects` is the grouping layer above `workspaces` and
`workspaces.project_id` is its membership column
(`crates/thegn-core/src/db.rs:96-100`). The existing `projects` table stores
programs under the decision above; the `workspaces` table remains the one-repo
cache. No migration and no DB view/alias is needed. Never run a migration or
the built binary against live state; any manual invocation must set
`XDG_STATE_HOME` to a temporary directory.

Core compatibility parsing and catalog logic remain substrate-free and pure,
with unit tests. Warnings, CLI alias detection, and rendering stay at the
edges. Do not grow `config.rs`, `keymap.rs`, or another existing god-file with
a new subsystem: use sibling compatibility modules. Preserve the one catalog,
provider seams, pure rendering, bounded resident pool, and best-effort cache
semantics from `CLAUDE.md` and `docs/ARCHITECTURE.md`.

## Ordered implementation plan

1. Land Chunk 1. It creates canonical config parsing/schema names, bounded
   aliases and warnings, home-manager compatibility, and catalog aliases.
   It leaves current host behavior loadable, so main remains compatible.
2. Land Chunk 2 serially. It adds host-edge CLI warnings and canonical program
   aliases, then changes UI labels/sidebar/palette/keymap projections while
   preserving old ids and chords. It updates the smoke assertions in an
   isolated state directory.
3. Land Chunk 3 serially. It updates README, help prose, the multi-repo page,
   generated-page inputs, help ratchets, and only the e2e baselines whose
   reviewed diff changes. This chunk must not hand-edit generated pages or run
   e2e in this lane.

## Ordered identifier/file audit

The mechanical search starts with `rg -n -i '\bworkspace(s)?\b'` and a second
search for `THEGN_WORKSPACES_DIR`, `workspaces_dir`, `[workspace.`,
`*-workspace`, `project.*`, and `--project`. Classify every hit before editing:

1. **Canonical product terms:** sidebar `Workspaces`/`WORKSPACES`, project
   picker/menu/modal/toast/status labels, `new-workspace`,
   `delete-workspace`, `switch-workspace`, `next-workspace`,
   `prev-workspace`, `summon-workspace`, `workspace.*` config rows, and
   one-repo help/README prose → project spelling or canonical project id.
2. **Multi-repo program terms:** `thegn project`, `wt new --project`,
   `project.*` capability ids, `Project*` storage APIs, and the existing
   `docs/help/projects.md` → program wording, with old aliases retained.
3. **Provider terms:** tracker `workspace_id`, `workspace_slug`, tracker
   `project_id` → unchanged, qualified only in prose.
4. **Build/container terms:** Cargo `[workspace]`, `/workspace` container paths,
   `cargo --workspace`, and test helpers that parse Cargo metadata → unchanged.
5. **State terms:** DB `workspaces`, `workspaces.project_id`, `ui_state` keys,
   migrations, and internal `Workspace*` names → unchanged.

The exact candidate file sets and per-coder commit subjects are in the three
chunk specifications below.

## E2E baseline inventory

The audit found 43 of the 45 Linux baselines contain the old vocabulary. These
are the exact files to review/re-record after the host chunk lands:

```text
test/muse/snapshots/chrome_regions__chrome/xterm__100x30__linux.txt
test/muse/snapshots/chrome_regions__chrome/xterm__160x40__linux.txt
test/muse/snapshots/chrome_regions__chrome/xterm__200x50__linux.txt
test/muse/snapshots/chrome_regions__chrome/xterm__80x24__linux.txt
test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__100x30__linux.txt
test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__160x40__linux.txt
test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__80x24__linux.txt
test/muse/snapshots/glitch_hunt_panel_accordion__after/xterm__100x30__linux.txt
test/muse/snapshots/glitch_hunt_panel_accordion__after/xterm__160x40__linux.txt
test/muse/snapshots/glitch_hunt_rendering__after_tall_short/kitty__100x30__linux.txt
test/muse/snapshots/glitch_hunt_rendering__after_tall_short/kitty__160x40__linux.txt
test/muse/snapshots/glitch_hunt_rendering__after_tall_short/vt220__100x30__linux.txt
test/muse/snapshots/glitch_hunt_rendering__after_tall_short/vt220__160x40__linux.txt
test/muse/snapshots/glitch_hunt_rendering__after_tall_short/xterm__100x30__linux.txt
test/muse/snapshots/glitch_hunt_rendering__after_tall_short/xterm__160x40__linux.txt
test/muse/snapshots/glitch_hunt_rendering__after_wide_narrow/kitty__100x30__linux.txt
test/muse/snapshots/glitch_hunt_rendering__after_wide_narrow/kitty__160x40__linux.txt
test/muse/snapshots/glitch_hunt_rendering__after_wide_narrow/vt220__100x30__linux.txt
test/muse/snapshots/glitch_hunt_rendering__after_wide_narrow/vt220__160x40__linux.txt
test/muse/snapshots/glitch_hunt_rendering__after_wide_narrow/xterm__100x30__linux.txt
test/muse/snapshots/glitch_hunt_rendering__after_wide_narrow/xterm__160x40__linux.txt
test/muse/snapshots/glitch_hunt_rendering__before/kitty__100x30__linux.txt
test/muse/snapshots/glitch_hunt_rendering__before/kitty__160x40__linux.txt
test/muse/snapshots/glitch_hunt_rendering__before/vt220__100x30__linux.txt
test/muse/snapshots/glitch_hunt_rendering__before/vt220__160x40__linux.txt
test/muse/snapshots/glitch_hunt_rendering__before/xterm__100x30__linux.txt
test/muse/snapshots/glitch_hunt_rendering__before/xterm__160x40__linux.txt
test/muse/snapshots/glitch_hunt_resize__after_storm/xterm__100x30__linux.txt
test/muse/snapshots/palette__theme_query/kitty__100x30__linux.txt
test/muse/snapshots/panel_git__branches/xterm__100x30__linux.txt
test/muse/snapshots/panel_git__branches/xterm__160x40__linux.txt
test/muse/snapshots/panel_system__system/xterm__100x30__linux.txt
test/muse/snapshots/panel_work__work/xterm__100x30__linux.txt
test/muse/snapshots/responsive_breakpoints__layout/xterm__100x30__linux.txt
test/muse/snapshots/responsive_breakpoints__layout/xterm__160x40__linux.txt
test/muse/snapshots/responsive_breakpoints__layout/xterm__200x50__linux.txt
test/muse/snapshots/responsive_breakpoints__layout/xterm__80x24__linux.txt
test/muse/snapshots/sidebar__focused/xterm__100x30__linux.txt
test/muse/snapshots/themes__abyss#styled/xterm__100x30__linux.txt
test/muse/snapshots/themes__ember#styled/xterm__100x30__linux.txt
test/muse/snapshots/themes__light#styled/xterm__100x30__linux.txt
test/muse/snapshots/themes__storm#styled/xterm__100x30__linux.txt
```

The two audited baselines without a matching hit are
`test/muse/snapshots/chrome_regions__chrome/xterm__40x12__linux.txt` and
`test/muse/snapshots/responsive_breakpoints__layout/xterm__40x12__linux.txt`;
do not modify them unless a reviewed generated diff proves a changed label.

## Required verification and ratchets

Every implementation chunk updates its relevant ratchet in the same commit.
The config chunk covers `test/env-overlay-ratchet.txt`, the capability surface
gap ratchet if alias rows require mirrored gaps, config-example/HM drift, and
the control-schema snapshot test. The host chunk covers CLI help-group drift,
keymap round trips, and smoke checks. The docs chunk updates only shrink-only
help ratchets after real coverage is established; `test/help-context-ratchet.txt`
must remain empty. Generated keybindings/config-reference pages are validated,
not hand-written.

Scoped verification is specified in each chunk as `just quick <crate>` plus
targeted `cargo nextest run -p <crate> <filter>`. No coder should run `just
test`, `just ci`, a full-workspace compile, e2e, a migration, or a live-state
binary invocation for this design lane.
