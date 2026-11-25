# The `thegn` CLI contract

The TUI is the product; the CLI is the launcher, the remote control, and the
automation surface. This page is the stable contract scripts and agents can
rely on.

## Grammar

Noun-verb namespaces mirror the domain model (repo → workspace → worktree):

| Group         | Commands                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | Channel                                                                         |
| ------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Workspace     | `wt list\|new\|rm\|diff\|disk\|clean` · `repo list\|recent\|trust\|provenance\|forget` · `program` — Manage programs (multi-repo groups — grouping only, no policy; alias: `project`; `list\|create\|rename\|rm\|assign`) · `open <repo>` · `map` · `land` · `integrate` · `merge` · `release prepare --bump <alpha\|beta\|rc\|patch>` · `search <pattern> [--regex\|--structural] [--replace <tpl> [--apply]]` — workspace find & replace (read/write scoped)                                                                                                                                                                | stable                                                                          |
| Forge         | `pr` · `issue` · `dispatch` · `supervise` · `kaneo` · `ci` · `autopilot` — Opt-in issue-to-PR supervisor; `status` is read-only and never starts work                                                                                                                                                                                                                                                                                                                                                                                                                                                                         | stable                                                                          |
| Environments  | `env` · `zone` · `host` (dev) · `placement` (dev) · `fleet` (dev) · `job` (dev) · `sandbox` · `debug` · `mcp` · `plugin`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | stable; `host`, `placement` and `fleet` are dev; `sandbox` remote parts are dev |
| Session       | `session list\|open\|fork\|close\|send\|snapshot\|attach\|wait\|record` · `notify` · `automations` — Inspect trusted automation rules, dry-run an event fixture, or manage stored rules (`list\|test\|add\|rm\|runs\|pause\|resume`) · `lead` — The Lead kill switch (`pause [--keep-sessions]\|resume\|status [--json]\|login [--mcp <server>]`) · `inbox` — The native inbox, mail, Todoist or any command (`list [--account <a>] [--unread] [--json]\|test <account> [--json]\|status [--json]`) · `logs` · `share` · `forward` · `sandbox-argv` · `events` — Stream the daemon's filtered live event feed (`events tail`) | stable                                                                          |
| Control plane | `serve` · `session` · `attach` · `pair` · `api` · `proxy` — Model proxy: status/stats (read), start/stop (admin) (`status\|stats\|start\|stop`) · `agent`                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | stable                                                                          |
| Meta          | `voice setup\|status\|model list\|model pull\|model rm` · `config` · `usage` — Per-account AI usage windows as a table or JSON · `secret` — Credential broker: store/remove/list/migrate/audit secrets, rotate SSH keys (`set\|rm\|list\|migrate\|audit\|ssh rotate`) · `keys` — Inspect and check keybindings: `list`, `validate`, `hints` · `theme` · `skills` · `doctor` · `db` — Inspect or reclaim unused state database space (`vacuum`) · `setup` · `completions`                                                                                                                                                      | stable; `voice` is dev                                                          |

The gRPC control service also exposes `AgentSessions`, `SessionsRecord` and
`WorktreesCreate`; their capability ids and request/response details are in the
[gRPC method reference](api/grpc.md). Launch presets stay CLI-only because
their configuration and environment resolve in the local compositor.

### Channels

Stable verbs are available in every build. Dev verbs are available in the dev
channel (`.#dev` flake output or `THEGN_CHANNEL=dev`); a stable build refuses
`host`, `placement` and `fleet` with a message identifying each as a dev-channel feature
and directing you to the dev build or `THEGN_CHANNEL=dev`. Experimental remote
worktrees, cloud providers, placement, and voice stay off in stable even when
configured. Observe dashboards and plugin app tabs are stable. The stable/dev
matrix is shown by `thegn doctor` and [release channels](help/release-channels.md).

`voice setup [--language en|auto] [--model NAME] [--kind embedded|http|command]`
configures experimental dictation. Setup confirms model size before download;
`--yes` accepts it in scripts, and `--model-path PATH` uses a local GGML file.
HTTP setup accepts a full `--endpoint` and optional `--credential` secret reference;
`--final-only` avoids repeated audio billing for live previews. `voice status
--json` reports readiness without microphone access. `voice model list --json`,
`voice model pull NAME [--yes]`, and `voice model rm NAME` manage pinned artifacts.
Native capture requires `voice-native` (`voice-metal` on Apple silicon) and the
experimental channel. See [voice help](help/voice.md) and
[release verification](voice-verification.md).

`session open --resume-work <row>` resumes an eligible unfinished pipeline row
through the roster: it refuses an unstamped spawning/running source
whose worker may still be live, reconciles eligible parked or exited work, and
atomically admits the finisher against the stage's duplicate/capacity gate
(refusal leaves the source unchanged). It renders the new row's exact id and
artifact target into the stage prompt, with the source artifact and
git status/diff-stat facts carried as bounded, untrusted recovery data (prior
terminal output is not replayed).

`session open --track <issue>` is the plain-open path for an ad-hoc worker. It
requires `--worktree`, opens the session first, then records a roster row for
the supplied issue id (for example, `linear:THE-1976`); the row is visible to
`dispatch list`, `dispatch wait` and `dispatch verify`. For a lane without an
issue, `session open --brief-file F [--vars key=value]` reads the prompt from a
file; repeatable `--vars` bindings render `{key}` placeholders in that file.
`--brief-file` and `--vars` are for plain opens and cannot be combined with
`--prompt`, `--issue` or `--resume-work`; `--vars` requires `--brief-file`.

The daemon session controls accept either a positional session id or the
existing `--session` spelling: `session snapshot [SESSION]`, `session attach
[SESSION]`, and `session wait [SESSION]`. `snapshot` emits the raw ANSI screen
by default, or plain rows with `--text` and geometry plus base64 ANSI with
`--json`; `attach` streams live output until Ctrl-C without stopping the
session. `wait` accepts
`--until exited|idle|blocked|done|match:<regex>|screen:<regex>|stable:<ms>` and
optional `--timeout <ms>`; a timeout exits 2. `session send` types text, or with
`--key <KEY>` (repeatable, at most 64) presses named keys such as `up`, `esc`,
`f5` or `ctrl+c`; `--paste` sends the text as a paste and cannot be combined
with `--key`.

`session fork <id>` asks the daemon to start a new PTY from a live session.
Use `--scrollback` for a bounded handoff, `--tab` for a new tab, and
`--fork-worktree` to create a separate worktree first. `--json` includes the
child's `forked_from` lineage without exposing the launch recipe.

`session open --stage --chunk`, `dispatch claim --chunk`, and
`dispatch put --chunk <file>` record the chunk a row dispatches under through
one atomic admission decision. The file's `files:` frontmatter (globs: `*`
within a segment, `**` across) is checked against every active sibling scope
for that issue and worktree;
overlaps and unmet `after:` dependencies refuse without leaving a row. Active
siblings whose scope cannot be read or parsed also refuse with the row and path
named. `dispatch claim --allow-scope-overlap <reason>` and
`dispatch put --force --force-reason <reason>` are explicit, audited overrides;
`session open` has no override. A `dispatch put` row recorded without
`--session` names no worker, so a pane exit never stamps it (a daemon session id
that matches no row is stale); close it with `dispatch set-status`. Pass
`--session <id>` to give the row an identity. Scope display:
`dispatch list` carries a `chunk` column (the file's basename), and JSON rows
carry `chunk_path` plus `chunk_files_from_file_at_display_time` (the parsed
`files:` list read at display time, not a claim of admitted ownership).

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

### thegn info

`thegn info` explains the effective workspace or worktree settings and the
layer that supplied each value. Use `--workspace` or `--worktree` to inspect a
different target, or `--json` for the stable machine-readable report.

```sh
thegn info [--workspace SLUG|PATH] [--worktree PATH|BRANCH] [--json]
```

The JSON document has top-level keys such as `schema`, `target`, `workspace`,
`workspace_stats`, `worktree`, `sessions`, `settings`, `backends`, `overrides`,
and `notes`.

## Headless worktree lifecycle

```sh
wt=$(thegn wt new fix-parser --repo ~/code/app)   # prints the path only
cd "$wt"
thegn wt rm fix-parser --force                    # teardown + git + DB
```

`wt new` reuses the TUI wizard's pipeline (branch naming templates, base
resolution, the serial git-mutation lock, DB registration) but never
provisions a sandbox — the compositor prepares lazily on first open.
Use `--isolation standard` or `--isolation sealed` to override the worktree's
inherited sandbox profile. The worktree stores a pin only when the choice
differs from its effective default — the wizard's **Isolation** row cycles
`standard` and `sealed`, so choosing the effective default stores no pin.
Inspect it with `thegn sandbox profile show`; change it with
`thegn sandbox profile set <standard|sealed>` or remove the override with
`thegn sandbox profile clear` to inherit again. Each profile command accepts
`--worktree <path>`.
`wt rm` runs provider/sandbox teardown synchronously and refuses to delete
when it cannot confirm the sandbox is gone (it names the sandbox and the two
ways out; see the environments help page), then
`git worktree remove`, then cleans every DB row (including tab groups, so a
removed worktree is never resurrected at the next launch).

`wt new --batch FILE` creates one lane for each non-comment, non-blank line in
the file. Each line is `<name>` or `<name> <base>`; a per-line base overrides
the shared `--base`. The complete file is validated before the first checkout.
The batch form accepts the usual `--repo`, `--env`, `--isolation`,
`--exact-branch`, `--folder` and `--json` options.

## Build lanes (`wt disk --lanes`)

```sh
thegn wt disk --lanes [--worktree PATH] [--json]
thegn wt disk --lanes [--worktree PATH] --apply [--json]
```

Build lanes are the coordinator's per-lane build sandboxes: the immediate
subdirectories of a checkout's `target/lanes/` — the 129 GiB the repository
hygiene audit found with no owner attached. `--lanes` inventories exactly one
checkout's lanes, largest first. Without `--apply` it is a report: it removes
nothing, rewrites nothing, does not touch the `worktree_disk` cache, and visits
no other worktree. `--all` is a usage error here, because a lane inventory is
scoped to one checkout.

Each lane is measured with the same apparent-byte walk as `thegn disk`, so the
two surfaces agree byte for byte. `BYTES` is the whole lane directory and
`BUILD` is the regenerable output inside it: a lane that is a copy of the
repository contributes its `target/` subtree, a directory that is itself a
Cargo target dir contributes itself, and anything else contributes nothing.

Classification is evidence-based, and every row prints its reason:

- **Owner** — the lane's own git checkout (`HEAD`'s branch), read from a `.git`
  directory or a linked worktree's `gitdir:` file. A directory with no checkout
  of its own is reported as `owner unknown`; it is never attributed to the
  checkout that happens to contain it, because ownership is what makes a lane
  reclaimable. The owner field is the lane's own checkout path (the same as
  `path`), not a coordinating job: no job attribution is recorded. The checkout
  must resolve: a `gitdir:` file pointing at a missing directory, or a git dir
  with no readable `HEAD`, is an orphan and reports `owner unknown`. Symlinked
  lane entries are listed as unknown and never followed.
- **Activity** — Cargo's per-profile `.cargo-lock` first (a `try_lock` that
  fails means a build is running; the probe never waits and releases
  immediately), then the newest write anywhere in the lane, compared against the
  same one-hour recency window the low-disk rule uses.
- **Eligibility** — `eligible` only for a build lane with a verified owner, no
  held lock, and no write inside that window. A held lock or a recent write is
  `held`; unknown ownership, an unreadable lane, or a non-build directory is
  `unknown`. Only `eligible` lanes contribute to the eligible total.

The report ends with lane count, total bytes, build output, eligible bytes and
the scan duration. `--json` emits one object with the same fields; each `lanes`
row carries `lane`, `path`, `bytes`, `build_bytes`, `kind`, `owner`, `branch`,
`activity`, `activity_evidence`, `eligibility` and `reason`.

### Reclaiming eligible lanes (`--apply`)

`--apply` reclaims the build output of the lanes the inventory classified
`eligible`. Review the dry run first: it lists every candidate, its estimated
`BUILD` bytes, and the reason each other lane is held or unknown.

Each candidate is re-inventoried immediately before anything is removed. The
lane directory's identity, its kind, the verified owner and branch, Cargo's
`.cargo-lock` probe, the newest write anywhere in the lane, and the measured
bytes must all still match the dry run; if any of them changed — the lane
started building, took another branch, or was replaced — the lane is left
touched and the new reason is printed. Ambiguous lanes are never reclaimed.

Removal takes only the regenerable output. A checkout lane keeps its `.git`,
`Cargo.toml` and sources; only its `target/` subtree goes. A lane that is itself
a lane that is itself a bare Cargo target dir is never removed: it has no
ownership proof yet (a follow-up), so it is reported `unknown`. The Cargo build
locks are held across each removal, and any failed or partial removal makes the
command exit non-zero. Measuring the next comparable build time is a manual
operator step; nothing measures rebuild cost.
Nothing outside the selected lane's output is touched: the checkout being
scanned, other lanes, ordinary `target/` output, and the lane's git evidence all
stay.

The apply report lists each removed lane with the apparent bytes removed, every
lane left untouched with its reason, the total reclaimed, and the cleanup
duration. `--json` emits the dry-run `report` object plus `reclaimed_bytes`,
`removed` (`lane`, `path`, `bytes`), `skipped` (`lane`, `eligibility`, `reason`)
and `apply_duration_ms`; a skipped lane's `eligibility` is its report class, or
`retained` when the apply's re-check kept a lane the report had called eligible.
Removal is per lane, so a partial run is retryable: the lanes it could not
remove stay eligible for the next `--apply`.

Cleanup is opt-in and never runs automatically; to stop future cleanup, stop
passing `--apply`. After a real apply, record the `reclaimed_bytes` and skipped
lanes from the report together with the elapsed time of the next comparable
build in the issue handoff, so rebuild cost sits beside the recovered bytes.

## Vercel orphan reap (`env reap`)

`thegn env reap --provider vercel [--apply] [--yes] [--json]` finds Vercel
sandboxes that carry this controller's tag but have no ledger record. It is a
dry run by default and prints names, ages, tags and a cost estimate.
`--apply` deletes only sandboxes that pass the tag proof, asks you to type
`yes` on a tty unless `--yes` is given, and refuses with no tty and no `--yes`.
A partial or unknown listing refuses `--apply`. It never runs automatically.
See [Vercel sandboxes](help/vercel-sandbox.md).

`thegn env reap --provider sprites [--apply | --unrecorded] [--yes] [--json]` is the Sprites
counterpart (`--unrecorded` is a read-only listing that refuses `--apply`). It lists orphans, sprites past `max_lifetime_secs` and sprites
"not recorded by this host", and `--apply` destroys only sprites the local
ledger recorded (never an unrecorded one). `thegn agent run --wait --pull` runs
`env pull` for the worktree after the agent exits. See
[Environments](help/environments.md).

`thegn env adopt <sprite> --env <S> [--yes] [--json]` records an existing
sprite in the ledger after showing its name, status, `created_at`, this host's
controller id and the account fingerprint; type the sprite name back on a tty,
or pass `--yes`. It never changes the sprite. `thegn land --release-sandbox`
destroys a provider worktree's proven sandbox after a successful land (the
default keeps it). `thegn agent run --wait --keep-running` skips the stop of a
Fly or machine0 machine after the run. See [Environments](help/environments.md).

## Filling the warm pool (`env pool-fill`)

`thegn env pool-fill [--worktree W] [--size N] [--json]` provisions the missing
warm spares for a worktree's project and provider env, one at a time, in the
foreground. `--size N` (0 to 8) first saves the per-project pool target, the
same setting as the TUI's warm-pool keys. The command never destroys anything:
`--size 0` only turns the pool off, and the TUI drains extra spares. It refuses
Vercel envs (their pool target is always 0) and envs that are not provider
envs, and it stops with a retry message while the TUI's own reconcile holds the
pool lease. A claim made from a CLI or daemon process does not refill the pool;
it prints a hint to run this verb. See [Environments](help/environments.md).

## Retrying a failed pull-back (`env sync-retry`)

`thegn env sync-retry [--json]` retries the pull-back for every journal entry in
state `Failed` whose worktree directory exists. It never destroys a sandbox, and
exits nonzero while any entry still fails. A failed pull-back on close shows a
warning that names this verb.

## Inspecting a sprite (`fleet inspect`)

`thegn fleet inspect <sprite> --env <S> [--policy] [--json]` (dev channel) reads
one sprite's status, `created_at` and URL auth without waking it. `--policy`
also reads the memory policy, which may wake a cold sprite; errors never print
the request URL.

## Warming the devShell cache (`sandbox warm-devshell`)

`thegn sandbox warm-devshell [--json]` runs `nix print-dev-env --json` in the
current repository's main checkout and writes the cache read by
`[sandbox] inject_devshell` (0600 file, 0700 directory). It evaluates the
flake as you; nothing runs it automatically.

## Provisioning in `agent run --json`

`thegn agent run --json` carries `provision`: `none`, `claimed_spare` or
`provisioned`.

## Forgetting projects (`repo forget`)

`thegn repo forget [--keep <path>] [--copy-from <DB>] [--apply] [--json] [paths...]`
removes project rows from this instance's database only; files, worktrees and
branches are untouched. `--copy-from <DB>` first seeds this instance's empty
state dir from another instance's database (SQLite backup, verified), then
forgets. It refuses when this instance already has a `thegn.db`, when the
schema differs, or when source and destination are the same file; the source is
never written. See [Running two instances](help/two-instances.md).

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
`repo list`, `repo recent`, `repo provenance`, `env list`, `host list`,
`ci runs`, `share list`, `forward list`, `merge list`, `session list`,
`pair list`, `disk` (and `wt disk --lanes`, which emits the lane inventory as
one object), `map`,
`dispatch list` (the agent-dispatch roster; `--active` keeps only rows that
occupy a slot; rows carry the daemon-written retry `note` — headless workers
that died of a transport failure are relaunched by the daemon per
`[pipeline.transport_retry]`, every outcome parked `waiting_human`), `agent list` (`{agents, stages}` — the effective harness /
model / env keys / permission count of every entry and pipeline stage), and
`wt new --json` (`{branch, path, root, base}`). Treat the shapes as a stable
API. (Two pre-existing surfaces keep their historical shapes: `notify list
--json` is NDJSON, `doctor --json` is one object.) A few list surfaces are
text-only today — `zone list`, `mcp list`, and `theme list` have no `--json`.

`merge list --json` rows add a `claim` object (`null` when unclaimed) and a
`landed` object for landed rows, with `source_oid`, `target_base_oid` and
`result_oid` (`null` when not landed). Older landings carry only recorded
values; existing fields keep their shapes.

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

## Direct Iroh control pairing

On the server, start the foreground control service with its separate direct
Iroh listener enabled:

```sh
thegn serve --iroh-direct --no-pair-url
```

The listener prints its stable peer ID and concrete IP socket addresses. The
peers need a working direct IP path (for example, the same LAN with the
relevant UDP traffic allowed). This mode does not use Tailscale, a relay,
endpoint lookup, or the sandbox `ExecReq` transport. Existing HTTP pairing is
unchanged.

On the server, use the printed peer ID and addresses to create a protected
invitation file. This command mints the existing scoped one-time code and
writes it directly to an owner-only file:

```sh
thegn pair direct new --endpoint-id <peer-id> --address 192.0.2.7:12345 \\
  --scope read --out invite.json
```

On the receiving peer, redeem the invitation from a file (or pipe it to stdin)
and verify the saved profile with an authenticated read request:

```sh
thegn pair direct redeem home --invitation-file invite.json
thegn pair direct me home
```

The bearer is stored through Thegn's OS keyring, with its protected secret-file
fallback; it is not printed or accepted in argv. Profile metadata is kept in a
private state directory. The client pins the invited peer ID and uses only the
explicit IP addresses. Requests use the existing scoped token and revocation
state. Revoke it on the server with `thegn pair revoke <pairing-id>`. This
experimental surface requires the dev release channel.

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

Beyond the docs tools, `--scopes` (comma-separated `read,write,git,exec,admin`,
default `read`) gates a set of **state tools** that talk to a running pane
daemon — default-deny: a tool neither appears in `tools/list` nor is callable
until its scope is granted.

| Tool                        | Scope         | What it does                                                              |
| --------------------------- | ------------- | ------------------------------------------------------------------------- |
| `sessions_list`             | `read`        | list the daemon's live sessions                                           |
| `worktrees_list`            | `read`        | list registered worktrees (daemon, else DB cache)                         |
| `worktrees_sandbox_profile` | `read`        | a worktree's pinned sandbox profile (null = inherit configured policy)    |
| `leases_list`               | `read`        | relay lease state per session                                             |
| `me`                        | `read`        | the caller's pairing id, label, granted scopes                            |
| `fleet_list`                | `read`        | provider fleet inventory, agents joined by worktree, and estimated costs  |
| `env_status`                | `read`        | a worktree's remote sandbox health: state, lifetime, cost, one probe      |
| `sessions_wait`             | `read`        | block until a session reaches a state (exited/idle/blocked/done/regex)    |
| `sessions_snapshot`         | `read`        | a live session's current emulator screen (`session`)                      |
| `semantic_map`              | `read`        | ranked, budgeted repo map of a worktree's indexed entities                |
| `semantic_blast_radius`     | `read`        | a worktree's blast-radius: changed entities, callers, untested, risk      |
| `git_status`                | `read`        | list changed files in a worktree                                          |
| `merge_list`                | `read`        | list the merge queue for a worktree's repository                          |
| `pr_status`                 | `read`        | read cached pull-request status, one row per worktree                     |
| `calendar_events`           | `read`        | merged calendar events in an inclusive ISO-date window                    |
| `calendar_clocks`           | `read`        | resolved home and world clocks                                            |
| `issues_list`               | `read`        | list tracker issues (status/limit/project/query, registered repo)         |
| `issues_get`                | `read`        | one tracker issue with detail and comments                                |
| `dispatches_list`           | `read`        | the agent-dispatch roster, newest first (limit/before paging)             |
| `merge_add`                 | `git`         | enqueue a worktree's branch on the merge queue                            |
| `notify_push`               | `write`       | push a tray notification (title, optional body/urgency/source)            |
| `sessions_open`             | `write`       | open a session — raw `argv`, or a configured agent by name                |
| `sessions_kill`             | `write`       | kill a session's process (idempotent)                                     |
| `sessions_resize`           | `write`       | resize a live session's PTY (`session`, `rows`, `cols`)                   |
| `sessions_split`            | `write`       | split a session: sibling pane (`session`, `dir`, `argv`, `cwd`, `env`)    |
| `worktrees_open`            | `write`       | focus a workspace in the running instance (`repo`, optional `branch`)     |
| `dispatches_put`            | `write`       | append a roster row (`issue_id`, `worktree_path`, `agent_name`, ...)      |
| `dispatches_set_status`     | `write`       | set a roster row's status (`id`, `status`, optional `lead` proof)         |
| `agent_dispatch`            | `write`       | launch a configured agent in a worktree and track its dispatch row        |
| `issues_update`             | `write`       | patch an issue (`id`, optional `status`, `assignee`, `priority`, `title`) |
| `issues_comment`            | `write`       | comment on an issue (`id`, `body` up to 64 KiB)                           |
| `sessions_input`            | `write` **+** | send raw terminal input/control characters to a live session              |

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
launch environment values are replaced by a size descriptor, and free text (an
issue comment body, a dispatch task) by a length-and-digest marker, never logged
verbatim.

Every tool in `tools/list` carries MCP `annotations`: the docs tools and the
`read`-scope state tools (including `sessions_snapshot`) are `readOnlyHint: true`; `sessions_kill`,
`sessions_input`, `agent_dispatch`, `dispatches_set_status`, `issues_update` and `tools_run` are `destructiveHint: true`; `folders_assign`,
`editor_open`, `merge_add`, `notify_push`, `sessions_open`, `sessions_fork`,
`sessions_resize`, `sessions_split`, `worktrees_open`, `dispatches_put` and `issues_comment` are write tools marked `destructiveHint: false`. A new write tool is marked destructive until it is
reviewed. The hints help clients ask before acting; the scopes above are what
thegn enforces.

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
budget (`THEGN_COMPLETE_BUDGET_MS`, default 300), and any failure completes
nothing rather than printing an error. Branch, PR and issue completion are
deliberately **not** offered: a `<TAB>` does not run git and does not call the
forge.

`thegn completions <shell> --static` emits a self-contained script instead —
structure only, no live values. It is the fallback if the dynamic mechanism is
ever unavailable.
