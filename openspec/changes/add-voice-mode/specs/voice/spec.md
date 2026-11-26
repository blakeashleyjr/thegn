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
Preview inference SHALL suppress non-speech and blank tokens, disable temperature
fallback retries, use one segment without timestamps, and scale its audio context
to the preview window. Final inference SHALL suppress non-speech and blank tokens
while retaining the default temperature fallback and full audio context.

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

#### Scenario: Readiness verdict

- **WHEN** a user runs voice status or doctor
- **THEN** report whether configuration is complete, naming the first missing step
  in order: experimental channel, enabled, native build, model or provider
- **AND** never open the microphone, so readiness does not prove device access

### Requirement: HTTP provider and command compatibility

HTTP SHALL support configured endpoint/model/language and broker credential
references, multipart WAV input and JSON text output. It SHALL bound deadlines,
cancellation and response sizes, surface failures, and never silently fall back.
Setup SHALL disclose repeated audio billing for live requests and offer final-only.
Existing argv WAV-in/text-out command adapters SHALL remain final-only and accept
external or native capture.
The Hex preset SHALL invoke `hex transcribe <temporary WAV>` through the bounded
command adapter, remove the temporary recording after transcription, and report
when the `hex` executable is unavailable with installation guidance. Whisper
SHALL remain the default provider.

#### Scenario: HTTP live and final-only

- **WHEN** HTTP live preview is enabled
- **THEN** send overlapping windows sequentially and one complete recording on stop
- **AND** with final-only enabled send just the complete recording

### Requirement: Level-based silence rejection and final edge trim

Voice transcription SHALL use DC-removed RMS levels over 30 ms windows. It
SHALL accept a window at or above -50 dBFS immediately, or accept a contiguous
run of at least six windows at or above the greater of the 10th-percentile noise
floor plus 12 dB and -66 dBFS. One quiet window MAY be bridged within a run;
two consecutive quiet windows SHALL end it. The final embedded decode SHALL
include 200 ms of context around detected speech, clamped to the recording;
previews SHALL retain their original sample offsets. HTTP SHALL skip both
preview and final requests when the recording has no detected speech.

#### Scenario: Silence and noise do not reach transcription

- **WHEN** a recording is digital silence or only low-level stationary noise
- **THEN** embedded transcription returns empty text and HTTP sends no request

#### Scenario: Final decode keeps speech context

- **WHEN** speech is detected in the final recording
- **THEN** embedded final inference uses the speech span plus up to 200 ms on
  either side, while preview inference uses the untrimmed preview window

### Requirement: Measured release acceptance

Native desktop enablement SHALL wait for Linux and Apple silicon real-device
capture/permission/routing/packaging evidence. Warm preview p95 SHALL target three
seconds or less on recorded reference machines under coding workload. Finalization
SHALL be measured separately; implementation alone SHALL NOT assert these results.

#### Scenario: Missing verification

- **WHEN** platform evidence or tracker reconciliation remains outstanding
- **THEN** retain experimental gating and mark delivery verification-gated

### Requirement: Non-speech annotation and credit stripping

Before a final transcript is normalized, voice SHALL remove Whisper non-speech
annotations (for example `[BLANK_AUDIO]`, `[Music]`, or `(laughter)`) and one
trailing subtitle credit. Square-bracket spans SHALL be removed when their text
starts with a non-speech word. Parenthesized spans SHALL be removed only when that
word ends the span, so `(see above)` is kept. A bare `Thank you.` SHALL be kept.
Foreign-language credits SHALL apply only when the configured language is not `en`.

#### Scenario: Annotations and credits

- **WHEN** a final transcript is `Ship it. Thanks for watching!`
- **THEN** insert `Ship it.`
- **AND** `hello [Music] world` becomes `hello world`, and `see (above)` is unchanged

### Requirement: Final transcript pipeline order

Every final transcript SHALL pass through the same cleanup in this order: strip
non-speech annotations and credits, then drop a transcript that only echoes the
vocabulary prompt (embedded and HTTP kinds only), then apply `[voice]
replacements`. Partial previews SHALL NOT be rewritten.

#### Scenario: Annotation hides a prompt echo

- **WHEN** vocabulary is `["T3 Code", "Ottobit"]` and the final transcript is `[BLANK_AUDIO] T3 Code, Ottobit`
- **THEN** nothing is inserted
