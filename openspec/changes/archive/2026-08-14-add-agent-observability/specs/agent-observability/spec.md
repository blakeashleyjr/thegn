# Agent observability

## ADDED Requirements

### Requirement: Usage and rate-limit state is read locally without any API

The usage/rate-limit reader SHALL derive each agent's plan usage and reset
window **only** from the provider's on-disk credential-home state (`~/.claude`,
`~/.codex`) and MUST never make a network call or depend on the LLM proxy, so it
works for agents that bypass the proxy and remains functional when the AI layer
is absent.

#### Scenario: Usage parsed from disk for the active account

- **WHEN** the active worktree's resolved provider account dir contains valid
  usage state
- **THEN** a typed `UsageState` (plan %, reset window) is produced from that file
  alone, with no network request and no proxy involvement

#### Scenario: Missing or malformed state hides the widget

- **WHEN** the usage file is absent or unparseable
- **THEN** the reader yields no usage state and the statusbar widget is hidden
  rather than erroring

#### Scenario: Approaching the limit warns

- **WHEN** parsed usage reaches at least 80% of the plan
- **THEN** the usage chip surfaces an 80% warning state

### Requirement: Usage refresh runs off the event loop

Usage state SHALL refresh on the background ticker via `RefreshKind::Usage` on a
coarse (~30–60s) cadence, perform its disk read off the event loop, and MUST
pulse the `TerminalWaker` so an idle loop wakes only to service a real change,
preserving the 0%-idle contract.

#### Scenario: Idle wake with unchanged usage skips the frame

- **WHEN** the usage refresh fires and the parsed state is unchanged
- **THEN** the loop wakes, finds no model delta, and the render decision is Skip

#### Scenario: Changed usage repaints chrome

- **WHEN** the parsed usage state changes
- **THEN** the chrome is marked dirty and the next frame is Full

### Requirement: The OSC title is the authoritative agent-state signal

Working / idle / waiting agent state SHALL be determined from the pane's OSC 0/2
terminal title when a title signal is present, and the existing CPU-jiffy
heuristic MUST be used only as a fallback when no title classification applies,
so an agent that reports state via its title is read directly.

#### Scenario: Title classification overrides the heuristic

- **WHEN** a pane's OSC title classifies to a working/idle/waiting state
- **THEN** the activity FSM uses that state regardless of the CPU heuristic

#### Scenario: No title falls back to CPU

- **WHEN** a pane has set no classifiable OSC title
- **THEN** the activity dot is driven by the CPU-jiffy heuristic and the
  sticky-red waiting semantics are unchanged

### Requirement: Cross-provider session history lists from native transcripts

A `SessionHistoryBackend` SHALL enumerate past agent sessions by scanning each
provider's native transcript directory (`~/.claude`, `~/.codex/sessions`)
read-only, exposing cwd, branch, model, token estimate, and first-ask per
session, and MUST never mutate those files.

#### Scenario: Sessions listed across providers

- **WHEN** the history backend scans the configured providers' transcript dirs
- **THEN** it returns the per-session metadata sorted newest-first, capped to a
  bounded count

#### Scenario: Cache evicts a vanished transcript

- **WHEN** a cached `agent_sessions` row points at a transcript file that no
  longer exists on disk
- **THEN** that row is evicted so the list reflects only live transcripts

### Requirement: One-click resume uses the agent's own command

Resuming a listed session SHALL launch the originating provider's own CLI with
its native `--resume` flag as an ordinary pane launch, and MUST NOT run on the
event loop.

#### Scenario: Resume builds the provider argv

- **WHEN** the user resumes a session for a given provider
- **THEN** the constructed command is that provider's CLI with `--resume`,
  launched as a normal pane (never blocking the loop)

### Requirement: The agents feed aggregates activity over the EventBus

The agents feed SHALL present a cross-worktree, threaded activity log assembled
from `EventBus` events grouped per worktree, MUST smart-pin worktrees with a
currently-running agent, and SHALL map a feed entry to its pane for
click-to-jump — built off the loop so it never blocks rendering.

#### Scenario: Events thread by worktree

- **WHEN** multiple agent events arrive for several worktrees
- **THEN** the feed groups them into per-worktree threads, newest entry first

#### Scenario: Running agents are pinned

- **WHEN** a worktree has an agent in the working state
- **THEN** that worktree is smart-pinned in the feed until the agent goes idle

#### Scenario: Feed delta repaints, idle wake skips

- **WHEN** a new feed event arrives versus an idle wake with no new event
- **THEN** the new event marks chrome dirty for a Full frame while the idle wake
  yields Skip
