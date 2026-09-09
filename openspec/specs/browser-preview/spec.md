# browser-preview Specification

## Purpose

TBD - created by archiving change add-browser-preview-loop. Update Purpose after archive.

## Requirements

### Requirement: Preview targets derive from discovered local services

thegn SHALL derive preview candidates from the existing detected-port and
forward state, select a deterministic active candidate, and update the preview
when that state changes without adding an idle poll.

#### Scenario: A development server becomes available

- **WHEN** the existing detector reports a preview-eligible local port
- **THEN** the preview surface exposes its loopback or forwarded URL without a
  second discovery loop

### Requirement: Preview opening delegates to existing placement seams

thegn SHALL open a selected preview in the user's external browser or, when a
terminal preview tool is configured, through the existing ordinary pane/tool
placement mechanisms. thegn MUST NOT embed a browser engine or read, copy, or
decrypt an installed browser's profile, cookies, history, sessions, or
credentials.

#### Scenario: Open in the user's browser

- **WHEN** the user invokes the preview open action for an active target
- **THEN** thegn delegates the URL to the existing external-open seam

#### Scenario: No browser identity is imported

- **WHEN** any preview surface is used
- **THEN** no installed-browser profile or OS-keychain material is accessed

### Requirement: Diagnostic preview fetch is bounded and credential-free

`preview.fetch` SHALL fetch an explicitly selected preview URL without ambient
cookies or browser identity, off the UI loop, with configured timeout and
response-size bounds. Completion SHALL return through the normal async result
and waker path, and failure SHALL preserve UI responsiveness.

#### Scenario: Oversized response is bounded

- **WHEN** a preview response exceeds the configured maximum size
- **THEN** the fetch stops at the bound and reports a stable error without
  blocking the event loop

#### Scenario: Disabled preview is idle

- **WHEN** preview support is disabled and no relevant state changes
- **THEN** it performs no periodic preview work
