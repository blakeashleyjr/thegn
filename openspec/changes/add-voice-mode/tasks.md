# Tasks — voice mode

## 1. Pure core (thegn-core)

- [ ] 1.1 `config_voice.rs`: `VoiceConfig` + `Default` + re-export, following
      the `config_ci.rs` sibling-module pattern; `config_enum!` for the
      transcriber `kind` (`whisper_cli` / `command` / reserved `whisper_rs`) —
      extend the pinned `config_enum` count test.
- [ ] 1.2 `voice.rs`: pinned model table (name → HF URL, `size_bytes`,
      `sha256`) for tiny/base/small/medium/large-v3/large-v3-turbo + `-q5_0`
      variants; model resolution decision (`model_path` override → managed
      store path); size-warning decision (threshold + confirm requirement).
- [ ] 1.3 Silence-stop decision function over per-frame RMS values
      (`silence_stop`, `silence_ms`, `max_seconds`) — pure, table-tested.
- [ ] 1.4 Transcript post-filter: trim, strip trailing newline, drop whisper
      noise tokens (`[BLANK_AUDIO]` etc.) — pure, table-tested.
- [ ] 1.5 Register voice as `Stability::Experimental` in the channel registry;
      extend `Config::clamp_to_channel` (+ clamp-note test).
- [ ] 1.6 Unit tests for all of the above (95% line gate applies).

## 2. Managed-tools `UrlArtifact` source

- [ ] 2.1 Core: add the `UrlArtifact` variant (url, `sha256`, `size_bytes`,
      non-executable) to the managed-tool source; resolution/marker logic +
      unit tests (no PATH tier for artifacts).
- [ ] 2.2 Host (`managed_tool.rs`): download + SHA256 verify before
      move-into-place + version marker; refuse without a matching
      `download_file` grant; size warning + explicit confirm above the
      threshold. Off the event loop (CLI path / worker).
- [ ] 2.3 Extend `thegn doctor` managed-tools reporting to artifact entries.

## 3. `thegn-voice` leaf crate

- [ ] 3.1 New `crates/thegn-voice/`: core-free leaf (the `thegn-media`
      pattern); deps `cpal` + `hound` only; config lowered in as a plain opts
      struct. Add to the workspace + `crate_boundaries.rs` expectations.
- [ ] 3.2 Capture driver: device open (named or default), 16 kHz mono
      downmix/resample, RAM ring buffer, per-frame RMS out, hard stop on cap.
      State transitions (not buffers) over a channel.
- [ ] 3.3 WAV encode (16-bit PCM, 0600 temp file under `$XDG_RUNTIME_DIR`,
      state-dir fallback; delete-on-every-exit-path).
- [ ] 3.4 Transcriber seam trait (object-safe, sync `&self`) + `whisper_cli`
      impl (vendor CLI confined to its impl file) + `command` impl; `SeamError`
      classification (`NotInstalled` → doctor guidance).
- [ ] 3.5 Probe: resolved binary/model/kind, device availability, per-platform
      permission notes (macOS TCC attribution to the hosting terminal).

## 4. Host wiring (thegn-host, behind the `voice` feature)

- [ ] 4.1 `voice` cargo feature (off by default, `profiling` precedent)
      carrying the optional `thegn-voice` dep; feature-off build keeps actions
      unregistered and config inert-but-accepted.
- [ ] 4.2 `handlers/voice.rs`: toggle/cancel state machine; capture thread +
      transcribe worker spawn (QoS `Utility`), channel drain, waker pulse.
- [ ] 4.3 whisper-cli resolution through the managed-tools three-tier order
      (`[managed_tools.whisper-cli]` override → PATH → managed, where assets
      exist).
- [ ] 4.4 Injection via `paste_text_into_pane` (focused pane at injection
      time; status-line fallback when focus moved/pane died; never a trailing
      newline).
- [ ] 4.5 Statusbar REC badge (state-transition damage only; optional elapsed
      readout on the existing 2 s ticker); pin the badge in `e2e_freeze.rs`.
- [ ] 4.6 Two `ACTION_SPECS` entries (`voice-toggle`, `voice-cancel`) with
      keywords + default chords clearing
      `declared_default_chords_actually_dispatch`; palette-gated on enabled.
- [ ] 4.7 Failure surfacing: transcriber/device errors to `model.status` +
      notification — never swallowed (primary-path rule).

## 5. CLI + doctor

- [ ] 5.1 `cmd/voice.rs`: `thegn voice model list|pull|rm` (`--json`,
      `--yes` for the size confirm), `thegn voice status`; refuse with
      guidance when the feature is not compiled or `enabled = false`.
- [ ] 5.2 Doctor: voice probe rows (kind, binary tier, model tier,
      pinned-vs-installed, device, `command`-kind labeled user-provided).
- [ ] 5.3 Smoke-test coverage for the CLI verbs (`test/smoke.sh`) — gated on
      the feature being compiled in the smoke build, else skipped.

## 6. Docs, config, validation

- [ ] 6.1 Document every `[voice]` key + `[[voice.grants]]` +
      `[managed_tools.whisper-cli]` in `config/config.toml.example`
      (`tests/config_example.rs` gates it); env-overlay knobs or ratchet pins
      per `tests/env_overlay_coverage.rs`.
- [ ] 6.2 New `docs/help/voice.md` claiming both action ids with real prose
      (help + prose ratchets), incl. the mic-permission-per-platform notes and
      the never-auto-submit rule.
- [ ] 6.3 `git add` all new crates/modules before nix-build (flake source
      allowlist sees only git-tracked files); check the crane source allowlist
      covers `crates/thegn-voice/`.
- [ ] 6.4 Run `just ci` once, at the end (includes openspec validate).

## 7. Phase 2 (deferred, separate follow-up)

- [ ] 7.1 `whisper_rs` native kind behind a `voice-native` feature (resident
      context, per-platform GPU features; excluded from check-cross/MSRV/nix
      default) — un-reserve the enum value when it lands.
- [ ] 7.2 Optional resident `whisper-server` kind for large-model latency.
