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

### Requirement: Live app opening preserves its own top-level session

thegn SHALL open the selected development app URL unchanged in an ordinary
top-level page of the external browser by default. App navigation, forms,
cookies, and the app's own hot-reload WebSocket SHALL remain the app's traffic;
thegn MUST NOT proxy or re-render app traffic for this path. Any future
embedded web surface SHALL also offer the ordinary-tab action and MUST NOT make
embedding the only way to open the app. Opening and copying SHALL use the
selected target's source and last known status before re-arming one bounded
probe, and SHALL distinguish a stopped target from one whose reachability is
not yet proven.

#### Scenario: Edit triggers the app's own hot reload

- **WHEN** the user edits a file served by a running development app and the
  app publishes an HMR update
- **THEN** the opened top-level app applies that update over its own WebSocket

#### Scenario: Open reports the last known target state

- **WHEN** the user opens or copies a selected preview target
- **THEN** the action reports the source and state the user saw, with a
  stopped target distinguished from an unproven one

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
