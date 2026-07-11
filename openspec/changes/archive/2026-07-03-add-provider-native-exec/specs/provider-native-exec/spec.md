# Provider Native Exec

## ADDED Requirements

### Requirement: Providers expose a generic native exec capability

A managed-sandbox provider SHALL declare an `exec_api` capability and, when set, MUST offer `open_exec` (start a PTY exec session) and `attach_exec` (resume a persisted session) returning a transport-agnostic session handle of channels (output frames, a control sink for stdin/resize/close, and the announced session id), with no vendor CLI involved.

#### Scenario: Sprites advertises native exec

- **WHEN** the provider is Sprites
- **THEN** `caps().exec_api` is true and `open_exec` connects to its WSS exec API with the bearer token

#### Scenario: A provider without native exec reports it

- **WHEN** `open_exec` is called on a provider whose `exec_api` is false
- **THEN** it returns a clear unsupported error rather than a partial session

### Requirement: An interactive pane can be backed by a provider exec session

A pane SHALL support a stream transport whose bytes come from a provider exec session instead of a local PTY, and it MUST feed the same emulator/grid and emit the same pane output/exit events as a PTY pane so the event loop and renderer are transport-blind.

#### Scenario: Native pane behaves like a terminal

- **WHEN** a worktree's env selects native provider exec
- **THEN** opening its shell pane streams the sandbox shell over the API, forwards keystrokes and resizes, and ends the pane on the remote shell's exit — with no vendor CLI process spawned

### Requirement: A per-env exec mode selects native vs CLI

An env's provider config SHALL accept an `exec` mode of `auto`, `api`, or `cli` defaulting to `auto`, where `auto` uses native exec when the provider supports it (else the CLI), `api` forces native exec, and `cli` always uses the vendor CLI bridge.

#### Scenario: Auto prefers native when available

- **WHEN** `exec` is unset (auto) and the provider has `exec_api`
- **THEN** the interactive pane attaches over the native API, not the CLI

#### Scenario: CLI mode keeps the bridge

- **WHEN** `exec = "cli"`
- **THEN** the pane is wrapped through the configured `interactive_command`

### Requirement: Native exec sessions reattach across restart

A native-exec pane's provider session id SHALL be persisted with the tab layout, and on restart the pane MUST reattach that session (replaying its scrollback) when it still exists, falling back to a fresh exec otherwise.

#### Scenario: Restart resumes the live session

- **WHEN** thegn restarts with a persisted native-exec pane whose remote session is still alive
- **THEN** the resurrected pane reattaches and replays the scrollback rather than starting a new shell
