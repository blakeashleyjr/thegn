# The `thegn` CLI contract

The TUI is the product; the CLI is the launcher, the remote control, and the
automation surface. This page is the stable contract scripts and agents can
rely on.

## Grammar

Noun-verb namespaces mirror the domain model (repo → workspace → worktree):

| Group         | Commands                                                                                                              |
| ------------- | --------------------------------------------------------------------------------------------------------------------- |
| Workspace     | `wt list\|new\|rm\|diff\|disk\|clean` · `repo list\|recent` · `open <repo>` · `land` · `integrate` · `merge`          |
| Forge         | `pr` · `issue` · `dispatch` · `kaneo` · `ci`                                                                          |
| Search        | `search <pattern> [--regex\|--structural] [--replace <tpl> [--apply]]` — workspace find & replace (read/write scoped) |
| Environments  | `env` · `zone` · `host` · `placement` · `debug` · `mcp`                                                               |
| Session       | `notify` · `logs` · `share` · `forward` · `sandbox-argv`                                                              |
| Control plane | `serve` · `session` · `attach` · `pair`                                                                               |
| Meta          | `config` · `theme` · `doctor` · `setup` · `completions`                                                               |

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
`forward list`, `merge list`, `session list`, `pair list`, `disk`,
`dispatch list` (the agent-dispatch roster), and
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

## Docs + live-state endpoint for agents (`mcp serve`)

`thegn mcp serve` runs thegn itself as an MCP server over stdio — a
Context7-style endpoint a coding agent connects to in order to learn how thegn
works, inspect the live config, and (once explicitly granted) drive the pane
daemon: open a session, send it input, wait on it, kill it. Register it once:

```sh
claude mcp add thegn -- thegn mcp serve
# or, generic MCP config:  { "command": "thegn", "args": ["mcp", "serve"] }
```

It speaks newline-delimited JSON-RPC (`initialize`, `tools/list`, `tools/call`,
`resources/list`, `resources/read`) and exposes the docs tools unconditionally:

| Tool             | What it returns                                                                  |
| ---------------- | -------------------------------------------------------------------------------- |
| `search_docs`    | full-text search of the in-app help corpus → matching page ids                   |
| `read_doc`       | a help page's markdown by id (incl. generated `keybindings`, `config-reference`) |
| `list_docs`      | every help page (id + title) — the browse index                                  |
| `get_config`     | your current effective config as JSON (secrets redacted); optional dotted `key`  |
| `explain_config` | how a config key resolves — effective value + which layer set it                 |

Resources mirror these: `thegn://help/<id>` per page, `thegn://config/current`,
`thegn://config/schema`, `thegn://doc/cli`, `thegn://doc/readme`. Docs
resources/tools never serve secrets — token/key/credential fields are masked
before `get_config` / `thegn://config/current` go out.

Beyond the docs tools, `--scopes` (comma-separated `read,write,git,admin`,
default `read`) gates a set of **state tools** that talk to a running pane
daemon — default-deny: a tool neither appears in `tools/list` nor is callable
until its scope is granted.

| Tool             | Scope         | What it does                                                           |
| ---------------- | ------------- | ---------------------------------------------------------------------- |
| `sessions_list`  | `read`        | list the daemon's live sessions                                        |
| `worktrees_list` | `read`        | list registered worktrees (daemon, else DB cache)                      |
| `leases_list`    | `read`        | relay lease state per session                                          |
| `me`             | `read`        | the caller's pairing id, label, granted scopes                         |
| `sessions_wait`  | `read`        | block until a session reaches a state (exited/idle/blocked/done/regex) |
| `sessions_open`  | `write`       | open a session — raw `argv`, or a configured agent by name             |
| `sessions_kill`  | `write`       | kill a session's process (idempotent)                                  |
| `sessions_input` | `write` **+** | send raw terminal input/control characters to a live session           |

`sessions_input` needs an additional, explicit `--allow-session-input` flag
on top of `write` scope — typing into an arbitrary live session (whatever is
running there executes exactly as if typed at its keyboard) is a materially
larger blast radius than the daemon's other write verbs, so it stays off even
under `--scopes write` until an operator opts in per-invocation:

```sh
thegn mcp serve --scopes write --allow-session-input
```

Every mutating tool call is audited (`tracing`, target `thegn::mcp`) with its
capability id and a redacted view of its arguments — terminal input bytes and
launch environment values are replaced by a size descriptor, never logged
verbatim.

## Third-party MCP aggregation (`mcp proxy`)

`thegn mcp serve` is thegn's _own_ tool endpoint; `thegn mcp proxy` is the
**hub** for _third-party_ upstreams. It aggregates every **exposed**
`[mcp_servers.<name>]` behind one stdio endpoint an agent registers as its
single MCP server — tools namespaced `<upstream>__<tool>`, calls routed to the
owning upstream, health-checked with a per-upstream circuit breaker.

```sh
thegn mcp wire                 # write the secret-free proxy entry into agent CLIs
thegn mcp wire --agent claude  # a specific vendor (claude|cursor|windsurf|vscode|zed|gemini|amp)
thegn mcp status               # per-upstream: exposed/hidden tools, scope, breaker
thegn mcp reload               # daemon: re-read config + reconcile upstreams
thegn mcp emit --proxy         # print the single secret-free entry (what `wire` writes)
```

- **Default-deny filtering.** An upstream contributes nothing until
  `[mcp_servers.<name>.proxy] tools = [...]` (globs; `["*"]` is the explicit
  everything opt-in) — the tool-poisoning blast-radius control. `thegn mcp list`
  shows each upstream's exposed-vs-hidden tools.
- **Credential custody.** Upstream `env` values may be `keyring:`/`env:`/`file:`
  secret refs, resolved **only at spawn** in the hub. The wired/emitted proxy
  entry carries **no env** — agents get the tools, never the keys. Manage keyring
  entries with `thegn mcp secret set|list|rm <name>` (`list` names entries, never
  values).
- **Partitioning.** `[mcp_servers.<name>.proxy] scope = "global"|"workspace"|
"worktree"` runs one instance per scope key, templating `{workspace}` /
  `{worktree}` / `{repo_root}` / `{branch}` into the server's env/args.
- **Presets.** `thegn mcp preset list | show <name> [--write]` ships vetted
  `[mcp_servers]` blocks (memory servers among them, at least one fully local and
  offline) — references, not bundled dependencies.

## Completions

`thegn completions bash|zsh|fish|elvish|powershell` generates completions
for the invoked binary name (`thegn` or `tg`).
