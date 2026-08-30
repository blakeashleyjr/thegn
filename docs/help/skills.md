---
id: skills
title: Agent skills
order: 31
---

# Agent skills

thegn ships agent-readable recipes for its workflows and can add your own
packages from `[skills].user_dirs`. The registry is local: shipped documents
are compiled into the binary, configured packages are read from disk, and no
network or database is needed. Inspect it with `thegn skills list`, or print
the canonical document for one entry with `thegn skills show <name>`.

Skills are **prose**. thegn validates, lists, and copies `SKILL.md`; it never
sources or executes a skill. A harness may offer the prose to an agent, which
then decides how to follow it under that harness's normal permission model.

## Seeding a worktree

`thegn skills seed [--worktree <path>]` writes eligible packages into the
configured harnesses' native project directories: `.claude/skills/`,
`.agents/skills/`, and `.pi/skills/` for Claude, Codex, and Pi respectively.
It never writes a harness home directory. Omitting `--worktree` uses the shared
worktree resolution described by `thegn help`; this is a per-worktree seed,
not home synchronization.

Automatic seeding runs during worktree creation and startup reconciliation
when `[skills] enabled = true`. An explicit `skills seed` remains available
when automatic seeding is disabled. A skill is eligible only when all three
selectors agree:

- `harnesses` contains the target harness.
- `gate` is open: `always`, `merge_queue` (the queue is enabled), or `pipeline`
  (at least one pipeline stage is configured).
- `when` contains the current phase: `create`, `startup`, or `explicit`.

The shipped registry contains `mq`, `pipeline`, and `supervise`; their gates
remain `merge_queue`, `pipeline`, and `always`. Use `[skills].exclude` to
withhold a name from every harness.

## Writing a package

Each entry in `user_dirs` is a package root. thegn inspects only its immediate,
non-symlink child directories, each shaped as `<name>/SKILL.md`. For example,
`~/.config/thegn/skills/review-ready/SKILL.md` can contain:

```markdown
---
name: review-ready
description: Check whether the current worktree is ready for review.
harnesses: claude,codex,pi
gate: always
when: create,startup,explicit
---

# Review readiness

Inspect the worktree and explain any unfinished checks before asking to land it.
```

Frontmatter is deliberately flat and bounded: exactly the five keys shown,
within 32 lines and 8 KiB; the whole UTF-8 document is at most 256 KiB. The
description is one non-empty line of at most 1024 characters. Comma-separated
`harnesses` must name known harnesses, `gate` must use one value above, and
`when` must be a non-empty list of the three phases above. Unknown keys,
duplicates, malformed fences, and empty list items reject that package while
other packages continue to load.

`name` must match its package directory and be one path-safe segment of at most
128 bytes. Use only ASCII letters, digits, `.`, `-`, and `_`; separators,
control characters, `~` prefixes, and `..` are rejected before any destination
path is built. The markdown body after the closing fence is otherwise opaque
prose.

For a skill contributed to the shipped registry, put literal `thegn ...`
examples in fenced command blocks and keep placeholders visibly replaceable.
The bundled-skill command-drift test checks cited verbs and flags against the
live CLI definition, so renaming a command without updating its recipe fails
the targeted host tests and names the stale skill command.

## Updates and conflicts

Seeded files carry a thegn-managed marker, the binary version, and a SHA-256
content hash. That proof makes reseeding idempotent and permits safe upgrades.
thegn never overwrites or deletes an unmarked file. If you edit a managed file,
its hash changes and thegn treats it as adopted: it is preserved and reported
as modified. Exclusion and removal of retired registry entries likewise remove
only files whose marker and actual hash prove they are still unmodified.

Run `thegn doctor` to inspect each configured harness's project root and its
current, stale, modified, unmarked, or absent skill state. See
[[configuration]] for `enabled`, `user_dirs`, and `exclude`.
