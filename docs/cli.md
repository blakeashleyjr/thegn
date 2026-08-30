# The `thegn` CLI contract

The TUI is the product; the CLI is the launcher, the remote control, and the
automation surface. This page is the stable contract scripts and agents can
rely on.

## Grammar

Noun-verb namespaces mirror the domain model (repo → workspace → worktree):

| Group         | Commands                                                                                                                           |
| ------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| Workspace     | `wt list\|new\|rm\|diff\|disk\|clean` · `repo list\|recent` · `open <repo>` · `map` · `land` · `integrate` · `merge`               |
| Forge         | `pr` · `issue` · `dispatch` · `kaneo` · `ci`                                                                                       |
| Search        | `search <pattern> [--regex\|--structural] [--replace <tpl> [--apply]]` — workspace find & replace (read/write scoped)              |
| Environments  | `env` · `zone` · `host` · `placement` · `sandbox` · `debug` · `mcp` · `plugin`                                                     |
| Session       | `session list\|open\|fork\|close\|send\|snapshot\|attach\|wait\|record` · `notify` · `logs` · `share` · `forward` · `sandbox-argv` |
| Control plane | `serve` · `session` · `attach` · `pair`                                                                                            |
| Meta          | `config` · `theme` · `skills` · `doctor` · `setup` · `completions`                                                                 |

`session open --resume-work <row>` resumes a failed pipeline row through the
roster (THE-86): it re-renders the row's stage prompt, gathers the row's
artifact/git/screen facts, and opens the finisher dispatch.

`session fork <id>` asks the daemon to start a new PTY from a live session.
Use `--scrollback` for a bounded handoff, `--tab` for a new tab, and
`--fork-worktree` to create a separate worktree first. `--json` includes the
child's `forked_from` lineage without exposing the launch recipe.

`dispatch put --chunk <file>` / `session open --chunk <file>` (THE-86) record
the chunk file a row dispatches under and run its scope gate before the row is
written: the file's `files:` frontmatter (globs: `*` within a segment, `**`
across) is checked against every ACTIVE sibling row's scope, and an overlap or
an unmet `after:` is refused with the colliding paths and row ids named —
`--force` (on `dispatch put`) is the explicit override. Scope display:
`dispatch list` carries a `chunk` column (the file's basename), and JSON rows
carry `chunk_path` plus `chunk_files` (the parsed `files:` list, omitted when
the file is unreadable at list time).

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

## Embedded and configured skills (`skills`)

```text
thegn skills list [--json]
thegn skills show <name> [--json]
thegn skills seed [--worktree <path>] [--json]
```

`list` prints deterministic metadata for the merged skill registry; `show`
prints one canonical, unmarked `SKILL.md` without writing anything. The three
shipped skills are compiled into the binary, so both commands work without a
network, a state database, or prior setup. Directories in `[skills].user_dirs`
extend that set from immediate `<name>/SKILL.md` packages. Discovery is
bounded and non-recursive, and the shipped entry wins if a configured package
reuses its name; unreadable or invalid packages are reported as diagnostics
without hiding the rest of the registry.

`seed` applies the explicit phase to one worktree, using the normal
`--worktree` resolution above. It selects the distinct harnesses referenced by
configured agents and pipeline stages (Claude when none are configured), and
writes only their native project layouts:

- Claude: `.claude/skills/<name>/SKILL.md`
- Codex: `.agents/skills/<name>/SKILL.md`
- Pi: `.pi/skills/<name>/SKILL.md`

A configured harness without a project skill layout is diagnosed and skipped.
Selection also respects each skill's harness list, lifecycle `when`, and
feature `gate`: the bundled `supervise` entry is always eligible, `mq` requires
the merge queue, and `pipeline` requires at least one configured pipeline
stage. All three permit create, startup, and explicit seeding. `[skills]
enabled = false` disables automatic create/startup seeding; it does not disable
an explicit `skills seed` command.

Every file thegn writes carries `thegn_managed`, the shipping version, and a
SHA-256 hash of the canonical unmarked document. A current managed file is a
no-op; an unmodified older managed file is updated. An unmarked file is
user-owned, and a managed file whose recorded hash no longer matches its
contents is treated as user-adopted: both are preserved and reported. An entry
named in `[skills].exclude`, or one no longer present in the registry, is
removed only when that same marker/hash proof says it is still unmodified.
Seeded paths are also added to the worktree's repository-local git exclude.

This is per-worktree provisioning, not synchronization of `~/.claude`,
`~/.codex`, `~/.pi`, or any other harness home. See the in-app `skills` page
for the package format.

## Machine-readable output (`--json`)

Most list-shaped read surfaces accept `--json` and emit exactly **one
compact JSON document** on stdout with no ANSI sequences: `wt list` / `list`,
`repo list`, `repo recent`, `env list`, `host list`, `ci runs`, `share list`,
`forward list`, `merge list`, `session list`, `pair list`, `disk`, `map`,
`dispatch list` (the agent-dispatch roster; `--active` keeps only rows that
occupy a slot; rows carry the daemon-written retry `note` — headless workers
that died of a transport failure are relaunched by the daemon per
`[pipeline.transport_retry]`, every outcome parked `waiting_human`), `agent list` (`{agents, stages}` — the effective harness /
model / env keys / permission count of every entry and pipeline stage), and
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

| Tool                    | Scope         | What it does                                                           |
| ----------------------- | ------------- | ---------------------------------------------------------------------- |
| `sessions_list`         | `read`        | list the daemon's live sessions                                        |
| `worktrees_list`        | `read`        | list registered worktrees (daemon, else DB cache)                      |
| `leases_list`           | `read`        | relay lease state per session                                          |
| `me`                    | `read`        | the caller's pairing id, label, granted scopes                         |
| `sessions_wait`         | `read`        | block until a session reaches a state (exited/idle/blocked/done/regex) |
| `semantic_map`          | `read`        | ranked, budgeted repo map of a worktree's indexed entities             |
| `semantic_blast_radius` | `read`        | a worktree's blast-radius: changed entities, callers, untested, risk   |
| `sessions_open`         | `write`       | open a session — raw `argv`, or a configured agent by name             |
| `sessions_kill`         | `write`       | kill a session's process (idempotent)                                  |
| `sessions_input`        | `write` **+** | send raw terminal input/control characters to a live session           |

The two `semantic_*` read tools (default `--scopes read`) answer from the
state DB + git listing directly — **no running daemon required** — and take a
`worktree` argument (plus `budget`/`file` for `semantic_map`). `semantic_map`
builds a capped index inline on first use; `semantic_blast_radius` returns a
clear "graph unavailable" result rather than erroring when no graph exists.

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

**If you installed a package** (Nix, or the release completions asset):
completions for both `thegn` and `tg` are already installed for bash, zsh and
fish. Nothing to do. They are generated by the same build that produced the
binary, so they cannot drift from it.

**Otherwise** (`cargo install`, a bare binary), write the file yourself, once,
per shell — and repeat each command with `tg` in place of `thegn` if you use the
alias (a script registered for `thegn` never fires for `tg`):

| Shell      | Command                                                                     |
| ---------- | --------------------------------------------------------------------------- |
| bash       | `thegn completions bash > ~/.local/share/bash-completion/completions/thegn` |
| zsh        | `thegn completions zsh > ~/.zfunc/_thegn` (any directory on `fpath`)        |
| fish       | `thegn completions fish > ~/.config/fish/completions/thegn.fish`            |
| elvish     | `thegn completions elvish` — no standard location; source it yourself       |
| powershell | `thegn completions powershell` — likewise; add it to your `$PROFILE`        |

Create the directory first if it does not exist. For zsh, the directory must be
on `fpath` before `compinit` runs (`fpath=(~/.zfunc $fpath)`) — that line is
cheap; the `eval` below is not.

**Do not put `eval "$(thegn completions zsh)"` in your shell rc.** That is the
usual advice for `gh` and `rustup`, and it is wrong here: thegn spawns a shell
per pane and a warm reattach restores many at once, so an rc-file `eval` puts a
`thegn` process launch into every pane restore. The installed file costs
nothing — your shell loads it lazily, on the first `<TAB>` for the command.

**Staleness.** The installed script is a registration shim: it contains no
command names, it asks the binary at completion time, so new verbs and flags
appear the moment you upgrade. A package regenerates it with the binary anyway.
For a hand-installed file, `thegn doctor` reports `fresh` / `stale` / `absent`
per shell and prints the command that fixes it.

**What completes:** verbs and flags always; and, with the shim installed, live
values — worktrees (`wt rm`, `--worktree`, `land`, `merge add`), repos
(`open`, `wt new --repo`), daemon sessions (`attach`, `session …`), registered
hosts, config keys (`config get|set`, `--set`), capability ids (`api call`),
and names from your config (`--profile`, `[env.<name>]`, `[[agents]]`,
`[mcp_servers.<name>]`). Values come from the state DB read-only, under a
budget (`THEGN_COMPLETE_BUDGET_MS`, default 100), and any failure completes
nothing rather than printing an error. Branch, PR and issue completion are
deliberately **not** offered: a `<TAB>` does not run git and does not call the
forge.

`thegn completions <shell> --static` emits a self-contained script instead —
structure only, no live values. It is the fallback if the dynamic mechanism is
ever unavailable.
