# Design — submodule integration

## 1. What exists (audit anchors)

- Only real handling: `BranchOps::nuke_working_tree` runs `submodule foreach
--recursive 'git reset --hard && git clean -fdx'` when `.gitmodules` is
  tracked (`crates/thegn-svc/src/git/branch.rs:101`).
- Status: porcelain **v1** `-z --no-renames`, parsed by
  `parse_status_porcelain` into `FileStatus { staged, unstaged, path }` —
  two chars survive, no submodule detail (the v2 `S<c><m><u>` columns simply
  aren't in the input). `GixGit::is_dirty` early-exits on any entry; gix
  submodule status is never consulted.
- Diff: `--numstat` yields `-\t-` for gitlinks (badges read 0);
  `parse_unified_hunks` / `patch.rs::parse_patch` / `forge::model::
parse_unified_diff` all treat `Subproject commit` lines as text;
  `FileKind` is `Modified|Added|Deleted|Binary|ModeOnly`.
- Lifecycle: `worktree::add` = `git worktree add --quiet -b …` only;
  `workspace_create.rs` = `git clone --progress` only; `remote.rs::
provision_repo_script` = `git clone <origin> .` only.
- Fold: object-DB `merge-tree` passes gitlinks through as opaque tree
  entries; a both-moved pointer surfaces as a generic conflicted path.
- Explorer: the vendored `git.yazi` Lua plugin has no `.gitmodules`
  awareness.

## 2. Reads: keep porcelain v1, add a targeted submodule read

Migrating the status parse to porcelain v2 would buy the `S<c><m><u>` columns
but rewrites `parse_status_porcelain`, `FileStatus`, `is_conflict` (which
pattern-matches the char pair), `conflict_count`, the bridge batch's
`is_dirty` shortcut, and `build_change_rows` in one motion. Not worth the
blast radius for one bit. Instead:

- A cached `.gitmodules` path set per worktree (mtime-invalidated, parsed
  with a tiny pure parser in core — unit-testable, no git subprocess). Any
  status/diff row whose path is in the set is classified `submodule`.
- A new defaulted `GitBackend` read, `submodule_states(loc) -> Vec<SubState
{ path, head_moved, dirty, untracked }>` (CLI: one `git submodule status
--recursive` + the existing status rows; gix later if its submodule API
  earns it), batched into `glyph_reads` so the hot path stays one bridge
  round-trip. It runs only when the `.gitmodules` set is non-empty — repos
  without submodules pay nothing.
- `GlyphReads` gains `submodule_dirty: Result<bool>` (independently
  degradable like every other field); `GlyphRow` grows its 9th slot.
  **Trap, planned for:** `GlyphRow` is a positional tuple — widening moves
  `glyphs_from_row`, `merge_glyph_scan` + its five `glyph_scan_*` tests,
  warmcache serde, and the DB `glyph_cache` JSON together; missing one
  silently zeroes the badge. Freshness follows the existing model: active
  worktree rescans per tick, background rows are TTL-cached, so the badge
  inherits that staleness (documented, not fought).

## 3. Patch engine: gitlinks are atomic

`FileKind::Submodule` is recognized from `index … 160000` / `new mode
160000` / `Subproject commit` shapes in `parse_patch` and
`parse_unified_diff`. Consequences, enforced in core with unit tests:

- `render_patch` round-trips a gitlink file byte-identically (the existing
  contract, extended by fixture).
- A `Selection` that would split a submodule hunk is rejected at the pure
  layer — staging surfaces offer whole-entry stage/unstage only (`git add
<path>` / `git restore --staged <path>`, never `git apply` with a partial
  gitlink hunk).

## 4. Rendering a pointer move

The changes/diff surfaces render a submodule row as `⊂ <path>  abc1234 →
def5678` (glyph via `caps::active_glyphs()`), and the drilled view shows a
bounded commit summary: `git -C <sub> log --oneline --no-decorate
old..new` capped at N lines, fetched off-loop, degrading to the bare SHAs
when the submodule checkout is missing or the range is unknown locally
(common right after a remote-side bump — never fetch to answer a render).
Direction is labelled (`forward`, `rewind`, or `diverged` via a local
`merge-base --is-ancestor` check, again best-effort).

## 5. Lifecycle: populated checkouts, gated by trust

`[git] submodules = "auto" | "off"`, default `auto` = act only when
`.gitmodules` exists at the checkout root:

- **Worktree create** (core `worktree::add` pipeline, shared by the wizard
  and `wt new`): after `worktree add` succeeds, run `git submodule update
--init --recursive` in the new worktree — off the event loop, progress in
  status, **non-fatal**: failure leaves a valid worktree with a visible
  "submodules not initialized" notice rather than rolling back. Worktrees
  share the superproject's `.git/modules`, so the common case is
  network-free.
- **Workspace clone** gains `--recurse-submodules`; the **remote provision
  script** appends `git submodule update --init --recursive` after its
  clone.
- **Trust gate:** submodule URLs and paths are repo-controlled input, and
  `update --init` clones and checks out from them. In a repo whose trust
  class does not already permit repo-driven execution, initialization
  requires the same consent flow `add-config-trust-resolution` defines for
  other repo-supplied config — a prompt naming the submodule URLs, TOFU-
  remembered. thegn never sets `protocol.file.allow` in production
  invocations (git's own default hardening stands).

## 6. Fold behaviour

A gitlink conflict (both sides moved the pointer) is reported as
`submodule pointer conflict: <path> (<ours> vs <theirs>)` in the drain
outcome and the agent-handoff prompt variables — and is **excluded** from
the driver/rerere routing added by `add-scm-workflow-customization`:
auto-picking a submodule commit is semantically dangerous (it silently
selects someone's dependency state), so pointer conflicts always defer to
the agent prompt (which states the rule: pick by understanding, or defer)
or the human.

## 7. Event loop, rendering, schema, help (config.yaml checklist)

- **Wake path:** all new reads (`submodule status`, log summaries,
  `.gitmodules` mtime checks) run off-loop on the existing hydrate/refresh
  workers, delivered over channels with a `TerminalWaker` pulse; `submodule
update --init` after worktree create runs on the same background pattern
  as post-create provisioning. Nothing polls; repos without `.gitmodules`
  add zero work.
- **Damage channels:** badges and change rows are chrome ⇒ `Full` on
  change, like every glyph update today. No pane interaction;
  `render_plan` tests untouched.
- **SQLite:** no schema change; the widened glyph row rides the existing
  `glyph_cache` JSON additively (older rows deserialize with the field
  defaulted).
- **Help:** no new action ids or keybinds are planned (stage/unstage keys
  are unchanged; atomicity is behaviour); the `[ui]` toggle and `[git]
submodules` key appear via the generated config-reference page. If drill
  navigation grows a key during implementation, it claims a help page in
  the same commit (ratchet).

## 8. Security

- **New execution surface:** `git submodule update --init --recursive`
  executes clones/checkouts from repo-controlled URLs — the reason for the
  §5 trust gate. No shell interpolation: all invocations go through the
  `git_cmd`/`GitLoc` builders with argv arrays and the scrubbed git env.
  thegn never enables `protocol.file.allow` or URL rewrites outside tests.
- **Log summaries** run `git -C <submodule-path> log` where the path comes
  from the superproject's tree — constrained to descend from the checkout
  root (reject `..`/absolute paths from a hostile `.gitmodules`).
- **No credentials touched:** submodule fetch auth rides the ambient
  identity exactly like every other git network op (identity selection is
  `add-decoupled-identities`; credential material is out of scope here).
- **Blast radius:** the only new writes are into freshly created checkouts
  (worktree/clone provisioning) — never into the object DB or refs; fold
  behaviour for gitlinks only gains _reporting_, never auto-resolution.
- Config keys are trusted-layer only; the untrusted `.thegn.*` overlay
  cannot flip `[git] submodules`.

## 9. Deferred / open questions

1. **File-explorer boundary markers** — the tree is the vendored `git.yazi`
   Lua plugin; teaching it `.gitmodules` means patching vendored code.
   Deferred until the drawer's ownership story is revisited; noted, not
   spec'd.
2. **Porcelain v2 migration** — revisit only if a second consumer needs
   v2-only data; the targeted read covers this change.
3. **Recursive nesting depth** — `--recursive` all the way down vs a depth
   cap; start uncapped (git's own default), revisit if provisioning cost
   shows up in the startup waterfall.
4. **gix submodule reads** — gix has submodule APIs; not adopted here (CLI
   read is one process per scan and correct). Candidate for the native
   engine later; the seam method makes that swap invisible.
