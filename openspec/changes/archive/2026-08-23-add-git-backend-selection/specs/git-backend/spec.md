## MODIFIED Requirements

### Requirement: GitBackend trait with native-first reads

Git operations SHALL go through the `GitBackend` trait; the read engine SHALL be selected by `[git] backend` (`auto` = the gix-native provider with CLI fallback, `gix`, or `cli`), obtained by host code from one shared handle rather than constructed per call site; reads MUST prefer the gix-native provider for speed and MUST fall back to the git CLI when the native path is missing or errors. The sidebar glyph reads (dirty, ahead/behind, branch, numstat diffs) SHALL be a `GitBackend` method so the selection governs the hot path too, with the native engine batching them over a bridged connection. Both engines SHALL implement `Probe` and appear in `thegn doctor`.

#### Scenario: Native read succeeds

- **WHEN** a supported read (e.g. ahead/behind, status) is requested and the gix
  provider can serve it
- **THEN** the native provider answers without spawning the git CLI

#### Scenario: Native gap falls back to CLI

- **WHEN** a requested operation is not implemented natively or the native call
  fails
- **THEN** the backend transparently falls back to the git CLI subprocess

#### Scenario: CLI engine selected

- **WHEN** `[git] backend = "cli"` is configured
- **THEN** every read, including the glyph scan, runs through the git CLI and doctor reports `git: cli`

#### Scenario: Host constructs the engine directly

- **WHEN** host code calls `GixGit::new()` instead of `git_handle::get()`
- **THEN** `just lint` fails
