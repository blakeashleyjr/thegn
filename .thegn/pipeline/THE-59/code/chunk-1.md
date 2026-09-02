# THE-59 chunk 1 — core voice contract, reducer, and config

## Scope

Create the substrate-free voice provider seam, pure idle/recording/transcribing
state machine, experimental feature gate, minimal config, and all config
documentation/ratchet updates. Do not add host process handling, audio code,
doctor wiring, keybinds, help pages, or a new capability row.

## Exact files touched

- `crates/thegn-core/src/voice.rs` — new pure provider contract, errors, states,
  events/effects, reducer, and unit tests.
- `crates/thegn-core/src/config_voice.rs` — new `[voice]` config type and strict
  `kind = "command"` enum.
- `crates/thegn-core/src/lib.rs` — register the two new modules.
- `crates/thegn-core/src/config.rs` — re-export/config field/default.
- `crates/thegn-core/src/channel.rs` — add `Feature::Voice` as Experimental,
  id/all/stability/allowed coverage and tests.
- `crates/thegn-core/src/config_resolve.rs` — stable-channel clamp for voice.
- `crates/thegn-core/src/config_tests.rs` — channel clamp/default assertions.
- `crates/thegn-core/src/config_validate.rs` — deliberate config-enum count and
  reachability ratchet update for `VoiceKind`.
- `config/config.toml.example` — document every `[voice]` key and both command
  contracts, including the no-bundled-STT/privacy posture.
- `test/env-overlay-ratchet.txt` — pin the five structured voice keys with the
  existing shrink-only-ratchet convention; do not add environment aliases.

## Approach

1. Keep `thegn_core::voice` independent of tokio, termwiz, command/process
   APIs, audio, filesystem, time sources, and vendor names. Use deterministic
   integer timestamps/durations supplied by the host.
2. Make the reducer explicit: `Idle → Recording → Transcribing → Idle`, with
   cancel and stale request-id paths returning to idle. Record the originating
   pane id; emit effects, never perform them. Empty/whitespace transcripts must
   emit no injection.
3. Define `VoiceProvider` as an object-safe synchronous operation with
   `Probe`/`Kind`-compatible concrete implementations and classified
   `VoiceError`. Do not use `async fn` in the public trait.
4. Keep config small: `enabled`, `kind = "command"`, `capture_command`,
   `command`, and bounded `max_seconds`. An empty command is unconfigured, not
   a parse failure. Do not add model, language, device, silence, recording,
   grant, URL, or retention keys.
5. Update the generated-schema count and env overlay ratchets in this same
   commit. Do not add a capability catalog row, completion slot, control-schema
   entry, or help ratchet line.

## Overlap/dependency

No overlap with chunk 2: all paths above are owned exclusively by this chunk.
Chunk 2 depends on this chunk’s public config/types and therefore runs serially
after this commit. Chunk 1 itself has no dependency on chunk 2.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core voice`
- `cargo nextest run -p thegn-core channel`
- `cargo nextest run -p thegn-core config`

Do not run `just test`, `just ci`, a full-workspace compile, e2e, or the built
binary. If a local config command is needed, set `XDG_STATE_HOME` to a fresh
temporary directory first.

## Done criteria

- Core tests cover every reducer transition, duplicate/cancel/stale result,
  max-duration, and empty transcript path; no core module imports host/runtime
  substrate.
- `VoiceKind` is strict-checked and its schema reachability/count ratchet is
  updated deliberately; stable config clamps `voice.enabled` off.
- All five keys and command contracts appear in the example config; the env
  overlay ratchet accounts for all five keys.
- No audio/whisper/ggml/cpal/hound dependency, model store, migration,
  capability row, completion slot, control schema, or host file is introduced.
- The coder commits exactly with subject:
  `feat(the-59): add voice provider contract and state machine`
