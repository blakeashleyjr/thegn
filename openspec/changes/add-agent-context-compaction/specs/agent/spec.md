# Agent

## ADDED Requirements

### Requirement: Scrollback fed to an agent can be deterministically compacted

thegn SHALL provide a deterministic, pure templater that collapses repetitive
scrollback lines into template groups (template, occurrence count, a few samples,
and variable slots) before the scrollback is handed to an agent or the proxy as
context. The same input MUST always produce the same groups in a stable order
(groups emitted in ascending first-occurrence order), and the compaction MUST NOT
issue any model call.

#### Scenario: Repeated lines collapse to one group

- **WHEN** a scrollback window contains many near-identical lines and compaction
  runs
- **THEN** those lines become a single template group with the correct occurrence
  count

#### Scenario: Compaction is deterministic

- **WHEN** the same scrollback window is compacted twice
- **THEN** the two results are identical, with groups in the same order

### Requirement: Compaction is gated by window size and off by default

thegn SHALL apply compaction only when a scrollback window exceeds a configured
size threshold, passing smaller windows through unchanged, because compaction
degrades small windows; and compaction MUST be off by default so the AI-free shell
and un-opted context paths are unaffected.

#### Scenario: A small window is passed through unchanged

- **WHEN** a scrollback window below the size threshold is prepared as context
- **THEN** the raw window is returned unchanged

#### Scenario: A large window is compacted when enabled

- **WHEN** compaction is enabled and a scrollback window above the threshold is
  prepared as context
- **THEN** the window is returned in compacted (template-group) form
