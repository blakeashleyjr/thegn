# Agent

## ADDED Requirements

### Requirement: A fleet view presents authoritative per-worktree agent metrics

thegn SHALL present a fleet view showing, per worktree running an agent, its
context-window usage, token split (input / output / cache-read / cache-create),
turn count, current task, child processes with open ports, and a live tool-call
timeline. Token and context metrics MUST be sourced authoritatively from the LLM
proxy's stored request records (not scraped from transcripts), and rendering the
view MUST NOT issue any model call. With no agent running, the fleet view MUST be
empty and MUST NOT affect the AI-free shell.

#### Scenario: Metrics come from the proxy, not scraping

- **WHEN** an agent in a worktree completes a request through the proxy
- **THEN** the fleet view's token split and turn count for that worktree reflect
  the proxy's recorded request, including cache tokens

#### Scenario: Rendering spends no quota

- **WHEN** the fleet view or its JSON snapshot is produced
- **THEN** no model call is issued; the data is derived only from stored request
  records and live process state

#### Scenario: Empty with no agent

- **WHEN** no worktree is running an agent
- **THEN** the fleet view is empty and the shell is unaffected

### Requirement: The fleet view detects context compaction

thegn SHALL flag a context compaction for a worktree when its context tokens
drop between consecutive turns by more than a configured threshold, so a reviewer
can see when an agent compacted its context. A small dip below the threshold and a
worktree's first turn MUST NOT be flagged.

#### Scenario: A large context drop is flagged as compaction

- **WHEN** a worktree's context tokens fall from one turn to the next by more than
  the threshold
- **THEN** a compaction is flagged for that worktree

#### Scenario: A small dip is not a compaction

- **WHEN** a worktree's context tokens fall by less than the threshold
- **THEN** no compaction is flagged

### Requirement: The fleet snapshot is available as machine-readable JSON

thegn SHALL provide a `thegn fleet --json` subcommand that emits the current
per-worktree fleet metrics as JSON for external tools, read-only and without
issuing a model call.

#### Scenario: JSON snapshot emits the rollup

- **WHEN** the user runs `thegn fleet --json`
- **THEN** the current per-worktree metrics are printed as JSON with no model call
