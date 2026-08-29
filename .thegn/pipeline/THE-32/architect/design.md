# THE-32 — deeper submodule integration in git views

Status: architecture for implementation on `tg/the-32-submodules`.

## Decision

Treat a submodule as a gitlink owned by the superproject, not as an ordinary
directory or text hunk. Thegn will have one pure, substrate-free submodule
domain model in `thegn-core`; `thegn-svc::git::GitBackend` will be the only new
git-read/write seam; and host workers will adapt that model to the panel,
sidebar, worktree creation, and measurement lanes.

The feature is additive at the existing surfaces: no new CLI verb, action,
capability-catalog row, SQLite table, or fetch-on-render behavior. A missing
submodule, unavailable object, failed provider, or denied trust request
degrades to the last-known state or a clear SHA/status notice. It never blocks
the event loop and never fabricates a clean result.

This follows the repository invariants in `CLAUDE.md:39-58` and
`docs/ARCHITECTURE.md:199-214`: 0% idle, off-loop work through a channel and
waker, core without substrate dependencies, provider seams rather than vendor
calls, one capability catalog, and config/schema/help ratchets in the same
change.

## Verified baseline and draft pruning

The existing OpenSpec is useful as a behavior checklist, but it is not the
implementation map. The following claims were re-checked against this branch.

| Draft claim                                                               | Branch evidence                                                                                                                                                                                                                                                                                    | Decision                                                                                                                                                                                                |
| ------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Status/diff have no useful seam or batch.                                 | `GitBackend` already exposes independent reads and `glyph_reads` at `crates/thegn-svc/src/git/mod.rs:236-263`; the bridge batches status, branch, ahead/behind, and diff at `:897-1043`.                                                                                                           | Preserve and extend the one batch. Do not add a subprocess per badge or migrate all status to porcelain v2.                                                                                             |
| `GlyphRow` can simply gain a ninth tuple slot and old cache rows default. | It is a positional tuple at `crates/thegn-host/src/hydrate.rs:25-42`; `warmcache.rs:18-29` directly deserializes it, while `glyph_refresh.rs:18-32` indexes it. Missing tuple elements do not automatically become a default.                                                                      | Prune the ninth-slot prescription. Introduce a named/version-tolerant cache record in a sibling host module that reads legacy eight-element rows as `submodule_dirty=false`, and writes the new record. |
| `patch.rs` treats gitlinks as text.                                       | `FileKind` has no submodule variant at `crates/thegn-core/src/patch.rs:57-81`; header/body parsing is at `:161-240`.                                                                                                                                                                               | Implement the proposed atomic gitlink model and fixture tests.                                                                                                                                          |
| Worktree creation is only `git worktree add`.                             | `cmd/wt.rs:246-290` and `core/worktree.rs:215-257` create/register without initialization; the TUI worker is already off-loop in `wizard.rs:1-19`.                                                                                                                                                 | Keep the worker/waker design, but put new git operations behind the host/service seam. Do not add submodule subprocesses to `thegn_core::worktree`.                                                     |
| Clone/provision are synchronous gaps.                                     | Local clone is off-loop in `crates/thegn-host/src/workspace_create.rs:56-93`; provider provisioning is an existing remote script boundary at `crates/thegn-core/src/remote.rs:678-698` and host execution at `crates/thegn-host/src/agent.rs:2649-2669`.                                           | Extend each existing worker/provider path, preserving progress and non-fatal failure. The core function remains pure command-data serialization; it must not execute git.                               |
| Measurement needs a new scheduler.                                        | Disk walks the whole tree at `crates/thegn-core/src/disk.rs:67-95`; nested registered-worktree subtraction is already at `:170-195`. LOC’s whole-tree tokei call is `crates/thegn-host/src/loc_scan.rs:18-41`; scheduling/cache/waker are already in `crates/thegn-host/src/measure/mod.rs:11-24`. | Change accounting boundaries only. Retain the existing off-loop planner, cache, and waker.                                                                                                              |
| Existing trust handling covers submodule URLs.                            | Repo trust currently gates repo-authored sandbox overlays (`crates/thegn-host/src/handlers/repo_trust.rs:1-10,42-67`), not network/file URLs.                                                                                                                                                      | Reuse the approval vocabulary/store but add a distinct submodule URL request; deny initialization until approved. Never silently turn on `protocol.file.allow`.                                         |
| Merge conflicts can stay generic.                                         | `MergeTreeOutcome` carries only paths (`crates/thegn-svc/src/git/plumbing.rs:21-28,65-100`), and `integrate.rs:127-240` routes generic paths through drivers/rerere.                                                                                                                               | Add pure gitlink conflict metadata and partition these paths before driver/rerere.                                                                                                                      |
| Explorer/vendor integration belongs here.                                 | The draft itself defers the vendored yazi plugin (`openspec/changes/add-submodule-integration/design.md:148-161`).                                                                                                                                                                                 | Keep it deferred. Git views and creation/scans are the issue scope; do not patch vendored UI code.                                                                                                      |

Already satisfied and therefore not to be reimplemented: the service read
seam, bridged glyph batch, off-loop hydration/cache, off-loop measurement
lanes, local clone worker, and the existing `Config::repo_git` trusted
workspace overlay (`crates/thegn-core/src/config.rs:2001-2157,6634-6648`).
The OpenSpec draft’s “only one historical call site” framing
(`proposal.md:7-30`) is stale in detail, although it correctly identifies the
missing user-visible behavior.

## Core contract

Add a sibling `thegn_core::submodule` module, exported from `lib.rs`. It owns
only data and pure functions:

- `SubmoduleSpec { name, path, url }` from a strict `.gitmodules` fixture
  parser. Reject duplicate paths, absolute paths, `..` escapes, empty paths,
  and malformed records. Keep URL text for trust display, but do not resolve or
  execute it in core.
- `SubmoduleState { path, recorded_sha, checked_out_sha, initialized, dirty,
untracked, pointer }`, where `pointer` distinguishes clean, moved, rewind,
  diverged, conflict, and unknown. Parse fixture strings for `git submodule
status --recursive` and the status evidence supplied by the seam; do not
  make the parser depend on a git executable.
- `SubmoduleDiff { path, old_sha, new_sha, kind }` and
  `SubmoduleSummary { direction, commits, truncated, unavailable }`. Direction
  is computed from explicit local ancestor facts, never guessed from SHA order.
  The summary has a bounded commit count and represents “objects unavailable”
  separately from an empty range.
- `SubmoduleRowPolicy`, a pure presentation decision used by all git views:
  pointer rows show the submodule glyph, path, abbreviated old/new SHA, and
  state labels; they do not show `+0/-0`. A drilled row may show a bounded
  commit list, otherwise it shows the two SHAs and why the range is unavailable.
- `is_submodule_descendant` / boundary helpers used by measurement tests. A
  path comparison is component-aware and cannot mistake `lib` for `library`.
- `SubmoduleConflict { path, ours_sha, theirs_sha }` and a pure formatter for
  `submodule pointer conflict: <path> (<ours> vs <theirs>)`. Extend the pure
  fold/prompt context (`crates/thegn-core/src/fold.rs` and
  `crates/thegn-core/src/agent_task.rs`) only enough to carry this typed
  detail; retain raw paths for git operations.

Extend `patch.rs` with `FileKind::Submodule` for mode `160000`, new/deleted
gitlink modes, and `Subproject commit` bodies. `render_patch` must round-trip
fixture strings. `Selection` must reject line-level selection for a gitlink;
whole-entry stage/unstage remains the existing `StageFile` operation. Extend
`forge/model.rs`’s unified-diff parser with the same marker so PR diffs do not
turn a gitlink into selectable text. These are core unit tests over fixture
strings, not repository tests or subprocess tests.

Add `SubmoduleMode::{Auto,Off}` in a small config sibling and wire it through
the exhaustive `GitConfig`/`GitOverlay`/`ConfigOverlay` paths in
`crates/thegn-core/src/config.rs`. `auto` means “initialize only when a
root-level `.gitmodules` exists”; `off` means do not initialize, clone
recursively, or query submodule state beyond cheap classification. Add the
`THEGN_GIT_SUBMODULES` env overlay. Add `[ui] sidebar_show_submodules = true`
beside the existing sidebar toggles in `config_ui.rs`. Both keys are trusted
config, not repo-local `.thegn.*` knobs.

Add one width-safe `GlyphSet` entry (Unicode and ASCII fallback) in
`thegn-core/src/termcaps.rs`. Draw sites use `caps::active_glyphs()`; no new
literal goes into panel/sidebar code and no glyph-literal debt is added.

## Git seam and read flow

Add a sibling `crates/thegn-svc/src/git/submodule.rs` for parsing/command
assembly and extend `GitBackend` with defaulted, independently failing
operations:

- `submodule_states(loc)` — one recursive status read plus the existing status
  evidence, skipped when the root has no `.gitmodules` or mode is `off`.
- `submodule_diffs(loc, base)` — raw gitlink old/new object IDs, not numstat.
- `submodule_summary(loc, path, old, new, limit)` — bounded local log and
  ancestor probes, never fetches.
- `init_submodules(loc, recursive)` — the write used after creation/clone,
  through the same argv/bridge/scrubbed-environment seam as other git writes.
- `submodule_conflicts(loc, paths)` — resolves mode/object metadata for already
  reported merge paths.

The CLI implementation is the initial provider. `GixGit` delegates to the
bounded CLI fallback until a native implementation is justified; it must not
silently invent a second provider. Existing bridge reads add the submodule
commands to the single `exec_batch`, and local reads use the existing bounded
CLI path. Every result is independently degradable. No `Command`, `git_out`,
or vendor call is added to core; all new git calls are in the service seam or
host provider implementation files.

The active/background glyph path adds `submodule_dirty` to the read result and
derives it from pointer, dirty, untracked, and uninitialized state. Keep the
current active cadence and background TTL. Replace the positional persisted
tuple with a named cache record in `crates/thegn-host/src/glyph_types.rs`, with
a custom legacy-array deserializer and `#[serde(default)]` for the new field.
Update `hydrate.rs`, `glyph_refresh.rs`, and `warmcache.rs` together. A failed
submodule read reuses only the last-known field; it must not turn the entire
row clean. The existing channel publication and `TerminalWaker` pulse remain
the wake path.

Change-row assembly joins `submodule_diffs` by path in the host model. A
gitlink’s missing numstat is `None`, never numeric zero. The ordinary row path
continues to render normal file additions/deletions.

## Rendering and staging

`ChangeRow` gains a typed optional submodule payload, not a stringly encoded
status. The changes section renders a policy-produced row such as:

```
⊂ vendor/lib  abc1234 → def5678  (forward, 3 commits)
```

The glyph comes from the capability table. Dirty, uninitialized, diverged,
and unavailable states are short labels. Drill/preview work is a host worker
that calls `GitBackend::submodule_summary`; it sends a typed preview result and
pulses the waker. It is capped, local-object-only, and degrades to SHA text.
No render function invokes git.

Stage/unstage on a gitlink routes to whole-file `StageFile`. The line-selection
path asks the core selection validator and displays “submodule pointers are
atomic” without queuing `git apply`. Add fixture tests for added, deleted,
cleanly moved, and dirty gitlinks.

The sidebar adds a separate submodule indicator, controlled by
`sidebar_show_submodules`, independent of the ordinary dirty dot. It is added
to `GitGlyphs`, cache hydration, row layout, and the existing pure display
model. It does not add an action or keybinding.

## Creation, clone, trust, and remote materialization

Create `crates/thegn-host/src/git_worktree.rs` as the shared host-side pipeline:
path preparation and rollback stay host/core policy, while the actual git
operations use `GitBackend`. Route the existing `wt new`, TUI wizard, daemon
service, and tracker creation call sites through it so no caller gets a
different submodule policy. The core worktree module remains for branch/path
policy; do not extend it with a new subprocess.

After a successful worktree add, if effective trusted config is `auto` and
`.gitmodules` is present, run recursive init on the worker lane. It is
non-fatal: the worktree remains registered and the user receives a progress or
failure notice. `wt new` keeps stdout exactly the created path and sends
progress/errors to stderr. The TUI uses the existing worker event channel and
waker; it may add a progress step in a new sibling event module rather than
growing the wizard state machine without need.

The same policy applies to local workspace clone (`--recurse-submodules` or
the seam equivalent), provider repo provisioning, and bundle/remote
materialization. Remote command text remains a pure, shell-quoted data product
at the existing provider boundary; execution stays in the host/provider
runner. Do not add shell execution to `thegn_core::remote`.

Before initialization, collect normalized URL/path pairs and ask the existing
repo-trust mechanism for a distinct submodule approval. A denied or unavailable
approval leaves the worktree usable and reports “submodules not initialized”.
Do not implement an ad-hoc prompt, persist URL secrets, interpolate URLs into
shell, or enable `protocol.file.allow` outside fixture setup. Tests that create
local submodules may use `-c protocol.file.allow=always` and
`-c commit.gpgsign=false` only in the test fixture, with a temporary
`XDG_STATE_HOME`.

## Measurement/accounting policy

Keep the existing scheduled background scans, single-round guard, cache, and
waker. Add a pure boundary fixture test and host adapter tests:

- Disk size is physical apparent bytes of the superproject tree, including a
  populated submodule exactly once. A submodule directory is not a registered
  worktree target and is never added as a second disk-cache entry. Existing
  `net_root_bytes` subtracts registered nested worktrees only; do not subtract
  submodule bytes from the parent and then add a synthetic child.
- LOC is superproject source LOC. Exclude each normalized submodule directory
  from the tokei input/boundary walk, including recursive descendants, so a
  vendored repository’s source is not counted as the parent’s LOC. The
  submodule’s own worktree, when independently registered, may have its own
  measurement.
- Git status/state reads summarize the gitlink and never descend into every
  submodule file for the parent change list. Dirty/untracked state comes from
  the targeted seam read and remains independently degradable.

Document this distinction in the measurement/help prose: disk answers “bytes
laid out here”, LOC answers “source owned by this worktree”, and neither
creates a second worktree row for a gitlink. This closes the double-count trap
described by the current disk comments (`crates/thegn-core/src/disk.rs:170-195`)
without changing scan cadence.

## Merge, PR, and conflict behavior

The forge parser’s `FileKind::Submodule` marker keeps PR diffs readable and
non-selectable. For merge-tree conflicts, obtain the two mode-160000 object IDs
through the service plumbing seam, classify them in core, and carry typed
details alongside raw conflict paths.

In `integrate.rs`, partition gitlink paths before regenerable classification,
custom-driver detection, and rerere. Never send a submodule path through the
throwaway real merge or `git add -A` auto-resolution path. Defer it with the
exact pointer-conflict message, including ours/theirs SHAs. The merge/fold
prompt variables receive structured details rendered by the pure core helper;
ordinary text conflicts retain current behavior. No pointer is auto-picked.

## Ratchets, tests, and delivery

No new capability/action/completion slot is needed. The implementation chunk
must verify the control-schema and completion snapshots remain unchanged. The
same commit that introduces keys/UI must update or regenerate the applicable
env-overlay and help/prose/context ratchets; it must not add glyph-literal,
ignored-result, or platform debt. The docs must cover `[git] submodules`,
`[ui] sidebar_show_submodules`, pointer rendering/atomic staging, lifecycle,
trust, and disk-vs-LOC accounting in the relevant `docs/help/` pages and the
example config.

Use only scoped checks while implementing:

- `just quick thegn-core`; `cargo nextest run -p thegn-core submodule`, plus
  `patch`/`fold`/`config` filters as touched.
- `just quick thegn-svc`; `cargo nextest run -p thegn-svc submodule` and
  `plumbing` filters.
- `just quick thegn-host`; `cargo nextest run -p thegn-host glyph_scan`,
  `changes`, `sidebar`, `measure`, `integrate`, and `worktree` filters as
  touched.

Do not run `just test`, `just ci`, a full-workspace compile, e2e, a migration,
or the built binary. Any `thegn` invocation must set `XDG_STATE_HOME` to a
fresh temporary directory. Fixture tests must be hermetic and must not use the
live state DB.

Implementation is split into the three file-disjoint coder chunks below.
Dependencies are explicit so the Lead can run them serially without merging
overlapping edits. Each coder commits early using the exact subject in their
chunk; the architect commit is separate and exact: `docs(the-32): architect
design + chunk specs`.
