# Agent Review

## ADDED Requirements

### Requirement: Pinned diff annotations batch into one agent prompt

The review pane SHALL let the user pin inline annotations to diff hunks that survive subsequent edits, and "send all to agent" MUST collect every pinned annotation into a single ACP `session/prompt` follow-up to the embedded agent rather than one prompt per annotation; with no agent/proxy configured the annotations MUST remain usable as a plain review-note list with the agent-send action disabled and no model call made.

#### Scenario: Annotations survive edits and batch to one prompt

- **WHEN** the user has pinned several annotations across hunks and selects "send all to agent" with an agent available
- **THEN** the pinned annotations are re-anchored across the intervening edits and dispatched as a single `session/prompt` follow-up

#### Scenario: No agent or proxy configured

- **WHEN** the user pins annotations but no agent/proxy is configured
- **THEN** the annotations remain a plain review-note list and the "send all to agent" action is disabled with no model call made

### Requirement: Commit-message draft falls back to a template when AI is off

On commit the system SHALL be able to draft a commit message from the staged diff via `thegn-proxy` and pre-fill the commit editor for the user to confirm, and when no agent/proxy is configured it MUST instead open the editor with the deterministic template or empty body; in neither case does it auto-commit or pass `--no-verify`.

#### Scenario: AI available drafts from the staged diff

- **WHEN** the user starts a commit with a non-empty staged diff and a proxy is configured
- **THEN** a drafted message is generated from the staged diff and pre-fills the commit editor for the user to edit and confirm

#### Scenario: AI off opens the template editor

- **WHEN** the user starts a commit with no agent/proxy configured
- **THEN** the commit editor opens with the deterministic template or empty body and no model call is made

#### Scenario: Hooks are never bypassed

- **WHEN** a commit proceeds with a drafted or template message
- **THEN** the configured hooks run normally and `--no-verify` is not passed

### Requirement: Fix with AI repairs failing checks without bypassing hooks

A "Fix with AI" action SHALL assemble a repair prompt from a failed pre-commit hook's output, or from a CI check's `CiRun` failed-job names plus `CiLog::first_failure_line`, hand it to the embedded agent, and re-run the failing check or hook after the agent edits the worktree; it MUST NOT pass `--no-verify` or otherwise bypass hooks, and when no agent/proxy is configured it MUST degrade to simply showing the failing output.

#### Scenario: Failed pre-commit hook repaired with AI

- **WHEN** a pre-commit hook fails and the user invokes "Fix with AI" with an agent available
- **THEN** the hook's output is sent to the agent as a repair prompt and the hook is re-run after the agent's edits, without `--no-verify`

#### Scenario: Failed CI check repaired with AI

- **WHEN** a CI check fails and the user invokes "Fix with AI" with an agent available
- **THEN** the repair prompt is assembled from the `CiRun` failed-job names and `CiLog::first_failure_line` and handed to the agent

#### Scenario: No agent or proxy configured

- **WHEN** the user invokes "Fix with AI" with no agent/proxy configured
- **THEN** the action shows the failing output (hook text or first-failure line) in the pane and makes no model call

### Requirement: Image-diff modes render before and after via the graphics preview path

The diff pane SHALL offer swipe and onion-skin comparison modes for a changed image whose format the graphics preview path supports, rendering the `HEAD` blob against the working blob through that path; this feature MUST be AI-free, and an image whose format the preview path does not support MUST fall back to the normal text/binary diff treatment.

#### Scenario: Supported image gets swipe and onion-skin modes

- **WHEN** a changed image of a preview-supported format is shown in the diff pane
- **THEN** the user can toggle swipe or onion-skin comparison of the `HEAD` blob against the working blob via the graphics preview path, with no model call involved

#### Scenario: Unsupported image format

- **WHEN** a changed image's format is not supported by the graphics preview path
- **THEN** the diff falls back to the normal text/binary diff treatment

### Requirement: Review enhancements run off the loop and stay damage-bounded

Every model call, CI fetch, and log fetch backing these features SHALL run off the event loop and return over a channel that pulses the `TerminalWaker`, and MUST NOT add a polling timeout or run blocking I/O on the loop; an in-flight draft or repair MUST leave the loop idle, and chrome transitions (pinning, draft editor open, fix status change, image-diff mode switch) MUST trigger a `Full` frame only on transition rather than per tick.

#### Scenario: Draft or repair in flight

- **WHEN** a commit-message draft or fix-with-AI repair is in flight and nothing else is dirty
- **THEN** an idle wake yields `Skip` with no frame and no added tick, and the result later arrives over a waker-pulsing channel

#### Scenario: Chrome transition on a review action

- **WHEN** the user pins an annotation, opens the drafted commit editor, changes a fix-with-AI status, or switches image-diff mode
- **THEN** a single `Full` frame is rendered on that transition rather than one per streaming tick
