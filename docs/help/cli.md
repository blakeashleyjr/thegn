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
The dry run reads the live database, including changes still in its
write-ahead log, without writing it; it refuses a non-empty log without its
`-shm` index.

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

The managed file store (the keyring fallback) refuses to read token files
accessible to other users or with extra hard links. Its message names the file
and tells you to run `chmod 600` on it, or remove the extra link and store the
secret again.

Remote calendar account tokens appear in `thegn secret list` and `audit` as
`calendar.accounts[<name>].token`. `thegn secret migrate` moves a literal token
to a 0600 file reference and rewrites only that account's token; an account
defined in another config layer is skipped with a message to move it by hand.

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

If thegn cannot record a close, a notice says “Close not recorded for N
worktree(s); they stay registered and reappear on the next refresh: …”.

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
collapse/pin state, and dispatch ledger (including notes). Active dispatch
rows (queued, spawning, running, waiting for a human, or PR open) arrive as
`abandoned` with a `migrated while <status>: not resumed` note; every row's
artifact and chunk paths are dropped, so work is neither resumed nor
re-dispatched under the target profile. By default, pane relaunch commands,
scrollback, dispatch reports, free-form notes, and note rows are not moved;
parked rows keep only the migration prefix. `--include-history` carries them
unchanged into the target as plain text that may contain anything a tool
printed. These values are never printed by the audit. Named global layouts,
global caches, transports, git files/objects, branches, and worktree disk
locations are not moved. Thegn never reads or copies credentials, tokens,
identities, accounts, pairings, or profile configuration; content history
crosses only with `--include-history`.

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
- `thegn pr merge` reads the worktree's PR, then confirms `#N` at its head;
  pass `--yes` (`-y`) without a TTY. It merges only that reviewed head, prints
  “PR merged” only when the forge confirms the merge, and exits non-zero if
  the head moves, the forge refuses, or the result is unknown.

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

`thegn ci logs <run-id> <job-id> --worktree <path>` prints a redacted,
bounded window of the cached job log (newest lines, or a window around the
first failure). It reads the cache first and uses the daemon's blocking provider
lane for a miss; `ci log` remains a compatibility alias.
With `--json`, stdout is one valid JSON value with explicit `fetched_at`,
`truncated`, and `redacted` metadata.
`ci logs` reuses a cached log only when it has a recorded head commit; a log
with an unknown head is printed but not cached. The CI panel's failed-job log
windows and background log ingestion apply a stricter check: they reuse a
cached window only for the run's current head and only if the job has not
finished again since the log was fetched.
If run detail is unavailable or the job is not listed, `ci logs` still prints
the log without a job name or head, warns, and does not cache it. With `--json`,
the warnings are in a `warnings` array (empty for a cached log); the control
API's `ci.logs` reply carries the same field.
The daemon/API `ci.logs` `tail_lines` argument can narrow `[ci] log_tail_lines`,
but a larger value is clamped to that setting. A value of `0` is refused.

When a GitLab job list reaches its page, row or byte budget, `thegn ci view` and
`ci logs` warn that the list is incomplete and show the first jobs. Incomplete
details are not used to ingest job logs, so their logs are not cached and CI
autofix does not start from them. `thegn ci trigger -i KEY=VALUE` inputs are
validated before any provider request; malformed or duplicate keys, control
characters, and oversized keys or values are refused.

On Unix, Ctrl-C or SIGTERM during `thegn ci rerun`, `ci trigger` or `ci cancel`
stops the provider command and exits non-zero; the action may or may not have
reached the provider. Check with `thegn ci runs`.

`thegn ci runs --json` emits a JSON array on success, including `[]` when the
provider has no runs. If no CI provider resolves, it emits an error object with
`reason: "no-provider"`; a fetch failure or partial list containing malformed
rows also emits an error object (`error`, and `discarded_rows` for a partial
list). It exits 0 in all three cases so scripts can consume the
graceful-degradation result.
Only `thegn ci runs --branch <current branch>` refreshes the cached runs shown
in the CI panel; other branches and the all-branch listing leave that cache
alone.
`ci runs --limit` accepts 1 to 200 runs; the daemon's `ci.runs` control route
rejects a limit above 200.

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

`thegn mcp install <name>` installs a declared `[mcp_servers.<name>]` source.
npm and cargo installers run inside bubblewrap with a read-only host root, the
host home hidden (with toolchain directories under it kept readable) and
temporary directories hidden, and only the staging and install directories
writable; network access remains available. If bubblewrap with
user namespaces is unusable, or on a non-Linux platform, installation is
refused unless `--unsafe-host-install` is given at an interactive terminal;
that runs the installer uncontained and prints a warning.

Before replacing an install, the staged binary must resolve to a regular file
inside staging or installation is refused and the previous install is kept. An
owner-only `.thegn-provenance.json`, installed beside the tool, records the
package and version, binary and available lock-file SHA-256s, and the
containment used.
Before reusing an installed npm or cargo tool, thegn checks
that the record matches its package, version, binary path and current SHA-256.
A missing record triggers a reinstall; when a record is mismatched, unreadable
or readable by other users, the tool refuses it and names the install directory
to remove before reinstalling.
For each managed npm or cargo tool, `thegn doctor` shows `verified`, `no
install record` (the next use reinstalls through the contained installer),
`MISMATCH` (refused until reinstalled), `not installed`, or `n/a (release
download)`. `doctor --json` includes `provenance` and `provenance_reason`. The
check is the same one used before launch and does not change the install.

`thegn supervise plan --all` and `thegn supervise status --all` read one
newest-first roster page. `--limit` defaults to 2048 and is capped at 4096;
`--before <row>` continues before a row id. Both options require `--all`, and
a truncated page prints its next cursor on stderr.

The `--issue` filters on `supervise plan` and `supervise status` match every
spelling of one issue (`THE-74`, `linear:THE-74`, any case); ids from different
providers stay distinct.

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
`thegn dispatch list` and `thegn dispatch status --all` show the newest 2048 rows by
default; `--limit` sets the page size and `--before <row>` uses the cursor printed
on stderr when older rows follow.
The shared JSON output for `thegn dispatch list --json` and
`thegn dispatch status --json` escapes terminal-sensitive controls and Unicode
format characters with JSON escapes such as `\uXXXX`; ordinary Unicode remains
readable.

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
or when the row's worktree is missing, detached, or dirty. `dispatch set-status done`
on an artifact-bearing row, `dispatch reap`, and the daemon's periodic reap require
a satisfied attestation for the row's current HEAD; `dispatch verify` shows its
state. The worker's own `gate:` line is a claim, not evidence. `--force --why`
remains the recorded override.

Merge-queue admission consumes the same trusted gate attestation for the enqueued
commit.

`dispatch set-status` follows a transition table: `merged` is reachable only
from `done` or `pr_open`; `done` only from `running`, `spawning`,
`waiting_human` or `failed`; and `queued` is never a target. Terminal rows move
only `done` → `merged` or `abandoned`, and `failed` → `done` or `abandoned`.
With `--json`, a classified refusal prints `{"error": ..., "id": ..., "message":
...}` with an error kind of `illegal_transition`, `stale`, `missing_evidence` or
`missing_row` before the command fails.

`dispatch claim`, `dispatch put`, `session open --stage --issue`, `session open --resume-work` and `dispatch set-status` take `--lease <name> --owner <id>` where applicable. A claim, put or resume binds its row to that lease; further mutations for that issue require the same live lease. `--admin --why '<reason>'` on claim, put or set-status is the manual override and is recorded on the row. Rows created without a lease behave as before.

The `[pipeline.transport_retry]` budget is stored on each dispatch row and
counted once per run. It survives daemon restarts without exceeding the
configured budget; a human re-drive starts a fresh budget. If a stage launch
cannot bind its session to its dispatch row, the worker is stopped and the
launch is refused.
The daemon also trusts the dispatch row when deciding whether an unbound launch
is a pipeline stage: it stops and refuses a stage row or a vanished row even if
the client did not mark the launch as a stage. `dispatch set-status --force` on
a missing row reports `missing_row`, including with `--json`.

`session open --resume-work` and the daemon's transport retry carry the same
stage configuration digest as a fresh stage dispatch. If the stage or agent was
edited after the prompt was rendered, the launch is refused as changed between
claim and launch. A launch-id-bound pipeline stage launch without a digest is
refused. Restart the daemon after upgrading; the digest format changed.

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
