---
files:
  - crates/thegn-host/src/voice.rs
  - crates/thegn-host/src/handlers/voice.rs
  - crates/thegn-svc/src/voice.rs
overlaps: []
after: []
---

# THE-59 revision 1 — make external transcription cancellable and bounded

## Gap

The implementation is correct for the ordinary success path, but the
external-process lifecycle does not satisfy the design's cancellation and
termination invariant.

1. `crates/thegn-host/src/voice.rs:127-157` starts transcription with no
   cancellation handle. `handlers::cancel` only returns the reducer to `Idle`;
   the service child continues running until it exits or its timeout expires.
   This violates the design requirement that cancel terminates the transcriber,
   discards its result, and leaves no user-command worker behind. The same gap
   occurs when live config reload disables voice while a transcription is in
   flight.
2. `crates/thegn-svc/src/voice.rs:111-119` calls `stdin.write_all(wav)` before
   entering the deadline/`try_wait` loop. A configured command that does not
   read stdin can block that write indefinitely for a sufficiently large WAV,
   so `max_seconds` is not a hard bound. The early error paths after spawn
   (`stdin`/`stdout`/`stderr` extraction and `write_all`) also return without
   killing and reaping the child. This violates the design's requirement that
   hanging, failing, and cancelled commands are bounded and terminated on all
   paths.

## Required correction

- Add a per-transcription cancellation signal/handle owned by
  `VoiceController`. `handlers::cancel` and the disable/reconfigure path must
  signal it before or while applying the reducer cancellation. Dropping the
  controller must also cancel any in-flight transcription. Keep stale
  request-id rejection as a second defense; cancellation must not inject or
  log the eventual result.
- Make the command provider's blocking stdin transfer participate in the same
  hard deadline and cancellation path as child waiting. A writer thread or an
  equivalent bounded design is acceptable, provided the event-loop-facing
  worker never blocks indefinitely and the child is killed and reaped on
  timeout, cancellation, spawn/setup failure, pipe failure, and reader/cap
  failure. Join/close the helper threads after termination so no detached
  command or pipe reader survives the utterance.
- Preserve argv-only execution, bounded stdout/stderr/WAV limits, the existing
  `Utility` QoS, and the synchronous provider seam. Do not move process work to
  the event loop or add a new CLI/control/capability surface.

## Tests and verification

Add focused regression coverage for:

- cancellation while transcription is in flight: the child is terminated (or
  the cancellation signal is observed), the result is ignored, and a later
  stale message cannot inject;
- a transcriber that never reads stdin / emits excessive output: the call
  returns at the configured bound and does not leave a child or reader thread;
- setup/write/timeout failure cleanup, including child reaping where the
  platform permits observing it.

Run:

```text
just quick thegn-svc
just quick thegn-host
cargo nextest run -p thegn-svc voice
cargo nextest run -p thegn-host voice
```

The revision must leave the existing core, service control-schema, and host
repository ratchets green. No unrelated formatting or polish round is needed.

## Commit subject

`fix(the-59): bound and cancel voice subprocesses`
