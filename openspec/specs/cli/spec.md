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

thegn SHALL provide `completions <shell>` generating shell completions from
the live clap definition, using the invoked binary name (thegn / tg) as
the completion target.

#### Scenario: Bash completions

- **WHEN** `thegn completions bash` runs
- **THEN** a completion script for the invoked binary name is written to stdout

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
