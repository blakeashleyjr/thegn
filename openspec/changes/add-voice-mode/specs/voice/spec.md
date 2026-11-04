# Voice Mode

## ADDED Requirements

### Requirement: Explicit optional dictation

Voice SHALL remain opt-in and experimental. Disabled voice SHALL open no device,
start no worker and issue no network request. Native dependencies SHALL live in
an optional leaf crate and SHALL be omitted from static remote bridge builds.

#### Scenario: Start, preview, stop

- **WHEN** a user starts voice on a live pane
- **THEN** allocate utterance identity before capture, display revisable partials,
  close the device on stop and replace partials with the complete-recording final
- **AND** paste normalized final text through existing pane input without submission

#### Scenario: Cancel, stale results, focus and pane lifecycle

- **WHEN** a session is cancelled, config changes, the pane closes during capture, or UI exits
- **THEN** close capture, cancel bounded worker work, and discard partial text
- **AND** reject stale events; hold completed final text if pane focus changed
  or its pane closed during finalization

### Requirement: Native worker and bounded rolling work

Native capture and embedded inference SHALL run in a first-party worker process.
Capture SHALL normalize to 16 kHz mono PCM in memory, bounded to 300 seconds.
Rolling windows SHALL overlap and at most one request SHALL be in flight, with
pending work coalesced. Linux SHALL use CPU; macOS MAY use Metal with CPU fallback.

#### Scenario: Slow inference

- **WHEN** decoding is slower than incoming audio
- **THEN** select the newest bounded window after completion without a request backlog
- **AND** stop closes the device independently of inference

### Requirement: Explicit verifiable model acquisition

Setup SHALL ask language/model and show size before acquiring a missing model.
Artifacts SHALL use immutable URLs, pinned SHA-256, bounded downloads, and atomic
installation in the user data directory. Capture SHALL wait until acquisition
completes. A local path SHALL be supported. CLI SHALL expose setup, status and
model list/pull/rm.

#### Scenario: Corrupt or cancelled download

- **WHEN** a download is interrupted, corrupt, excessive, or cancelled
- **THEN** remove its temporary artifact and preserve an existing installed model
- **AND** allow explicit retry

### Requirement: HTTP provider and command compatibility

HTTP SHALL support configured endpoint/model/language and broker credential
references, multipart WAV input and JSON text output. It SHALL bound deadlines,
cancellation and response sizes, surface failures, and never silently fall back.
Setup SHALL disclose repeated audio billing for live requests and offer final-only.
Existing argv WAV-in/text-out command adapters SHALL remain final-only and accept
external or native capture.

#### Scenario: HTTP live and final-only

- **WHEN** HTTP live preview is enabled
- **THEN** send overlapping windows sequentially and one complete recording on stop
- **AND** with final-only enabled send just the complete recording

### Requirement: Measured release acceptance

Native desktop enablement SHALL wait for Linux and Apple silicon real-device
capture/permission/routing/packaging evidence. Warm preview p95 SHALL target three
seconds or less on recorded reference machines under coding workload. Finalization
SHALL be measured separately; implementation alone SHALL NOT assert these results.

#### Scenario: Missing verification

- **WHEN** platform evidence or tracker reconciliation remains outstanding
- **THEN** retain experimental gating and mark delivery verification-gated
