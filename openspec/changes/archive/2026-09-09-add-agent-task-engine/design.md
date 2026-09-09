# Design — shared agent-task engine

## Invariants this change does NOT touch

- **Render damage channels:** none. Nothing here renders. The driver already runs
  off-loop (CLI, or `spawn_drive`'s `spawn_blocking`), and its progress channel
  (`DriveMsg` → waker pulse → `Panes`/`Full`) is unchanged.
- **Event-loop wake paths:** none added. No new ticker, no new channel.
- **SQLite:** no schema change, no `user_version` bump.
- **Help:** no new action id, keybind, zone, or panel section, so
  `test/help-ratchet.txt` and `test/help-context-ratchet.txt` are untouched. The
  config-reference help page is generated from `config.toml.example` at runtime
  (`thegn_core::help::config_ref`), so documenting the new keys there is
  sufficient — and is enforced by `tests/config_example.rs`.

## Two-layer substitution (the subtle part)

There are **two** templates in play, and conflating them is the existing bug.

```
                vars {branch, target, worktree, paths, log, …}
                                  │
        [pr]  prompt_template  ───┤  render_prompt()   → NO shell quoting
                                  ▼
                            prompt: String              (multi-line, quotes, $ …)
                                  │
        [cmd] command_template ───┤  substitute_command() → shell-quotes EACH var
                                  ▼
                       "claude -p 'You are resolving…' --permission-mode acceptEdits"
```

- `render_prompt` produces **prose**. It must not quote, escape, or shell-mangle
  anything; the result is human-readable text that also goes into the child's
  `THEGN_TASK_PROMPT` env var verbatim.
- `substitute_command` produces a **shell command line**. Every substituted value
  passes through `util::sh_quote`, so the template must use **bare** placeholders.

`config.toml.example:2800` violates this by writing `-p "{prompt}"`. Because
`sh_quote` wraps a multi-word string in `'…'` (`util.rs:919`), the shell sees
`"'…'"` and the agent gets a prompt with literal leading/trailing apostrophes.

**Fix:** correct the example, and have `validate_template` reject a command
template whose placeholder sits inside a quote run — a lint, reported by
`thegn config validate`, rather than a silent misfire at 2am mid-drain.

## Template syntax: `{var}`, not `{{.Var}}`

The repo has two precedents: `agent_command`'s `{prompt}` and `[[git_commands]]`'s
lazygit-style `{{ .Selected.Sha | quote }}` (`custom_cmd.rs:71`). This change uses
`{var}` because `agent_command` is an **existing shipped key** whose `{prompt}` /
`{branch}` / `{target}` placeholders must keep working. Introducing a second
syntax on the same key would break every current config.

Borrowed from `custom_cmd.rs`: an **unknown placeholder is a hard error**
(`TemplateError::UnknownVar`), not a silent empty expansion. A typo like
`{branchh}` should fail loudly at `config validate` time, not hand the agent a
subtly truncated prompt.

Literal braces escape as `{{` / `}}`.

## Agent resolution precedence

```
1. agent_command non-empty  →  use it verbatim as the command template.
2. agent non-empty          →  look up the [[agents]]/[[tools]] entry by name;
                               take its `provider` (or infer from the command's
                               program basename, via the existing crate::account
                               inference) and look up headless flags.
3. neither                  →  no agent; handoff degrades to notify (today's
                               behavior when agent_command is empty).
```

`agent_command` wins so nothing about existing configs changes, and so **any**
agent remains usable even if thegn has never heard of it.

### The provider → headless-flags table

```rust
"claude" => "claude -p {prompt} --permission-mode acceptEdits"
"codex"  => "codex exec {prompt}"
"aider"  => "aider --yes --message {prompt}"
_        => "<command> {prompt}"          // + a config_warn
```

Pure data, unit-tested. The fallback is deliberate: an unknown agent still gets
the prompt as an argument, which is the common CLI convention, and the warning
tells the user to set `agent_command` if that guess is wrong. This is what makes
"works with any agent" true rather than aspirational — the table is a
convenience, never a gate.

## Byte-identical defaults

`default_prompt(TaskKind::MergeConflict)` and `default_prompt(TaskKind::GateFailure)`
reproduce today's `build_prompt` output exactly, as templates:

```
You are resolving a merge-queue blocker for the git branch `{branch}`, which
must land onto `{target}`. …
```

A unit test renders each default against the same vars the old code would have
used and asserts equality with the literal string from the pre-change
`build_prompt`. That test is the proof that this refactor is behavior-preserving,
and it is why the merge queue's existing real-git integration tests
(`agent_resolves_conflict_and_branch_lands`, `agent_that_cannot_fix_marks_needs_human`)
must pass **unmodified**.

## Env contract

`agent_run` exports, for every task kind:

| var                 | value                                 |
| ------------------- | ------------------------------------- |
| `THEGN_TASK_KIND`   | `merge_conflict` / `gate_failure` / … |
| `THEGN_TASK_PROMPT` | the rendered prompt                   |
| `THEGN_WORKTREE`    | cwd (unchanged)                       |
| `THEGN_BRANCH`      | branch (unchanged)                    |

Plus, **only** for the two merge kinds, the legacy aliases `THEGN_MERGE_PROMPT`
and `THEGN_MERGE_TARGET`. They are shipped surface someone may script against;
they cost two lines and are documented as deprecated.

`util::GIT_ENV_VARS` scrubbing, the `spawn_grouped` process group, the watchdog
thread, and the 1 MiB-capped pipe drains all move across verbatim — this is a
lift, not a rewrite.

## Why the exit code still doesn't decide anything

`drive_queue` ignores the agent's exit status today and re-attempts the fold to
decide (`merge_driver.rs:262-264`). That stays. An agent that exits non-zero
after committing a good fix should still land, and an agent that exits zero
having done nothing must not. The re-attempt is the arbiter; keeping that
unchanged is part of "no decision changes".
