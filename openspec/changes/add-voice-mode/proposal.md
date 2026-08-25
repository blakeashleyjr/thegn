# Voice mode — toggle-to-talk speech-to-prompt

Linear: THE-59

## Summary

An **optional, strictly additive** voice-input mode: press a keybind, speak,
press it again (or go silent), and the on-device transcription of what you said
is injected into the focused pane as ordinary input — exactly as if it had been
typed, and never auto-submitted. No cloud service, no API key, no audio leaving
the machine by default.

The pieces, in one pass:

- **Capture**: a toggle-to-talk action starts a mic capture on a background
  thread (cpal), with a statusbar recording indicator, a hard max duration, and
  optional silence auto-stop. (True hold-to-talk needs key-release events,
  which terminals do not deliver portably — see the design; toggle is the
  honest primitive.)
- **Transcription**: a provider seam, not a hard whisper dependency. The
  default kind shells out to a `whisper-cli` binary (whisper.cpp) resolved by
  the managed-tools three-tier story; a generic `command` kind runs any
  user-configured WAV-in/text-out program; a native in-process `whisper_rs`
  kind is reserved for a later phase behind its own cargo feature.
- **Models**: ggml models (75 MiB tiny → 2.9 GiB large-v3, incl.
  large-v3-turbo ~1.6 GiB) acquired through a new managed-tools **direct-URL
  artifact source** — grant-checked (`download_file`), SHA256-pinned, with a
  size warning + explicit confirmation before multi-hundred-MB downloads.
- **Injection**: the transcript goes through the existing hardened paste path
  (`paste_text_into_pane` → `pane_writer::build_paste_bytes`) into the focused
  pane — one bracketed-paste-wrapped write that rides the daemon Stream
  transport when the daemon owns the PTY, so it lands in the durable session.
  No trailing newline is ever appended; the user reviews and presses Enter.

The default build carries **zero whisper weight**: everything above sits behind
a `voice` cargo feature (a new `thegn-voice` leaf crate, following the
`thegn-media` pattern), the default transcriber is a subprocess, and with
`[voice] enabled = false` (the default) no thread, no device, and no wake
source exists at all.

## Impact

- **Roadmap**: this deliberately **reopens cut item 499 ("voice")** in group
  **AP (Long-horizon bets & modes)** — `tasks.md` lists it under "Deliberate
  defers / cut candidates". The proposal keeps the cut's spirit: optional,
  additive, zero cost when absent. The audit phase should add an AP item
  (~499) linking here.
- **Spec**: new `voice` capability. `managed-tools` — ADDED a direct-URL
  artifact source (grant-checked file downloads with size + SHA256).
  `capability-grants` is reused unchanged (`download_file` kind already
  exists). No `state-db` change — voice keeps no DB state.
- **Capability catalog**: **no new rows.** Voice is an in-UI feature; external
  injection into a session already exists as `Verb::SendInput`
  (`thegn session send`). Model management is a local CLI verb like
  `thegn mcp install`, not a control-plane verb.
- **Code (sketch)**: new leaf crate `crates/thegn-voice/` (cpal capture, WAV
  encode, `whisper-cli` + `command` transcriber drivers; core-free like
  `thegn-media`); `thegn-core/src/{voice.rs,config_voice.rs}` (pure model
  table, silence/stop decision, transcript post-filter, config);
  `thegn-host/src/handlers/voice.rs` + statusbar badge; managed-tools source
  extension in `thegn-core` (decide) + `thegn-host/src/managed_tool.rs`
  (fetch); `cmd/voice.rs` (`thegn voice model list|pull|rm`, `thegn voice
status`).
- **Feature flags**: `voice` on `thegn-host` (off by default, like
  `profiling`); a later `voice-native` for whisper-rs. `[voice]` registers as
  **Experimental** in the channel registry (`thegn_core::channel`), so the
  stable channel clamps it off with a status note.
- **Actions/help**: two new action ids (`voice-toggle`, `voice-cancel`) →
  ACTION_SPECS entries + a new `docs/help/voice.md` claiming both (help +
  prose ratchets). Config keys documented in `config/config.toml.example`
  (generated config-reference page picks them up).
- **In-flight overlap**: none directly. `add-skills-registry` and
  `add-agent-task-engine` do not touch audio or input injection;
  `make-daemon-default` strengthens the injection story (daemon-owned panes)
  but this change works either way. The MCP write-tools branch (`--scopes`
  gating) is orthogonal — voice adds no MCP surface.

## Rationale

The issue's reference implementation (tuicommander) proves the UX: whisper-rs
on-device, push-to-talk hotkey, text injected into the active terminal, four
model sizes. Three findings from researching its stack shaped this proposal
away from a straight port:

1. **whisper-rs is a liability as a hard dependency.** The GitHub repo is
   archived (maintenance moved to Codeberg), and it builds whisper.cpp from a
   git submodule via cmake — a C++ toolchain in every build, a new failure
   mode for `check-cross`/`check-msrv`/nix-build, and megabytes of binary for
   a feature most users won't enable. As a _seam impl behind its own feature_
   it is fine; as the foundation it fails the "seams, not vendors" test.
2. **A subprocess default fits the house rules better.** whisper.cpp's
   `whisper-cli` takes a 16-bit 16 kHz WAV and prints text — a perfect
   vendor-CLI-inside-the-impl-file seam. It is packaged (nixpkgs `whisper-cpp`,
   homebrew), so the managed-tools PATH tier frequently resolves it for free;
   toggle-talk is inherently batch (record, then transcribe once), so the
   subprocess latency profile is acceptable; and GPU acceleration
   (Metal/Vulkan/CUDA) is the _binary's_ build concern, not thegn's.
3. **Push-to-talk (hold) is not portably implementable in a TUI.** Terminals
   deliver key presses, not releases; only the kitty keyboard protocol reports
   release events and thegn's input path does not negotiate it. Every honest
   terminal voice tool is toggle- or VAD-driven. So the primitive is
   **toggle-to-talk** with silence auto-stop — hold-to-talk is a possible
   future kitty-protocol progressive enhancement, not scoped here.

Models are plain file downloads from Hugging Face
(`huggingface.co/ggerganov/whisper.cpp`) with published checksums — squarely
the managed-tools + capability-grants story (`download_file` kind), missing
only a non-executable direct-URL source, which is a small, generally useful
extension (fonts, tree-sitter grammars, and future model-shaped artifacts want
the same thing).

## Non-goals

- **Always-on / wake-word listening.** Capture runs only between an explicit
  start and stop. The mic is never open outside that window.
- **Streaming partial transcripts.** Batch per utterance; a resident
  `whisper-server` mode is noted as a future latency optimization, not scoped.
- **Voice _commands_.** The transcript is inert text into a pane — never
  parsed into thegn actions. (A "computer, merge main" grammar would be a
  separate proposal with its own security story.)
- **Cloud STT as a built-in.** The generic `command` kind lets a user wire
  anything — including a cloud CLI — explicitly; thegn ships no cloud backend
  and the default posture is on-device only.
- **Hold-to-talk key-release semantics** (kitty protocol) — future work.
- **Translation, diarization, custom fine-tunes.** Transcribe-only, one
  speaker assumed; the model table covers the stock ggml set.
- **Making the shell depend on voice.** Everything is feature-gated and
  config-gated; the AI-free/additive rule from CLAUDE.md applies unchanged.
