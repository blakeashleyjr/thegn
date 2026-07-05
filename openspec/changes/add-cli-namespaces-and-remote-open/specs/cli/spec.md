# CLI

## ADDED Requirements

### Requirement: The worktree lifecycle is drivable headlessly through a `wt` namespace

superzej SHALL expose a `wt` noun-verb namespace (`wt list|new|rm|diff|disk|clean`)
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

- **WHEN** `szhost wt new fix-parser --repo <root>` runs with no TUI
- **THEN** a new branch + worktree exist under the configured worktrees dir,
  the worktree is registered in the DB, and stdout is exactly the new path

#### Scenario: Removal cleans resurrection state

- **WHEN** `szhost wt rm <path> --force` completes
- **THEN** the checkout is gone, the branch remains (absent `--delete-branch`),
  and no worktree/tab-group rows for it remain in the DB

#### Scenario: Unknown target

- **WHEN** `wt rm` is given a target matching no known worktree or branch
- **THEN** it lists candidates and exits with code 3

### Requirement: Legacy bare verbs remain functional but hidden

superzej SHALL keep the legacy top-level verbs `list`, `diff`, `disk`,
`clean`, `repos`, and `recent` working with output byte-identical to their namespaced
equivalents (`wt …`, `repo …`), and SHALL be hidden from `--help`. Their flags
MUST be shared definitions with the namespaced forms so the two spellings
cannot drift.

#### Scenario: Old scripts keep working

- **WHEN** `szhost list` runs after the namespaces land
- **THEN** its output is byte-identical to `szhost wt list` and no deprecation
  breaks the invocation

### Requirement: List-shaped read commands emit machine-readable JSON

superzej SHALL accept `--json` on every list-shaped read surface (`wt
list`/`list`, `env list`, `host list`, `ci runs`, `share list`, `forward
list`, `disk`) and
emit a single compact JSON document on stdout with no ANSI sequences. The CLI
SHALL honor a documented exit-code contract: 0 success, 1 error, 2
transient/retryable, 3 target not found.

#### Scenario: JSON is parseable

- **WHEN** `szhost wt list --json` runs
- **THEN** stdout parses as one JSON array and contains no escape sequences

#### Scenario: Scripts can branch on exit codes

- **WHEN** `szhost open no-such-repo --no-launch` fails to resolve
- **THEN** the process exits with code 3

### Requirement: Top-level help renders commands in semantic groups

`szhost --help` SHALL render non-hidden commands grouped (Workspace, Forge,
Environments, Session, Meta) with names and descriptions sourced from the live
clap definitions. A unit test MUST fail when a non-hidden command is not
assigned to exactly one group. Subcommand help (`szhost wt --help`) MUST be
unaffected by the grouping template.

#### Scenario: Grouped help

- **WHEN** `szhost --help` is rendered
- **THEN** the Workspace and Forge headings appear and hidden commands do not

#### Scenario: Ungrouped command fails CI

- **WHEN** a new visible top-level command is added without a group assignment
- **THEN** the drift-guard unit test fails

### Requirement: Shell completions are generated from the CLI definition

superzej SHALL provide `completions <shell>` generating shell completions from
the live clap definition, using the invoked binary name (szhost / superzej /
sj) as the completion target.

#### Scenario: Bash completions

- **WHEN** `szhost completions bash` runs
- **THEN** a completion script for the invoked binary name is written to stdout

### Requirement: `open <repo>` remote-controls or launches the compositor

`szhost open <repo>` SHALL resolve its argument (path, or unique basename/slug
match against known repos), and: when a live instance holds the profile
singleton lock, enqueue a `focus_workspace` intent in the DB `intents` mailbox
(consumed by the compositor's model refresh, claim-and-delete, last intent
wins); otherwise set the active-workspace pointer and launch the compositor,
which lands on that workspace via the existing startup resolution. Intent
consumption MUST tolerate a DB missing the `intents` table. Resolution misses
MUST list candidates and exit 3.

#### Scenario: Focus a running instance

- **WHEN** `szhost open myrepo` runs while a compositor is running
- **THEN** an intent row is enqueued and the running instance switches to that
  workspace within approximately one model-refresh tick

#### Scenario: Launch focused

- **WHEN** `szhost open myrepo` runs with no live instance
- **THEN** the active-workspace pointer is set and the compositor launches on
  that workspace

#### Scenario: Older DB without the mailbox

- **WHEN** the compositor hydrates against a DB lacking the `intents` table
- **THEN** hydration proceeds normally with no intents consumed
