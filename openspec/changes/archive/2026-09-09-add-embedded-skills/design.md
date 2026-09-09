# Design — embedded worktree skills

## Registry and trust boundary

The registry consists of immutable documents embedded in the binary plus
optional user-authored packages from configured directories. Thegn performs
no network access and never executes a document. Built-ins win name conflicts,
so an additional directory cannot replace shipped guidance invisibly.

Discovery is deliberately shallow and bounded: each configured directory may
contribute only immediate, regular, non-symlink `<package>/SKILL.md` files;
package names are validated before constructing paths; documents are capped at
256 KiB; and flat frontmatter is capped at 32 lines/8 KiB. Required fields are
`name`, `description`, `harnesses`, `gate`, and `when`.

## Selection and destinations

The host derives configured harnesses through the closed harness catalog. An
eligible document must name the harness, include the current seed phase
(`create`, `startup`, or `explicit`), pass its typed gate (`always`,
`merge_queue`, or `pipeline`), and not appear in `[skills].exclude`.

Destinations are project-local harness conventions under the selected
worktree: `.claude/skills`, `.agents/skills`, and `.pi/skills`. This keeps the
change scoped to the worktree and makes cleanup reviewable in Git. There is no
home-directory synchronization mode.

## Conservative reconciliation

Core renders canonical content and plans operations from an observed survey.
Each managed file carries a version/hash marker. A missing file is created; an
unchanged managed file may be upgraded; an unmarked, malformed, or modified
file is preserved and diagnosed. A retired file is deleted only when it is
still Thegn-managed and unchanged and discovery/survey was complete. This
fail-closed rule prevents partial I/O errors from being interpreted as absence.

## Lifecycle

With `[skills] enabled = true` (the default), existing worktree create and
startup seams call the same seeder used by `thegn skills seed`. Seeding is
idempotent and does not make prose executable. `list` and `show` are read-only;
`seed` reports deterministic per-file outcomes and diagnostics.
