# CLI

## Purpose

thegn's command-line surface exposes noun-verb namespaces (`wt`, `repo`,
`merge`, etc.) that drive the worktree lifecycle and workspace focus headlessly,
reusing the same core pipelines as the TUI. It keeps legacy bare verbs working
(hidden from help), emits machine-readable `--json` on list-shaped reads under a
documented exit-code contract, groups top-level help semantically, and generates
shell completions from the live clap definition.

## Requirements

### Requirement: The worktree lifecycle is drivable headlessly through a `wt` namespace

thegn SHALL expose a `wt` noun-verb namespace (`wt list|new|rm|diff|disk|clean`)
whose `new` and `rm` verbs create and remove worktrees headlessly, reusing the
same core pipeline as the TUI wizard (branch naming, base resolution, git
worktree add/remove under the serial git-mutation lock, DB registration and
cleanup). `wt new` MUST NOT provision sandboxes (the compositor prepares
lazily), MUST print the created worktree's absolute path as its only plain
output, and MUST roll back the git worktree if registration fails. `wt rm`
MUST refuse to remove a main worktree, MUST prompt unless `--force`, and MUST
clean the worktree's DB rows including its tab-group rows so a removed
worktree is never resurrected at next launch.

#### Scenario: Headless creation

- **WHEN** `thegn wt new fix-parser --repo <root>` runs with no TUI
- **THEN** a new branch + worktree exist under the configured worktrees dir,
  the worktree is registered in the DB, and stdout is exactly the new path

#### Scenario: Removal cleans resurrection state

- **WHEN** `thegn wt rm <path> --force` completes
- **THEN** the checkout is gone, the branch remains (absent `--delete-branch`),
  and no worktree/tab-group rows for it remain in the DB

#### Scenario: Unknown target

- **WHEN** `wt rm` is given a target matching no known worktree or branch
- **THEN** it lists candidates and exits with code 3

### Requirement: Legacy bare verbs remain functional but hidden

thegn SHALL keep the legacy top-level verbs `list`, `diff`, `disk`,
`clean`, `repos`, and `recent` working with output byte-identical to their namespaced
equivalents (`wt …`, `repo …`), and SHALL be hidden from `--help`. Their flags
MUST be shared definitions with the namespaced forms so the two spellings
cannot drift.

#### Scenario: Old scripts keep working

- **WHEN** `thegn list` runs after the namespaces land
- **THEN** its output is byte-identical to `thegn wt list` and no deprecation
  breaks the invocation

### Requirement: List-shaped read commands emit machine-readable JSON

thegn SHALL accept `--json` on every list-shaped read surface (`wt
list`/`list`, `env list`, `host list`, `ci runs`, `share list`, `forward
list`, `disk`) and
emit a single compact JSON document on stdout with no ANSI sequences. The CLI
SHALL honor a documented exit-code contract: 0 success, 1 error, 2
transient/retryable, 3 target not found.

#### Scenario: JSON is parseable

- **WHEN** `thegn wt list --json` runs
- **THEN** stdout parses as one JSON array and contains no escape sequences

#### Scenario: Scripts can branch on exit codes

- **WHEN** `thegn open no-such-repo --no-launch` fails to resolve
- **THEN** the process exits with code 3

### Requirement: Top-level help renders commands in semantic groups

`thegn --help` SHALL render non-hidden commands grouped (Workspace, Forge,
Environments, Session, Meta) with names and descriptions sourced from the live
clap definitions. A unit test MUST fail when a non-hidden command is not
assigned to exactly one group. Subcommand help (`thegn wt --help`) MUST be
unaffected by the grouping template.

#### Scenario: Grouped help

- **WHEN** `thegn --help` is rendered
- **THEN** the Workspace and Forge headings appear and hidden commands do not

#### Scenario: Ungrouped command fails CI

- **WHEN** a new visible top-level command is added without a group assignment
- **THEN** the drift-guard unit test fails

### Requirement: Shell completions are generated from the CLI definition

thegn SHALL provide `completions <shell>` generating shell completions from the
live clap definition, using the invoked binary name (thegn / tg) as the
completion target, so one binary serves both names with no special casing.

The **default** output SHALL be a registration script that resolves candidates
by calling back into the binary at completion time: it contains no command
names, so the completions it produces cannot go stale against the installed
binary, and it costs nothing at shell startup because the shell loads it lazily
on the first completion for that command.

`--static` SHALL emit a self-contained script generated from the command tree
instead — structure only, no live values — as the documented degradation path if
the dynamic mechanism is unavailable. Both forms MUST be buffered before being
written, so a consumer closing the pipe early is not a panic.

#### Scenario: Bash completions

- **WHEN** `thegn completions bash` runs
- **THEN** a completion script for the invoked binary name is written to stdout

#### Scenario: The default output asks the binary rather than naming commands

- **WHEN** `thegn completions zsh` runs
- **THEN** the emitted script registers a completion for `thegn` that invokes
  `thegn` at completion time, and contains no subcommand names

#### Scenario: The registration invokes the command by name

- **WHEN** a completion script is generated by a packager, in a build sandbox or
  a temporary directory
- **THEN** the script invokes the command by its plain name, resolved through
  `PATH` — never by the absolute path the script was generated from

#### Scenario: `--static` still emits a self-contained script

- **WHEN** `thegn completions zsh --static` runs
- **THEN** a self-contained completion script for the invoked binary name is
  written to stdout, covering the command tree without invoking the binary

### Requirement: `open <repo>` remote-controls or launches the compositor

`thegn open <repo>` SHALL resolve its argument (path, or unique basename/slug
match against known repos), and: when a live instance holds the profile
singleton lock, enqueue a `focus_workspace` intent in the DB `intents` mailbox
(consumed by the compositor's model refresh, claim-and-delete, last intent
wins); otherwise set the active-workspace pointer and launch the compositor,
which lands on that workspace via the existing startup resolution. Intent
consumption MUST tolerate a DB missing the `intents` table. Resolution misses
MUST list candidates and exit 3.

#### Scenario: Focus a running instance

- **WHEN** `thegn open myrepo` runs while a compositor is running
- **THEN** an intent row is enqueued and the running instance switches to that
  workspace within approximately one model-refresh tick

#### Scenario: Launch focused

- **WHEN** `thegn open myrepo` runs with no live instance
- **THEN** the active-workspace pointer is set and the compositor launches on
  that workspace

#### Scenario: Older DB without the mailbox

- **WHEN** the compositor hydrates against a DB lacking the `intents` table
- **THEN** hydration proceeds normally with no intents consumed

### Requirement: Machine-readable output goes through one emitter

`--json` output SHALL be printed through `cmd::emit_json` (one compact document per invocation); files that print JSON any other way are pinned in `test/json-emit-ratchet.txt` (shrink-only) until routed through it or through a deliberate pretty emitter.

#### Scenario: New hand-rolled JSON

- **WHEN** a command prints `serde_json::to_string_pretty(..)` directly and its file is not pinned
- **THEN** `just lint`'s `json-emit` ratchet fails

### Requirement: CLI worktree creation uses the shared submodule initializer

After `wt new` creates a checkout, it SHALL invoke the same `[git].submodules`
post-checkout initializer as UI and remote creation. In `auto`, initialization
SHALL run only for valid metadata and after the repository's current submodule
request is approved; in `off`, it SHALL be skipped. Failure SHALL be surfaced
without deleting the successfully-created worktree.

#### Scenario: Initialization is pending approval

- **WHEN** `wt new` creates a worktree whose repo declares submodule URLs not
  yet approved
- **THEN** the worktree remains created, init does not run, and the CLI reports
  the pending trust action

### Requirement: Agent orchestration is drivable from the CLI

thegn SHALL expose the orchestration loop headlessly: `session open` launches a
configured agent by name into a worktree (prompt, headless/interactive, and
worktree-binding flags mirroring the control-plane launch), `wt new
--from-issue <id>` creates and links a worktree from a tracker issue,
`dispatch list` and `dispatch set-status` read and advance the durable roster,
and `issue list` accepts status and limit filters. List-shaped reads MUST emit
`--json` through the one-emitter convention under the documented exit-code
contract, so a supervisor can drive the whole loop with no MCP transport.

#### Scenario: Opening a worker headlessly

- **WHEN** `thegn session open --agent claude --prompt <p> --worktree <w>
--headless` runs against the daemon
- **THEN** the agent launches through the same composition as a TUI launch and
  the session id is printed (JSON when requested)

#### Scenario: The roster is scriptable

- **WHEN** `thegn dispatch list --json` runs
- **THEN** every dispatch row is emitted with its issue, worktree, agent, and
  parseable status

#### Scenario: Filtering issues for the next batch

- **WHEN** `thegn issue list --status todo --limit 3 --json` runs
- **THEN** at most three issues with the requested status are emitted,
  machine-readable

### Requirement: The roster is writable from the CLI, pipeline columns included

The CLI SHALL append a dispatch row — issue, worktree, agent — and SHALL accept
the pipeline fields (stage, parent row, session, artifact path) on that same
command, so recording a pipeline dispatch needs no second verb and no HTTP
transport. A parent that names no existing row MUST be rejected before anything
is written. The command SHALL support machine-readable output under the CLI's
one-document `--json` convention, and the human roster listing SHALL show each
row's stage and parent.

#### Scenario: Recording a chunk dispatch

- **WHEN** `thegn dispatch put <issue> <worktree> <agent> --stage code --parent
<id> --session <s> --artifact <p> --json` runs
- **THEN** one row is appended and emitted with its new id, its queued status,
  and all four pipeline fields

#### Scenario: Atomic admission of overlapping chunk claims

- **WHEN** two chunk claims with overlapping `files:` scopes contend for a
  stage with capacity for both
- **THEN** the serialized admission grants at most one row, the other receives
  a retryable refusal, and no refused row is left queued

#### Scenario: A forced overlap is auditable

- **WHEN** `thegn dispatch claim --chunk <file> --allow-scope-overlap <reason>`
  admits an overlapping chunk
- **THEN** the non-empty reason is recorded on the row and is visible in the
  claim output and roster JSON

#### Scenario: A parent that does not exist

- **WHEN** `thegn dispatch put … --parent <unknown-id>` runs
- **THEN** the command fails naming that id, and no row is written

#### Scenario: Listing a mixed roster

- **WHEN** `thegn dispatch list` runs over a roster holding both pipeline and
  plain dispatches
- **THEN** each row shows its stage and parent, with absent values rendered as a
  placeholder so the table stays aligned

### Requirement: A CLI-launched agent can be adopted into a pane

`thegn session open` SHALL accept a flag asking a running compositor to graft the
new session into a real pane, instead of leaving it headless. The flag SHALL
default to off, so a fan-out never takes over the user's screen unasked, and
requesting it MUST remain a nudge rather than a dependency: with no compositor
running the session still opens and stays headless.

#### Scenario: Opening a watchable stage agent

- **WHEN** `thegn session open --agent <a> --worktree <w> --adopt` runs against
  the daemon
- **THEN** the session opens and the request to graft it into a pane is recorded
  for the compositor

#### Scenario: Opening with no compositor attached

- **WHEN** the same command runs with no compositor attached
- **THEN** the session still opens headless and the command succeeds

### Requirement: Session open dispatches a pipeline stage

`thegn session open` SHALL accept `--stage <name> --issue <id>`, which selects
a configured `[[pipeline.stages]]` entry and performs one atomic stage dispatch
(insert the row, render the prompt from the stage's template, refuse an empty
render, open the session — the daemon launch layers the stage's overrides —
stamp the row, mark it running) as specified in the agent spec. `--stage`
WITHOUT `--issue` stays THE-83's overlay open: a plain launch with the stage's
`model` / `env` / `permissions` layered over the agent entry. An explicit
`--prompt` SHALL be refused on the dispatch form (the template owns the task)
and SHALL be honoured on the overlay form. The verb SHALL print the row id,
session id and artifact path it produced, and on failure SHALL name the roster
row it left `failed`.

#### Scenario: A dispatched stage prints its handles

- **WHEN** `session open --stage <name>` succeeds
- **THEN** the output names the roster row id, the daemon session id and the
  sanitized artifact path, and the row is readable back with those values

#### Scenario: A stage whose prompt renders empty is refused

- **WHEN** the stage's rendered prompt is empty
- **THEN** the verb exits non-zero naming the stage, opens no session, and
  leaves no running row

### Requirement: The roster's done outcome is gated at the CLI

`thegn dispatch set-status <id> done` SHALL verify the row's artifact through
the run-completion contract (exists under the worktree, tracked by git) before
writing `done`, SHALL print the refusal reasons verbatim on failure, SHALL
report an uncommitted-worktree state without blocking on it, and SHALL leave
`failed`/`abandoned`/`merged` and artifact-less rows ungated. Every other
status SHALL behave exactly as before.

#### Scenario: An uncommitted artifact refuses done

- **WHEN** a roster row's artifact is written but not committed
- **THEN** `set-status done` exits non-zero and names the artifact with the
  reason to commit it

#### Scenario: A no-artifact row is set done as before

- **WHEN** a row carries no artifact
- **THEN** `set-status done` is not gated and behaves exactly as it did before
  this change

### Requirement: Dispatch verification is a read-only CLI verb

`thegn dispatch verify <id>` SHALL report the run-completion verdict for one
roster row — ok, the artifact path, exists / tracked / dirty flags and the
refusal reasons — without mutating the row, and SHALL be drivable in JSON for a
supervisor's consumption.

#### Scenario: A supervisor checks a claim before recording it

- **WHEN** `dispatch verify` runs against a row whose artifact is missing or
  untracked
- **THEN** the verdict is not-ok with the same reasons the gated
  `set-status done` would print, and the row is unchanged

### Requirement: Dispatch wait blocks on live workers

`thegn dispatch wait` SHALL block until the exit of the session behind one
explicit roster row, or — with `--any` — of every current spawning/running row
that carries a session, composing the routed `sessions.wait` primitive with its
timeout semantics. Selection and refusals SHALL follow the wake primitive's
named errors (no such row / not spawning-running / no session / nothing
active), printed verbatim.

#### Scenario: A wait on a live worker returns at its exit

- **WHEN** `dispatch wait <id>` targets a spawning/running row with a session
- **THEN** the verb blocks in its own process until that session exits and
  reports the row it woke for (id, stage, issue)

#### Scenario: A wait with nothing live refuses honestly

- **WHEN** `dispatch wait --any` finds no spawning/running row with a session
- **THEN** the verb exits non-zero with the nothing-active message rather than
  waiting forever or returning instantly against a parked row

### Requirement: Session liveness is visible and closable by name

`thegn session list` SHALL surface each session's liveness — including exited
sessions with their exit time, exit code and final state from the daemon's
tombstones — and `thegn session list --live` SHALL restrict the listing to
live sessions. `thegn session close <id>` SHALL close a session by name over
the existing `sessions.kill` control capability (no new capability row), so a
supervisor never has to hand-assemble a JSON body against the control plane.

#### Scenario: An exited session is reported, not vanished

- **WHEN** a worker's session exits and the supervisor lists sessions
- **THEN** the exited session still appears with its exit time and exit code
  (tombstone-backed), and `--live` filters it out

#### Scenario: Closing a session is one named verb

- **WHEN** a supervisor wants a worker stopped
- **THEN** `thegn session close <id>` ends that session without the caller
  writing raw control-plane JSON

### Requirement: Completions are installed by the packager, not the user's shell rc

A packaged install SHALL place completion files for **both** installed command
names (the channel binary and its short alias) into the platform completion
directories for bash, zsh and fish, generating each script through the name it
targets. A build that cannot execute the binary it just built (cross
compilation) MUST skip completion generation rather than fail the build.
Releases SHALL additionally publish the generated scripts as a single
arch-independent asset, since the command tree does not vary by target.

The documented install instructions MUST NOT require an rc-file `eval` (the
`eval "$(thegn completions zsh)"` pattern), because thegn spawns a shell per
pane and a warm reattach restores many panes at once — an rc-file `eval` would
add a `thegn` process launch to every pane restore. Documentation SHALL instead
give, per shell, the single command that writes the file to that shell's
completion directory, for installs no packager owns.

#### Scenario: A packaged install completes both names

- **WHEN** the package is built on a platform that can execute the host binary
- **THEN** completion files for the channel binary **and** its short alias are
  installed for bash, zsh and fish, and each script targets the command name it
  is installed under

#### Scenario: A cross build skips completions rather than failing

- **WHEN** the package is built for a host platform the build platform cannot
  execute
- **THEN** the build succeeds with no completion files installed

#### Scenario: A release ships completions once

- **WHEN** a release is published
- **THEN** a single arch-independent completions asset is attached, covering
  both command names, rather than a copy inside each per-target archive

### Requirement: A completion request is bounded and never mutates state

Serving a completion is a process launch on a keypress. The completion path
SHALL be dispatched before the normal subcommand path and MUST NOT load the
layered configuration, open the state database read-write, run migrations,
install logging, or publish sandbox policy. When the process is not serving a
completion, the check MUST cost no more than a single environment read.

A completion request:

- MUST NOT create, migrate, or write to the state database, and MUST NOT create
  its directories — reading state SHALL use a read-only open with a short busy
  timeout;
- MUST NOT perform network or forge I/O, and MUST NOT run subprocesses;
- MUST exit 0 with no output on any error, timeout, missing state, or panic, so
  the shell falls back to its own filename completion;
- MUST NOT print warnings, errors, backtraces or crash notices on any stream;
- MUST honour `--profile <name>` given on the command line being completed, so
  candidates come from that profile's state rather than another's;
- MUST evaluate value sources lazily and independently — a request that needs
  only command structure reads neither the database nor the configuration;
- SHALL be bounded by a deadline (configurable, defaulting to a fraction of a
  second) checked between sources; on expiry it emits what it has gathered and
  stops.

#### Scenario: Completing on a machine that has never run thegn

- **WHEN** a completion is requested with an empty state directory
- **THEN** the process exits 0 and the directory is still empty

#### Scenario: A completion never speaks out of turn

- **WHEN** a completion is requested against unreadable, unmigrated or missing
  state
- **THEN** nothing is written to stdout or stderr, the exit status is 0, and
  command structure still completes

#### Scenario: An exhausted budget completes nothing, quietly

- **WHEN** a completion is requested with the deadline set below the cost of its
  sources
- **THEN** the process exits 0 having printed no error, no partial line and no
  diagnostic

#### Scenario: A profile-scoped completion reads that profile

- **WHEN** a completion is requested for a command line carrying
  `--profile <name>`
- **THEN** the candidates come from that profile's state, not the default
  profile's

### Requirement: Completion value sources are implemented or reserved

Each value-taking slot in the CLI tree — identified by its command path and
argument id — SHALL be bound to a value source, declared structural (completed
by the command tree itself), or pinned in a shrink-only ratchet file with the
gap recorded. A drift check over the live command tree MUST fail when a new slot
is neither bound, declared, nor pinned, and MUST also fail on a pin for a slot
that no longer exists.

Every value source kind SHALL be either implemented or `reserved`, and a
`reserved` kind SHALL carry the reason it is not served. Kinds whose values
would require git or network I/O — branch, pull request and issue candidates —
SHALL be `reserved`, because the fast-path contract forbids that work.
`thegn doctor` SHALL project those declarations — how many kinds are served, and
every `reserved` kind with its reason — so that a value which deliberately does
not complete is visible as a decision rather than as a defect.

Candidate output SHALL be de-duplicated, prefix-filtered and capped, and values
that cannot be represented in the line-oriented shell protocols MUST be dropped
rather than emitted, since such a value would split one candidate into two
rather than merely render badly.

#### Scenario: A new uncompletable slot fails the drift check

- **WHEN** a new value-taking argument is added to the CLI tree without a source
  binding, a structural declaration, or a ratchet pin
- **THEN** the completion slot drift check fails

#### Scenario: A `<TAB>` never calls the forge

- **WHEN** a completion is requested for a slot that takes a pull request or an
  issue
- **THEN** no forge or network call is made and the slot completes nothing

#### Scenario: Doctor names the reserved value sources

- **WHEN** `thegn doctor` runs
- **THEN** the completions section reports the number of value sources served
  and lists each reserved kind with the reason it is not served

### Requirement: Completion freshness is diagnosable

`thegn doctor` SHALL report, per shell and per installed command name, where
completion files are found and whether they are current: `fresh` when the
installed file matches what the running binary generates, `stale` when it
differs, `dynamic` when it is a registration script (which asks the binary and
therefore cannot go stale), and `absent` when no file is found. A `stale` or
`absent` entry SHALL print the exact command that fixes it, with the destination
path filled in. Detection MUST compare content, never modification times, since
packaged installs normalise timestamps. The same data SHALL be available in
`thegn doctor --json`. Missing or unreadable completion files MUST be reported,
never treated as a doctor failure.

#### Scenario: A hand-installed file has drifted

- **WHEN** `thegn doctor` runs with a completion file that no longer matches
  what the binary generates
- **THEN** the entry is reported `stale` with its path and the command that
  regenerates it

#### Scenario: A registration script is not called stale

- **WHEN** `thegn doctor` runs with a registration script installed
- **THEN** the entry is reported `dynamic` rather than compared for staleness

#### Scenario: Nothing installed is a report, not a failure

- **WHEN** `thegn doctor` runs on a machine with no completion files anywhere
- **THEN** every entry is reported `absent` with its fix command and the command
  exits 0
