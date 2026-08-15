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

## Remote control (`open`)

`thegn open <repo>` resolves its argument (a path anywhere inside the repo,
or a unique repo basename) and:

- **live instance running** — enqueues a `focus_workspace` intent in the
  SQLite `intents` mailbox; the compositor's model refresh claims it within
  ~1s (no IPC — the DB is the mailbox, same as notifications);
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
