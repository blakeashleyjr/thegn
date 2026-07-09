# Observe Integration

## Purpose

Integration governs how Observe embeds into superzej: it mounts as an app-tab via the sz-kit `AppTile` contract, contains panics so a failing panel never crashes the host, layers its config into superzej's TOML with redacted secrets, and starts fully usable with no configuration against built-in sources while operating offline with no external runtime.

## Requirements

### Requirement: Observe mounts as an app-tab without crashing the host

[M] Observe SHALL mount as an "Observe" app-tab through the sz-kit `AppTile` contract, and a panic in a panel/query/render path MUST be contained (e.g. `catch_unwind`) so it degrades that panel or the tile while the host keeps running and restores the terminal on any crash.

#### Scenario: A panel panics

- **WHEN** a panel's render or query path panics
- **THEN** the panel/tile shows an error state and superzej continues running with
  the terminal intact

### Requirement: Layered config with redacted secrets

[M] Source and Observe config SHALL layer into superzej's TOML (`[observe]`, `[observe.source.<name>]`) following the defaults → file → env → flags order; credentials MUST come from env/file/OS keyring (never inline plaintext) and MUST be redacted in logs and the query inspector. [S] TLS verification is on by default with explicit per-source opt-out.

#### Scenario: Secret is not logged

- **WHEN** a source is configured with a bearer token via `env:`
- **THEN** the token is never written to logs or shown in the query inspector

### Requirement: Sane first run and self-contained operation

[M] Observe SHALL start usable with no configuration — defaulting to the test and `host` sources so the user sees data immediately — and MUST operate fully offline against reachable backends with no external runtime or config server.

#### Scenario: Launch with no config

- **WHEN** the Observe tab is opened on a fresh install
- **THEN** it renders the built-in `host`/test sources without requiring any setup
