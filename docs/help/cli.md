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

Noun-verb namespaces mirror the domain model (repo → project →
worktree):

| Group         | Commands                                                                                                       |
| ------------- | -------------------------------------------------------------------------------------------------------------- |
| Project       | `wt list/new/rm/diff/disk/clean` · `repo list/recent` · `open <repo>` · `map` · `land` · `integrate` · `merge` |
| Program       | `program list/create/rename/rm/assign` · `wt new --program`                                                    |
| Forge         | `pr` · `issue` · `ci` · `kaneo`                                                                                |
| Automation    | `autopilot status [--repo PATH] [--json]`                                                                      |
| Environments  | `env` · `zone` · `host` · `placement` · `debug` · `mcp` · `plugin`                                             |
| Session       | `notify` · `logs` · `share` · `forward`                                                                        |
| Control plane | `serve` · `session` · `events` · `attach` · `pair` · `api`                                                     |
| Meta          | `config` · `keys` · `theme` · `doctor` · `db` · `setup` · `completions`                                        |

Global flags everywhere: `--config`, `--log-level`, `--set key=value`
(repeatable), and `--profile <name>`.

Before reclaiming pages, `thegn db vacuum` removes semantic-index rows for
deleted files and cache keys left by removed worktrees, and reports both
counts. `--json` emits one report (including `pruned_semantic_rows` and
`pruned_cache_keys`); `--dry-run` removes nothing.

Some of these are dev-channel only — see [[release-channels]].

`thegn api` is the capability catalog as a client: `api list` prints every
capability (scope, surfaces), `api schema` the control wire contract, and
`api call <cap> --params '{…}'` performs any routed capability over the
control socket — a newly routed verb is callable with no CLI change.
For example, `folders.assign` assigns or clears a worktree's repo-local sidebar
folder (`folder` name, or `clear: true`) through `api call` (Git scope), HTTP,
gRPC, MCP (`folders_assign`, needs `--scopes git`), or plugin `host.call`.
`thegn plugin list|check` inspects the configured [[plugins]].

The multi-repo group commands use the canonical `thegn program` namespace.
`thegn project` remains a deprecated, behavior-identical alias for three
stable releases and warns with the replacement. Likewise, `wt new --program`
is canonical while `wt new --project` remains accepted as a deprecated alias;
the warning is kept off JSON stdout.

### Secrets

`thegn secret set|rm|list|migrate|audit|ssh rotate` manage the credential
broker without printing secret values. `rm` reports the keyring and file
outcomes and exits nonzero if keyring removal fails, its outcome is unknown, or
file removal fails; retry once the keyring responds when its outcome is unknown.
A timed-out keyring write never falls back to the file store. `audit` shows
managed SSH fingerprints in OpenSSH `SHA256:` form and labels older stored
forms "legacy, unverified" (`fingerprint_verified` in `--json`).
Only one `thegn secret ssh rotate` runs per key at a time; a second is refused
before changes, and a custody record changed mid-rotation stops further remote
changes.
Keyring operations run on one worker. While an earlier keyring call is stuck
past its timeout, further keyring operations fail at once without starting,
and nothing is written to a file instead.

### Tailing the event feed

`thegn events tail` is a read-only thin client for the daemon's live control
feed. It waits on the stream; it does not poll, open a session, or enable
`--allow-session-input`:

```sh
thegn events tail
thegn events tail --kinds activity,exit --session "$SESSION"
thegn events tail --signal-lag --json | jq -c 'select(.kind == "activity")'
```

`--kinds` accepts the control API's comma-separated observer-filter kinds:
`activity`, `lease`, `pairing`, `sessions`, and `exit`.
`snapshot` and `delta` are attach-only and are rejected here. `--session`
narrows session-keyed frames. `--signal-lag` opts into an explicit `lagged`
frame when the bounded feed has dropped events. JSON mode emits one canonical
frame per line, with the non-filterable daemon `hello` frame first on every
transport.

The feed has no replay. Bootstrap by listing the resources you need, subscribe,
then re-list after `sessions`, opted-in `lagged`, or reconnect signals. Listing
and subscribing are not atomic, so clients must idempotently reconcile by
stable session/worktree ids. Pane attach is separate: it supplies the current
emulator `snapshot` followed by live `delta` frames; the observer feed does not
promise a synthetic state snapshot.

The local Unix socket uses the current same-user policy. A TCP `serve` endpoint
uses its existing bearer token and read scope; filters only narrow an already
authorized subscription. A missing daemon returns the usual recoverable
no-daemon error. There is no new config key.

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
a sandbox; the compositor prepares that lazily on first open. It runs
`pre_create` before git and waits for post-create hooks before printing the
new path. `wt rm` runs `pre_destroy` before teardown, then `git worktree
remove`, `post_destroy`, and DB cleanup. A failed blocking destroy hook leaves
the worktree in place and names `--force`; that existing flag is also the
explicit confirmation and skips the veto. Repo hooks are still warn-only.

Close keeps a worktree's files and records that you closed it, so automatic
discovery will not bring it back. Run `thegn wt reopen <path>` to clear that
record and register it again without starting a pane. Worktrees closed before
this release have no close record.

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

`wt new --from-issue` and batched project creation use the same lifecycle
seam. The pipeline board itself is structure-only: an external supervisor
that creates a worktree should use `wt new` or the existing
`worktrees.create` control operation, both of which run the shared hooks.

To move a persisted session presentation between profiles, use the
admin-only, two-store operation:

```sh
thegn --profile <source> session move <worktree> --to-profile <target>
```

`--profile` (or `THEGN_PROFILE`) selects the source; `<target>` must already
exist. The exact stored worktree path is selected. The move carries its
worktree registration, all matching current groups and tabs, sidebar
collapse/pin state, and dispatch ledger (including notes and artifact/chunk
paths). Pane commands, scrollback, reports, and notes are opaque text and are
carried unchanged, but never printed by the audit. Named global layouts,
global caches, transports, git files/objects, branches, and worktree disk
locations are not moved. Neither profile configuration nor credentials,
tokens, identities, accounts, pairings, or secrets are read or copied.

The target transaction commits first and is read back using a sanitized
fingerprint. Only after confirmation are the selected source rows deleted;
dispatch IDs are remapped and source daemon/pane IDs are cleared. A retry
adopts an identical target import and completes pending source cleanup. A
target-confirmed but source-pending result is retryable (exit 2). The command
is cold-safe. Live source daemon sessions refuse the operation unless
`--kill` is explicit; `--kill` terminates and re-lists them before import.
`--dry-run` prints the complete redacted plan and never kills or writes, and
`--json` emits the audit object for scripts. A target daemon notification is
best effort and a warning cannot undo a confirmed move.

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
`wt new --json`, and `autopilot status`. Treat those shapes as a stable API.
`autopilot status --json` returns `{enabled, repo, runs}`; `runs` contains only
bounded issue identity, lifecycle, worktree/branch, PR, timestamps, and reason
fields. It never starts, stops, retries, or exposes issue bodies, commands,
secrets, or CI logs. With autopilot disabled it reports the disabled state and
an empty/bounded journal view. (`notify list --json`
is NDJSON and `doctor --json` is a single object — both historical.)

`thegn ci logs <run-id> <job-id> --worktree <path>` prints the bounded,
redacted cached job-log tail. It reads the cache first and uses the daemon's
blocking provider lane for a miss; `ci log` remains a compatibility alias.
With `--json`, stdout is one valid JSON value with explicit `fetched_at`,
`truncated`, and `redacted` metadata.

`thegn ci runs --json` emits a JSON array on success. If provider fetching fails,
or the provider returns a partial list containing malformed rows, it emits an
error object (`error`, and `discarded_rows` for a partial list) and exits 0 so
scripts can continue to consume the command's graceful-degradation result.

`thegn host discover` is the inbound tailnet path: it lists the machines
your tailscale client already knows (from `tailscale status --json`) as
remote-host candidates, credential-free. `--promote <name|fqdn>` saves one
as a `[host.<name>]` target with no stored secret — Tailscale SSH (tailnet
ACLs) or the target's own sshd authorizes at connect time. It reads nothing
but the local client and runs only when you invoke it; `thegn doctor` shows
the `host_discovery`/`tailnet` probe. This is unrelated to `[sandbox.vpn]`
(a sandbox's own egress tunnel).

`thegn host add <target> --name <alias> --replace` takes over an alias with
stale recorded state from a host no longer defined in configuration and clears
that state. It refuses while the host has live placement tenants.
Host aliases use lowercase letters, digits, and hyphens, start and end with a
letter or digit, and are limited to 128 bytes. If a new host's default alias is
already used by another host, thegn adds a short digest of the host authority.

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
refuses a row whose artifact is missing or untracked unless you pass
`--force --why '<reason>'`; that reason is stored on the row and shown by
`dispatch list` and `dispatch status`. An untracked artifact is not a handoff.
See [[daemon-and-sessions]] for the
dispatch door (`session open --stage --issue`) that creates these rows.

`thegn dispatch gate <row>` runs the repo's configured `[merge_queue] gate_command`
through the isolated fold-gate executor and records a trusted attestation bound
to that worktree's committed HEAD. This explicit Lead-only invocation runs even
when `[merge_queue] gate_on = false`; that switch controls automatic fold gating.
It is refused under a worker dispatch capability, when no gate is configured,
or when the row's worktree is missing, detached, or dirty. `dispatch verify` and
`dispatch set-status done` do not consume this attestation yet.

`dispatch set-status` follows a transition table: `merged` is reachable only
from `done` or `pr_open`; `done` only from `running`, `spawning`,
`waiting_human` or `failed`; and `queued` is never a target. Terminal rows move
only `done` → `merged` or `abandoned`, and `failed` → `done` or `abandoned`.
With `--json`, a classified refusal prints `{"error": ..., "id": ..., "message":
...}` with an error kind of `illegal_transition`, `stale`, `missing_evidence` or
`missing_row` before the command fails.

The `[pipeline.transport_retry]` budget is stored on each dispatch row and
counted once per run. It survives daemon restarts without exceeding the
configured budget; a human re-drive starts a fresh budget. If a stage launch
cannot bind its session to its dispatch row, the worker is stopped and the
launch is refused.
The daemon also trusts the dispatch row when deciding whether an unbound launch
is a pipeline stage: it stops and refuses a stage row or a vanished row even if
the client did not mark the launch as a stage. `dispatch set-status --force` on
a missing row reports `missing_row`, including with `--json`.

Finished rows (not review tasks) older than 90 days are pruned during
background maintenance unless their worktree is still registered, an active
autopilot run uses them, or a retained child points to them. `thegn db vacuum`
does not run this prune.

- `thegn doctor` — resolved terminal capabilities, release channel,
  environment. See [[terminal-compatibility]] and [[debugging]].
  Its sandbox section also reports how sandboxed panes see git metadata; the
  same exposure appears in `--json` output.
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
