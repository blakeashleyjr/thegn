# THE-32 reconcile — merge current main into the lane

## Files to touch (exact paths)

- `crates/thegn-core/src/config.rs`
- `crates/thegn-core/src/config_tests.rs`
- `crates/thegn-core/src/config_tests_coverage.rs`
- `crates/thegn-core/src/config_validate.rs`
- `crates/thegn-host/src/cmd/wt.rs`
- `crates/thegn-host/src/daemon/service.rs`
- `crates/thegn-host/src/diff_view.rs`
- `crates/thegn-host/src/handlers/tracker.rs`
- `crates/thegn-host/src/pr_view.rs`
- `crates/thegn-host/src/sidebar_view.rs`
- `crates/thegn-host/src/wizard.rs`
- `docs/help/merge-queue.md`
- `docs/help/workspaces-and-worktrees.md`

## State you are landing in

`git merge main` is ALREADY IN PROGRESS and left exactly those 13 files
conflicted — 34 hunks total. Do not abort or restart it. Resolve, then
`git commit` the merge.

The lane's own five new files conflict with nothing and must not be rewritten:
`thegn-core/src/{submodule,config_git}.rs`,
`thegn-host/src/{git_worktree,glyph_types}.rs`,
`thegn-svc/src/git/submodule.rs`.

**Do not apply a blanket "keep both sides" rule.** Nine of the 34 hunks are
additive collisions where keeping both is right. The other twenty-five are two
places where main _restructured_ code this lane also changed, and there
"keeping both sides" produces something that does not compile or silently drops
a main feature. Those two are specified below. Read them before you resolve
anything in `pr_view.rs`, `diff_view.rs`, `handlers/tracker.rs` or
`daemon/service.rs`.

## Group A — additive collisions (keep both sides)

`config.rs`, `config_tests.rs`, `config_tests_coverage.rs` and the first
`config_validate.rs` hunk are all "this lane added `git_submodules`, main added
`editor_provider`, on the same line". Keep both entries, in both the override
struct, the `set!` block, the `THEGN_*` env parser, and both test corpora.

`config_validate.rs` also holds a marked-enum comment ladder with a **pinned
count**. Main now ends at `// 97 → 98 (THE-60)` and pins `98`. Append this
lane's entry as the next free number and move the pin to match:

    // 98 → 99 (THE-32): `[git] submodules` (SubmoduleMode) — lifecycle
    // initialization policy, trusted config only.

then `defs.len() == 99`. Do not renumber main's existing ladder entries.
`config_validate::marked_definition_count_is_pinned` is the check — if it
reports a different number, that number is right and this instruction is
wrong; trust the test.

`sidebar_view.rs` is one test-function collision — keep both tests.

## Group B — main replaced the diff row model (`pr_view.rs`, `diff_view.rs`)

This is the hard one. Main moved the Files view off flat diff lines and onto a
**row** model that interleaves PR review feedback:

- `open_file_lines(i) -> Vec<&DiffLine>` is gone, replaced by
  `open_file_rows(i) -> Vec<ReviewRow>` built through
  `expanded_file_rows(file, anchored_review, include_resolved)`.
- New siblings: `anchored_review()`, `files_feedback_rows()`,
  `visible_threads()`. Row counts now add `files_feedback_rows().len()`.

This lane meanwhile added submodule semantics to those same functions: an
`open_file_lines` that filters `!f.is_submodule`, its own `file_row_count(i)`
helper, and an "submodule pointers are atomic" guard on the open path.

**Resolution: main's model wins as the structure; this lane's submodule
behaviour is ported onto it.** Concretely:

- Delete this lane's `file_row_count` and its `open_file_lines`. Do not
  reintroduce either name.
- Express the submodule rule inside main's row path: a file with
  `is_submodule` yields no expandable rows, so `open_file_rows` returns empty
  for it (or the single pointer row, matching how the lane rendered it).
- Keep the "submodule pointers are atomic" guard, rewritten against
  `open_file_rows`, so opening a submodule file still sets that status and
  returns `Pending` instead of expanding.
- Keep main's `files_feedback_rows()` contribution to every count you touch.
  A count that forgets it puts the PR feedback rows out of reach.

### Trap in `diff_view.rs` — do not drop main's colouring

In the file-list render, main emits two coloured segments per file:

    seg(Tok::Hue(Hue::Green), format!("+{adds} ")), seg(Tok::Hue(Hue::Red), ...)

This lane replaced that whole block with a single `seg(Tok::Slot(S::Dim), stats)`
so a submodule could show `<glyph> pointer`. Taking this lane's side wholesale
silently makes **every** file's +/- counts dim and uncoloured — a real
regression that no test will catch.

Resolve it as a branch, not a replacement: submodule files render the pointer
glyph, every other file keeps main's green/red `+adds -dels` segments. The
glyph must come from `crate::caps::active_glyphs().submodule` — never a literal
— because the glyph/colour chokepoint is an enforced ratchet.

## Group C — main added worktree lifecycle hooks (`handlers/tracker.rs`, `daemon/service.rs`)

Both sides changed the same worktree-creation call. Main wrapped creation in
lifecycle hooks:

- `worktree_lifecycle::run_event(..., HookEvent::PreCreate, ...)`, bailing when
  `pre.blocked()`,
- `wt::add_checked_with_state(...)` (not `add_checked`),
- failure text through `worktree_lifecycle::create_failure_with_add_state(...)`.

This lane routed creation through `crate::git_worktree::add_checked` and then
called `git_worktree::initialize(...)` to init submodules.

**Resolution: keep main's hook flow verbatim — the `PreCreate` event, the
`blocked()` bail, `add_checked_with_state`, and the failure helper — and add
this lane's submodule `initialize(...)` call after creation succeeds.** A
blocking `pre_create` must still leave git and the database untouched, so the
submodule init belongs strictly after the successful create, never before the
hook.

If `git_worktree::add_checked` exists only to wrap `wt::add_checked`, make it
delegate to `add_checked_with_state` so both doors keep the same state
handling. Do not leave two creation paths with different hook behaviour —
`daemon/service.rs` and `handlers/tracker.rs` must agree.

Submodule init failure stays **non-fatal** (warn, worktree stays registered),
which is what the lane's help text already promises.

## Group D — help docs (ratcheted; keep both, mind the rename)

- `docs/help/merge-queue.md`: this lane added the `{submodule_conflicts}`
  placeholder; main renamed `[workspace.<slug>]` to `[project.<slug>]` (THE-10).
  Keep **both** — the new placeholder AND main's `[project.<slug>]` spelling.
  This is the classic rename-meets-addition hunk; reverting the rename here is
  the most common way this merge goes wrong.
- `docs/help/workspaces-and-worktrees.md`: pure addition on both sides — keep
  this lane's submodule paragraphs and main's lifecycle-hooks paragraphs.

## On the workspace → project rename

THE-10's rename is on main. Verified: this lane's five new files contain zero
`workspace` mentions, so the exposure is confined to the edges above. Where
main renamed something, keep the rename; never leave the two spellings mixed.
`workspace` does survive deliberately as a compatibility alias in a few places
— check `git show main:<file>` before "fixing" one.

## Verification required before you report

Run the FULL suite:

    XDG_STATE_HOME=/home/blake/.superzej/pipeline-state RUSTC_WRAPPER= \
      THEGN_ALLOW_HEAVY=1 cargo nextest run --workspace --no-fail-fast

`--no-fail-fast` matters: fail-fast hides later failures and costs a round trip
each. Fix everything reported and re-run until green.

Then, because this merge touches ratcheted surfaces:

    just quick thegn-core && just quick thegn-host
    git diff --check
    treefmt --ci

The `XDG_STATE_HOME` prefix is REQUIRED on every command that may open thegn's
database, or a schema-ahead branch migrates the running instance's live DB.

Report honestly. A merge that compiles but drops main's PR-feedback rows or its
diff colouring is a FAIL, not a pass — say so and name what you could not
verify rather than reporting a green you did not get.
