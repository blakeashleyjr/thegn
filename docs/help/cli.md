---
id: cli
title: The thegn CLI
order: 16
actions: [integrate, merge-drain]
---

# The thegn CLI

The TUI is the product; the CLI is the launcher, the remote control, and
the automation surface. Everything here works from any shell — including
a pane inside thegn itself.

`thegn` with no arguments launches the compositor. The short alias `tg`
works everywhere.

## The grammar

Noun-verb namespaces mirror the domain model (repo → workspace →
worktree):

| Group         | Commands                                                                                                       |
| ------------- | -------------------------------------------------------------------------------------------------------------- |
| Workspace     | `wt list/new/rm/diff/disk/clean` · `repo list/recent` · `open <repo>` · `map` · `land` · `integrate` · `merge` |
| Forge         | `pr` · `issue` · `ci` · `kaneo`                                                                                |
| Environments  | `env` · `zone` · `host` · `placement` · `debug` · `mcp` · `plugin`                                             |
| Session       | `notify` · `logs` · `share` · `forward`                                                                        |
| Control plane | `serve` · `session` · `attach` · `pair` · `api`                                                                |
| Meta          | `config` · `keys` · `theme` · `doctor` · `setup` · `completions`                                               |

Global flags everywhere: `--config`, `--log-level`, `--set key=value`
(repeatable), and `--profile <name>`.

Some of these are dev-channel only — see [[release-channels]].

`thegn api` is the capability catalog as a client: `api list` prints every
capability (scope, surfaces), `api schema` the control wire contract, and
`api call <cap> --params '{…}'` performs any routed capability over the
control socket — a newly routed verb is callable with no CLI change.
`thegn plugin list|check` inspects the configured [[plugins]].

## Which worktree am I acting on?

Every verb that acts **on** a worktree takes `--worktree <path>`. Omit it
and the target resolves in order:

1. `$THEGN_WORKTREE` — injected into every thegn pane;
2. the git toplevel of the current directory;
3. the current directory itself.

> Step 1 **overrides your current directory** inside a thegn pane. A
> script that `cd`s into a different repo and runs a worktree verb with no
> flag still targets the _pane's_ worktree. Pass `--worktree .` to mean
> "here, regardless of the pane".

Verbs whose argument _is_ the object keep positionals instead:
`wt rm <target>`, `wt new [name]`, `merge add [worktrees…]`,
`open <repo>`.

## A headless worktree, start to finish

```sh
wt=$(thegn wt new fix-parser --repo ~/code/app)   # prints the path only
cd "$wt"
thegn wt rm fix-parser --force                    # teardown + git + DB
```

`wt new` reuses the TUI wizard's pipeline — branch-name templates, base
resolution, the git-mutation lock, DB registration — but never provisions
a sandbox; the compositor prepares that lazily on first open. `wt rm`
tears down the sandbox, runs `git worktree remove`, and cleans every DB
row, so a removed worktree is never resurrected at the next launch.

To put an agent in it without a pane on screen:

```sh
thegn session open --agent coder --worktree "$wt" \
  --prompt "Implement the parser fix; commit on this branch" --bind --json
thegn session open --agent coder --stage code --worktree "$wt" --prompt "…"
```

`--agent` is an `[[agents]]`/`[[tools]]` name or a bare harness id
(`claude`, `codex`, `pi`); a prompt makes the launch headless. `--stage`
layers a `[[pipeline.stages]]` entry's `model` / `env` / `permissions`
over the agent — see [[configuration]]. The daemon composes the same
sandbox, credentials, model flag and env overlay an interactive pane gets.

## Landing work

- `thegn merge add` queues the current worktree's branch.
- `thegn integrate` folds the queued branches once, printing the plan and
  confirming first (`--dry-run` to preview, `--yes` for scripts). It folds
  only what you queued unless you pass `--all`.
- `thegn land` is the blessed one-shot: fold, gate, advance `main`.

None of these check the target out, which is what makes them safe against
a running instance. See [[merge-queue]] for the whole flow.

## Scripting

Most list-shaped reads accept `--json` and emit exactly one compact JSON
document on stdout with no ANSI: `wt list`, `repo list`, `repo recent`,
`env list`, `host list`, `host discover`, `ci runs`, `share list`,
`forward list`, `merge list`, `session list`, `pair list`, `map`, and
`wt new --json`. Treat those shapes as a stable API. (`notify list --json`
is NDJSON and `doctor --json` is a single object — both historical.)

`thegn host discover` is the inbound tailnet path: it lists the machines
your tailscale client already knows (from `tailscale status --json`) as
remote-host candidates, credential-free. `--promote <name|fqdn>` saves one
as a `[host.<name>]` target with no stored secret — Tailscale SSH (tailnet
ACLs) or the target's own sshd authorizes at connect time. It reads nothing
but the local client and runs only when you invoke it; `thegn doctor` shows
the `host_discovery`/`tailnet` probe. This is unrelated to `[sandbox.vpn]`
(a sandbox's own egress tunnel).

`thegn map` prints a **repo map**: the worktree's tree-sitter-indexed
entities (functions, types, …) grouped by file, ranked by caller
in-degree, under a line budget (`--budget`, default `[semantic]
map_budget_lines`). `--file <path>` narrows to one file's outline;
`--json` emits the rows (kind, name, file, line, degree) for scripts and
agents. No language server is needed — the index is built from the git
file listing on first use, capped by `[semantic] index_max_files` (an
oversized worktree maps _partially_ and says so). The same map is a
read-scope MCP tool (`semantic.map`) on `thegn mcp serve`.

Exit codes:

| Code | Meaning                                        |
| ---- | ---------------------------------------------- |
| 0    | success                                        |
| 1    | error                                          |
| 2    | transient — worth retrying                     |
| 3    | target not found (repo, worktree, branch, env) |

> `2` is overloaded: `clap` also uses it for **usage** errors (unknown
> flag, bad value), which are permanent. A script that retries on `2`
> should first confirm the command actually parsed.

## Inspecting the setup

`thegn agent list` is the one-screen answer to "what will actually run": one
line per `[[agents]]`/`[[tools]]` entry and per pipeline stage — harness,
model, env keys (never values), permission count — resolved the way a launch
resolves them. `thegn dispatch list --active` is the roster reduced to the rows
that occupy a slot. Both are terse by default and take `--json`.

The dispatch roster closes its own loop: `thegn dispatch verify <id>` checks
a finished row's artifact (exists under the worktree, tracked by git — exit
2 with the reasons when not), `thegn dispatch wait [--row <id>] [--any]
[--timeout ms]` blocks until a row's session exits (first wake wins with
`--any`, exit 2 on timeout), and `thegn dispatch set-status <id> done`
refuses a row whose artifact is missing or untracked unless `--force` — an
untracked artifact is not a handoff. See [[daemon-and-sessions]] for the
dispatch door (`session open --stage --issue`) that creates these rows.

- `thegn doctor` — resolved terminal capabilities, release channel,
  environment. See [[terminal-compatibility]].
- `thegn keys list` — every effective binding, from all three sources.
  The same set [[keybindings]] shows.
- `thegn keys validate` — non-zero on a chord conflict, so it works in a
  pre-commit hook.
- `thegn config` — read and explain resolved config; see
  [[configuration]].
- `thegn completions <shell>` — shell completion. Your package manager
  already installed these for `thegn` and `tg`; run it yourself only for
  a `cargo install`ed or hand-copied binary — never as an `eval` in your
  shell rc, which would launch thegn again in every pane it opens.
  `thegn doctor` says whether an installed file is stale. Worktrees,
  repos, sessions and config keys complete live; branches and PRs do
  not, deliberately.

`thegn open <repo>` takes a path anywhere inside a repo, or a unique repo
basename, and raises it in a running instance — the remote control.
