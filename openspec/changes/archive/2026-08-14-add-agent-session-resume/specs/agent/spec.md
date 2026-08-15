# Agent

## ADDED Requirements

### Requirement: Agent session identity is captured on start

thegn SHALL capture a running agent's native session identity and its
relaunch parameters when the agent starts, via a hook installed into the
harness's own configuration by `thegn agent hooks setup`. The installer MUST be
idempotent (re-running it is a no-op) and MUST merge into rather than overwrite
the harness config for the supported harnesses (Claude Code, Codex, Gemini CLI).

#### Scenario: Hook installer is idempotent

- **WHEN** `thegn agent hooks setup` runs twice against an installed harness
- **THEN** the harness config gains the thegn hook exactly once and the second
  run reports it already present

#### Scenario: Session start records identity

- **WHEN** an agent with an installed hook starts a session in a pane
- **THEN** thegn records the harness id, native session id, cwd, and sanitized
  relaunch argv for that worktree and pane

### Requirement: Captured relaunch parameters never contain secrets

The captured relaunch parameters SHALL be sanitized before persistence: secret-
bearing flags and their values (API keys, tokens, auth, inline prompts) MUST be
removed while safe operational flags (model, sandbox, cwd, bypass flags) are
preserved. A persisted record MUST NOT contain any secret substring.

#### Scenario: API key is stripped

- **WHEN** an agent is launched with `--api-key sk-secret` (or `--api-key=sk-secret`)
- **THEN** the persisted relaunch parameters contain neither the flag nor its value

#### Scenario: Safe flags survive

- **WHEN** an agent is launched with `--model opus --sandbox`
- **THEN** those flags are preserved in the persisted relaunch parameters

### Requirement: A restored session resumes the agent's own conversation

On session resurrection, thegn SHALL reconstruct a harness-specific resume
command from the captured session id and sanitized parameters and launch it for
the restored pane, so the agent continues its prior conversation rather than
starting fresh. Reconstruction MUST respect the existing restore-consent flow
(panes the user opted out of restoring are not resumed), and when the upstream
session no longer exists it MUST fall back to a plain relaunch and surface a
notice.

#### Scenario: Claude Code resumes by id

- **WHEN** a pane that had a Claude Code session is restored
- **THEN** thegn launches `claude --resume <session id>` with the preserved
  safe flags

#### Scenario: Missing upstream session degrades gracefully

- **WHEN** a captured session id no longer exists in the harness
- **THEN** thegn launches the harness fresh and surfaces a notice that the
  prior session could not be resumed

#### Scenario: Opted-out pane is not resumed

- **WHEN** the user declines to restore a given pane
- **THEN** no resume command is reconstructed or launched for it
