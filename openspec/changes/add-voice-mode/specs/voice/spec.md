# Voice Mode

## ADDED Requirements

### Requirement: Voice mode is optional and costs nothing when absent

Voice input SHALL be strictly additive: compiled only under a `voice` cargo
feature (audio/whisper dependencies carry zero weight in the default build),
disabled by default (`[voice] enabled = false`), and registered as an
Experimental feature so the stable channel's config clamp forces it off with
the standard status note. While disabled or not compiled, thegn MUST create no
audio thread, open no capture device, and add no wake source — the 0%-idle
contract is unchanged. A build without the feature MUST accept `[voice]` config
keys without blocking launch and report the feature as unavailable with
guidance rather than failing silently.

#### Scenario: Default build carries no voice cost

- **WHEN** thegn is built without the `voice` feature or runs with
  `[voice] enabled = false`
- **THEN** no capture thread or device handle exists, no voice wake source is
  registered, and idle behaviour is byte-identical to a build before this
  change

#### Scenario: Stable channel clamps voice off

- **WHEN** a stable-channel build loads a config with `[voice] enabled = true`
- **THEN** the clamp disables it and the standard one-line clamp note is shown

#### Scenario: Feature-off build gives guidance

- **WHEN** a user invokes a voice CLI verb or the config enables voice in a
  build compiled without the feature
- **THEN** thegn refuses with a message naming the missing feature, and the
  config keys do not block launch

### Requirement: Capture is toggle-to-talk, explicit, bounded, and visible

Voice capture SHALL start only on an explicit user action (`voice-toggle`) and
stop on the earliest of: the same action again, a configured hard cap
(`max_seconds`), or — when enabled — a trailing-silence window
(`silence_stop` / `silence_ms`). A `voice-cancel` action (and Esc while
recording) MUST discard the buffer without transcribing. While capture is
active a statusbar recording indicator MUST be visible, and it MUST be pinned
under the e2e determinism freeze. The silence/cap stop decision MUST be pure
core logic (unit-tested over per-frame RMS input); there SHALL be no always-on
or wake-word listening.

#### Scenario: Toggle starts and stops a capture

- **WHEN** the user presses the `voice-toggle` chord, speaks, and presses it
  again
- **THEN** capture runs only between the two presses, the recording indicator
  is shown throughout, and the buffer is handed to the transcriber on stop

#### Scenario: Silence auto-stop

- **WHEN** `silence_stop` is enabled and the trailing `silence_ms` of audio
  stays below the silence threshold
- **THEN** capture stops and transcription begins without a second keypress

#### Scenario: Hard cap and cancel

- **WHEN** a recording reaches `max_seconds`, or the user invokes
  `voice-cancel` mid-recording
- **THEN** capture stops immediately; on cancel the audio is discarded and
  nothing is transcribed

### Requirement: Capture and transcription run off the event loop

Audio capture and transcription SHALL run on background threads declaring a
non-interactive QoS class, delivering results to the loop over a channel plus
a `TerminalWaker` pulse. The capture side MUST send state transitions rather
than per-buffer audio events, and the recording indicator MUST damage chrome
only on state transitions (any elapsed-time readout rides the existing
ticker). Transcription failures MUST surface via status/notification, never
silently.

#### Scenario: Recording does not storm the loop

- **WHEN** a multi-second capture is in progress
- **THEN** the event loop is woken only for capture state transitions (and
  existing ticker beats), not per audio buffer

#### Scenario: Transcript arrives over the channel

- **WHEN** transcription completes off-thread
- **THEN** the result is sent on a channel, the waker is pulsed once, and the
  loop injects it on the next drain

### Requirement: The transcriber is a provider seam

Transcription SHALL be a provider seam: an object-safe trait with a config
`kind` where every value is implemented or reserved, `SeamError`
classification (a missing binary reports not-installed with acquisition
guidance), and a Probe reported in `thegn doctor` naming the selected kind,
the resolved binary and model, and the capture device state. The default kind
MUST be a `whisper_cli` subprocess whose vendor CLI is invoked only inside its
implementation file, with the binary resolved by the managed-tools three-tier
order (override → PATH → managed). A generic `command` kind MUST run a
user-configured program (WAV in, transcript out) and be labeled user-provided
in doctor output. A native in-process kind (`whisper_rs`) SHALL be reserved
until implemented behind its own cargo feature.

#### Scenario: Default subprocess transcription

- **WHEN** `kind = "whisper_cli"` and a `whisper-cli` binary resolves via
  override, PATH, or the managed tier
- **THEN** the captured audio is transcribed by invoking that binary with the
  resolved model, off the event loop

#### Scenario: Missing binary is guided, not fatal

- **WHEN** no `whisper-cli` resolves on any tier
- **THEN** the seam reports not-installed, doctor names the tiers it tried,
  and the user is pointed at PATH installation or the managed pull

#### Scenario: Reserved kind is rejected by strict validation

- **WHEN** a config selects `kind = "whisper_rs"` before that impl ships
- **THEN** `thegn config validate --strict` rejects it as reserved

### Requirement: Transcripts inject as inert input to the focused pane

A completed transcript SHALL be injected into the focused pane through the
same hardened paste path as user-initiated pastes: one
bracketed-paste-wrapped write with embedded paste markers neutralized, queued
non-blocking to the pane transport (in-process PTY writer or daemon Stream
relay), so daemon-owned sessions receive it identically to typed input.
thegn MUST NOT append a newline or otherwise auto-submit the transcript, and
MUST NOT parse it into thegn actions. If the focused pane changed or died
between capture and completion, the transcript MUST be surfaced via
status/notification instead of being written to a pane the user is not
looking at. No new capability-catalog row is added: external injection remains
the existing `SendInput` capability.

#### Scenario: Transcript lands as reviewable text

- **WHEN** transcription completes while the originating pane is still focused
- **THEN** the text appears at the pane's input as a single hardened paste
  with no trailing newline, and nothing executes until the user presses Enter

#### Scenario: Daemon-owned pane receives the transcript

- **WHEN** the daemon owns the pane's PTY
- **THEN** the injection rides the daemon transport and the text persists in
  the session exactly like typed input

#### Scenario: Focus moved away mid-transcription

- **WHEN** the focused pane at completion differs from the pane focused at
  capture start (or the pane exited)
- **THEN** the transcript is offered via the status line/notification and is
  not written into the newly focused pane unasked

### Requirement: Audio stays on-device and ephemeral by default

With the built-in transcriber kinds, no audio or transcript SHALL leave the
machine. Captured samples live in memory; any temporary audio file MUST be
written with owner-only permissions to a runtime directory and deleted on
every exit path (success, failure, cancel). Recordings persist only under an
explicit `keep_recordings = true` opt-in, to a configurable directory.
Transcript content MUST NOT be written to logs. The `command` kind is
user-consented: configuring an external transcriber command is the user's
explicit choice, and doctor MUST label it so the on-device claim is never
silently false.

#### Scenario: Temp audio is cleaned up on failure

- **WHEN** transcription fails after the temp WAV was written
- **THEN** the temp file is still deleted and the failure is surfaced via
  status/notification

#### Scenario: Opt-in persistence

- **WHEN** `keep_recordings = true`
- **THEN** the WAV is retained under the configured recordings directory;
  otherwise no audio survives the utterance

### Requirement: Models are acquired grant-checked, size-warned, and integrity-pinned

Whisper models SHALL be described by a pinned core table (name → download URL,
declared size, SHA256) covering the stock ggml set including large-v3-turbo,
and acquired through the managed-tools artifact story: the download is refused
without a matching `download_file` capability grant, the declared size is
shown before fetching with explicit confirmation required above a threshold,
the checksum is verified before the model is installed, and fetches never run
on the event loop. Model resolution SHALL be: `model_path` override → the
managed model store; `thegn voice model list|pull|rm` manages the store and
`thegn doctor` reports the resolved model, its tier, and its
pinned-vs-installed state.

#### Scenario: Ungranted download is refused

- **WHEN** `thegn voice model pull large-v3` runs with no `download_file`
  grant covering the model URL
- **THEN** the download is refused with a message naming the missing grant

#### Scenario: Large model requires explicit confirmation

- **WHEN** a requested model's declared size exceeds the warning threshold
- **THEN** the size is shown and the pull proceeds only with explicit
  confirmation (`--yes` or an interactive confirm)

#### Scenario: Checksum mismatch aborts the install

- **WHEN** a downloaded model's SHA256 does not match the pinned value
- **THEN** the file is discarded, nothing is installed, and the failure is
  surfaced

#### Scenario: Override skips the store

- **WHEN** `model_path` points at an existing ggml file
- **THEN** resolution uses it directly and no download is attempted

### Requirement: Voice surfaces are registered and documented

The voice actions SHALL be ACTION_SPECS entries (`voice-toggle`,
`voice-cancel`) with default chords that dispatch, palette entries gated on
the feature being enabled, and a `docs/help/voice.md` page claiming both ids
with real prose — including the per-platform microphone-permission notes
(macOS attributes the mic prompt to the hosting terminal app) and the
never-auto-submit rule. Every `[voice]` key SHALL be documented in
`config/config.toml.example` and validated by the strict config validator.

#### Scenario: Actions are claimed by the help page

- **WHEN** the help ratchets run
- **THEN** both voice action ids are claimed by `docs/help/voice.md` and the
  page mentions them by chord, id, or label

#### Scenario: Config keys are documented and validated

- **WHEN** `tests/config_example.rs` and `thegn config validate --strict` run
- **THEN** every `[voice]` key is present in the example config and the
  example validates clean
