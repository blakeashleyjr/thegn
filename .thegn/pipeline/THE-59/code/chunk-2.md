# THE-59 chunk 2 — external command wiring, doctor, UI, and help

## Scope

Wire chunk 1 into the service and host edges: the generic command provider,
doctor probe, off-loop capture/transcription lifecycle, focused-pane injection,
toggle/cancel actions, recording status token, deterministic freeze, and help.
Do not add audio dependencies, native STT, model management, a CLI command, a
new capability row, or a control protocol field.

## Exact files touched

- `crates/thegn-svc/src/voice.rs` — new generic command provider implementation,
  cheap offline probe, argv validation, bounded transcript command, and unit
  tests; vendor aliases, if any, stay here.
- `crates/thegn-svc/src/lib.rs` — register the voice module.
- `crates/thegn-svc/src/seam/registry.rs` — add one `voice_probes` projection to
  the existing doctor registry.
- `crates/thegn-host/src/voice.rs` — new host controller, command capture/
  transcription workers, bounded child cancellation, QoS/CPU-slice launch,
  result channel, and mode-chip decoration helper.
- `crates/thegn-host/src/handlers/mod.rs` — register the I/O-free voice drain.
- `crates/thegn-host/src/handlers/voice.rs` — new loop-side reducer/effect
  drain, action dispatch, stale-result/focus checks, status/toast behavior, and
  unit tests; no process or audio I/O in this module.
- `crates/thegn-host/src/main.rs` — register the host voice module.
- `crates/thegn-host/src/run.rs` — initialize/drain voice state, delegate the
  two action arms, and expose only the narrow existing paste helper visibility
  needed by the handler; keep provider logic out of this pinned file.
- `crates/thegn-host/src/keymap.rs` — add `VoiceToggle`/`VoiceCancel`, key
  round-trip, and free default chord dispatch coverage.
- `crates/thegn-host/src/keymap_specs.rs` — add the two action specifications
  and claims; use the actual registered default chords.
- `crates/thegn-host/src/e2e_freeze.rs` — force voice disabled/idle under
  `THEGN_E2E` and test that the freeze is a no-op outside e2e mode.
- `docs/help/voice.md` — new help page covering lifecycle, contracts, safety,
  privacy, repair, no auto-submit, and an explanation of all five `[voice]`
  config keys.
- `crates/thegn-host/src/help/pages.rs` — register the new help page.

## Approach

1. Implement `CommandVoiceProvider` against chunk 1’s object-safe sync seam.
   Probe only checks configuration and executable availability; it never starts
   capture/transcription or performs a network request. Run argv directly with
   piped stdin/stdout/stderr and bounded output, never through a shell.
2. On an explicit toggle, snapshot the focused pane and request id, then spawn
   capture off-loop with `wrap_background_argv` and `platform::qos::Utility`.
   Capture stdout is one complete WAV; drain stderr; terminate on stop, cancel,
   max duration, child error, and shutdown. Keep bytes in RAM only.
3. On stop, submit the bounded WAV to the provider on another off-loop worker
   under the same CPU slice. Send only state/result messages via a channel and
   `TerminalWaker`; all reducer transitions and rendering/model updates happen
   on the loop. Never add a timed `poll_input` site or block before the first
   frame.
4. Inject only a successful, non-empty transcript into the original pane if it
   is still live and focused. Call the existing hardened paste helper so daemon
   panes use the existing stream relay. Never append newline, submit, parse
   transcript as a thegn action, or log transcript/audio content. Focus changes,
   cancellation, stale request ids, malformed UTF-8, timeout, and provider
   failure all discard safely and return to idle with a short status.
5. Add actions and help claims through the existing registries. Select a free
   `Alt v` toggle chord and a free cancel chord after the dispatch ratchet; Esc
   must cancel while recording. Decorate the existing mode chip only while
   recording using `crate::caps::glyph(...)` and existing accent styling. Do
   not add a `FrameModel` field or `BarBadge`: the mode chip is already
   persistent, pure, and rendered in the shared statusbar path.
6. Add the generic registry probe so both doctor text and JSON show the seam.
   Leave completion-slot, control-schema, surface-gap, help-context, and panel
   prose ratchets unchanged and document that no delta is correct: there is no
   CLI, wire verb, panel, or new external capability.

## Overlap/dependency

No file overlaps chunk 1. This chunk depends on chunk 1’s `VoiceConfig`,
`Feature::Voice`, reducer, and provider contract, so it must run serially after
chunk 1. No other coder should edit these host/service/help paths in parallel.

## Tests to run

- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc voice`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host voice`
- `cargo nextest run -p thegn-host keymap`
- `cargo nextest run -p thegn-host statusbar`

For any doctor smoke invocation, set `XDG_STATE_HOME` to a fresh temporary
directory; never migrate or run the built binary against the live state DB.
Do not run `just test`, `just ci`, a full-workspace compile, or e2e.

## Done criteria

- Provider probe is cheap/offline and reports disabled/unconfigured/missing
  executable/ready through the existing generic doctor text and JSON paths.
- Capture and transcription never run on the loop; every child is argv-only,
  CPU-sliced, bounded, cancellable, stderr-drained, and absent when disabled.
- Core reducer is the sole state authority; stale/cancelled results cannot
  inject; injection uses the existing paste/daemon path, focused-pane check, no
  newline, and no auto-submit.
- Recording status is visibly persistent, ASCII-safe through the caps chokepoint,
  and e2e-frozen without changing idle frame output. Default chords dispatch and
  action/help registries round-trip.
- `docs/help/voice.md` is registered and documents every user-visible behavior,
  including all five `[voice]` keys (the TOML example remains the complete
  schema-facing reference).
  No new CLI/completion/control/catalog/help-context/panel-prose ratchet is
  added because no such surface exists.
- No `thegn-voice`, whisper/ggml/audio dependency, model download/store,
  migration, platform cfg, cloud integration, or native STT is introduced.
- The coder commits exactly with subject:
  `feat(the-59): wire external voice command mode`
