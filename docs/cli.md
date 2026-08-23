# The `thegn` CLI contract

The TUI is the product; the CLI is the launcher, the remote control, and the
automation surface. This page is the stable contract scripts and agents can
rely on.

## Grammar

Noun-verb namespaces mirror the domain model (repo → workspace → worktree):

| Group         | Commands                                                                                                     |
| ------------- | ------------------------------------------------------------------------------------------------------------ |
| Workspace     | `wt list\|new\|rm\|diff\|disk\|clean` · `repo list\|recent` · `open <repo>` · `land` · `integrate` · `merge` |
| Forge         | `pr` · `issue` · `kaneo` · `ci`                                                                              |
| Environments  | `env` · `zone` · `host` · `placement` · `debug` · `mcp`                                                      |
| Session       | `notify` · `logs` · `share` · `forward` · `sandbox-argv`                                                     |
| Control plane | `serve` · `session` · `attach` · `pair`                                                                      |
| Meta          | `config` · `theme` · `doctor` · `setup` · `completions`                                                      |

The legacy bare verbs (`list`, `diff`, `disk`, `clean`, `repos`, `recent`)
keep working forever with byte-identical output; they are merely hidden from
`--help`. Global flags everywhere: `--config`, `--log-level`,
`--set key=value` (repeatable), `--profile <name>`.

## Worktree targeting

Every verb that acts **on** a worktree (rather than taking one as its object)
uses the same scope selector: `--worktree <path>`. When the flag is omitted,
the target resolves in order:

1. `$THEGN_WORKTREE` (injected into every thegn pane), if it exists locally;
2. the git toplevel of the current directory;
3. the current directory itself.

Note that step 1 **overrides your current directory** inside a thegn pane: a
script that `cd`s into a _different_ repo and runs, say, `thegn sandbox-argv`
or `thegn placement plan` with no `--worktree` targets the **pane's** worktree,
not the directory it stands in. Pass `--worktree .` (or an explicit path) to
target the cwd regardless of the pane. (`sandbox-argv` and `placement plan`
adopted this shared resolution in 0.1.0-alpha.1; they previously used the raw
cwd only.)

Verbs whose argument **is** the object keep positionals: `wt rm <target>`
(path or branch), `wt new [name]`, `merge add [worktrees…]` (multi-target),
`open <repo>` (a repo, not a worktree).

Two verbs keep their own default rule: `wt disk` scans **all** known
worktrees unless `--worktree` narrows it to one, and `placement explain`
shows the most recent decision overall unless `--worktree` filters it.

The legacy trailing positional on `env *`, `placement plan|explain`,
`merge rm|land`, `land`, and `sandbox-argv` still parses but is deprecated
and hidden from help; passing both the flag and the positional is a usage
error. Scripts should move to `--worktree`. A usage error exits with clap's
argument-parse code (`2`) and is **permanent — do not retry it** (see the
exit-code note below); it is distinct from the retryable runtime failures the
table describes.

## Headless worktree lifecycle

```sh
wt=$(thegn wt new fix-parser --repo ~/code/app)   # prints the path only
cd "$wt"
thegn wt rm fix-parser --force                    # teardown + git + DB
```

`wt new` reuses the TUI wizard's pipeline (branch naming templates, base
resolution, the serial git-mutation lock, DB registration) but never
provisions a sandbox — the compositor prepares lazily on first open.
`wt rm` runs provider/sandbox teardown synchronously, then
`git worktree remove`, then cleans every DB row (including tab groups, so a
removed worktree is never resurrected at the next launch).

## Machine-readable output (`--json`)

Most list-shaped read surfaces accept `--json` and emit exactly **one
compact JSON document** on stdout with no ANSI sequences: `wt list` / `list`,
`repo list`, `repo recent`, `env list`, `host list`, `ci runs`, `share list`,
`forward list`, `merge list`, `session list`, `pair list`, `disk`, and
`wt new --json` (`{branch, path, root, base}`). Treat the shapes as a stable
API. (Two pre-existing surfaces keep their historical shapes: `notify list
--json` is NDJSON, `doctor --json` is one object.) A few list surfaces are
text-only today — `zone list`, `mcp list`, and `theme list` have no `--json`.

## Exit codes

| Code | Meaning                                                             |
| ---- | ------------------------------------------------------------------- |
| 0    | success                                                             |
| 1    | error                                                               |
| 2    | transient/retryable (e.g. a `host provision` step worth re-running) |
| 3    | target not found (repo, worktree, branch, env)                      |

Caveat: code `2` is **overloaded**. thegn returns it for retryable runtime
failures (above), but `clap` also uses `2` for argument/usage parse errors
(unknown flag, mutually-exclusive args, bad value) — those are **permanent**,
not retryable. A script that retries on `2` should first confirm the command
parsed (e.g. it is not a usage error printed to stderr) before treating the
exit as transient.

## Remote control (`open`)

`thegn open <repo>` resolves its argument (a path anywhere inside the repo,
or a unique repo basename) and:

- **live instance running** — enqueues a `focus_workspace` intent in the
  SQLite `intents` mailbox; the compositor's model refresh claims it within
  ~1s (no control-plane call — the DB is the mailbox, same as notifications);
- **no instance** — sets the active-workspace pointer and launches the
  compositor on that workspace;
- `--no-launch` — records the pointer/intent only (for scripts).

## Docs endpoint for agents (`mcp serve`)

`thegn mcp serve` runs thegn itself as a **read-only MCP server over stdio** — a
Context7-style endpoint a coding agent connects to in order to learn how thegn
works and inspect the live config. Register it once:

```sh
claude mcp add thegn -- thegn mcp serve
# or, generic MCP config:  { "command": "thegn", "args": ["mcp", "serve"] }
```

It speaks newline-delimited JSON-RPC (`initialize`, `tools/list`, `tools/call`,
`resources/list`, `resources/read`) and exposes:

| Tool             | What it returns                                                                  |
| ---------------- | -------------------------------------------------------------------------------- |
| `search_docs`    | full-text search of the in-app help corpus → matching page ids                   |
| `read_doc`       | a help page's markdown by id (incl. generated `keybindings`, `config-reference`) |
| `list_docs`      | every help page (id + title) — the browse index                                  |
| `get_config`     | your current effective config as JSON (secrets redacted); optional dotted `key`  |
| `explain_config` | how a config key resolves — effective value + which layer set it                 |

Resources mirror these: `thegn://help/<id>` per page, `thegn://config/current`,
`thegn://config/schema`, `thegn://doc/cli`, `thegn://doc/readme`. The endpoint is
**read-only** and never serves secrets — token/key/credential fields are masked
before `get_config` / `thegn://config/current` go out.

## Completions

`thegn completions bash|zsh|fish|elvish|powershell` generates completions
for the invoked binary name (`thegn` or `tg`).
