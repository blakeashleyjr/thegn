---
name: thegn
description: How to work inside a thegn worktree — find out where you are, run builds and tests on the project's remote runner, land or queue your branch, ask the human for attention, spawn helper agents, and stay inside the sandbox's limits. Use at the start of a task in a thegn worktree, before running a build or test, when the work is finished, or when you are blocked and need the human.
harnesses: claude,codex,pi
gate: always
when: create,startup,explicit
---

# Working inside thegn

You are running in a **thegn** worktree: a git worktree on its own branch,
shown as one row in the human's sidebar. The `thegn` CLI (alias `tg`) is on
your `PATH`. Every command below acts on the **current** worktree when run
from inside it. Add `--json` where a command offers it when you need to parse
the result.

## 1. Find out where you are

```bash
thegn info
thegn env show
```

`info` prints what is in effect here and why: the workspace, the worktree, its
branch, its environment and the settings that apply. `env show` names the
execution environment (local, a container, or a remote Sprites/Vercel
sandbox). Read both before you assume anything about the machine you are on.

```bash
thegn wt list
```

lists the other worktrees of this repository. Do **not** edit them; each one
belongs to another task.

## 2. Builds and tests

```bash
thegn job route
```

says where this project's `cargo build` and `cargo test` run. When it reports
the automatic route (`route = "auto"` with a runner), a bare `cargo build` or
`cargo test` at the worktree root is sent to the project's remote runner for
you; flags such as `--release` or `-p` run locally with a notice. To check a
line before you run it:

```bash
thegn job match "cargo build && cargo test"
```

To run a recipe on the runner explicitly and get its exit code and receipt:

```bash
thegn job run cargo-test
```

If a remote job fails with `job not started`, nothing ran and a retry is safe.
`THEGN_JOB_LOCAL=1 cargo build` forces one command to run locally.

## 3. Finishing: land or queue

When the work is committed and the human has asked you to integrate it:

```bash
thegn land
```

folds this branch onto the target branch, runs the gate, and advances the
target only when the gate passes. Never `git checkout main && git merge`.

When the repository uses the merge queue, queue the branch instead and let the
queue fold it:

```bash
thegn merge add
thegn merge list
```

Do not land or queue unless the human asked for it; committing on your branch
is always fine.

## 4. Asking for the human

The sidebar shows whether you are working, waiting, or done. When you are
blocked on a decision, raise a notification so the human sees it even when
your pane is in the background:

```bash
thegn notify push --kind agent_attention "Need a decision on the schema migration"
```

Use `--kind agent_done` when a long task finishes and `--kind agent_failed`
when it cannot be finished. Keep the message to one line that says what you
need.

## 5. Helper agents

To hand a self-contained subtask to another agent in **another** worktree:

```bash
thegn agent list
thegn agent run --agent claude --worktree /path/to/other/worktree --prompt "Fix the flaky test" --wait
```

`agent list` shows which agents and models are configured. `agent run --wait`
blocks until the helper exits and prints the tail of its output. Create the
helper's worktree first with `thegn wt new <name>` (it prints the new path).

## 6. Sandbox limits

Interactive panes usually run in a sandbox. Under the `standard` profile you
can write only inside this worktree, well-known credential files (SSH keys,
cloud logins, the `gh` token) are hidden, sibling repositories are hidden, and
agent settings are read-only. The `sealed` profile is stricter and may block
the network. If a write outside the worktree or a network call fails, that is
the sandbox, not a bug: say so to the human instead of working around it.

## 7. Reading thegn's own documentation

thegn's MCP server (`thegn mcp serve`, registered as `thegn-serve` when the
human ran `thegn mcp wire`) lets you search the help pages, read the current
configuration with secrets masked, and list sessions, worktrees and the merge
queue. Prefer it to guessing how a feature works.
