# THE-59 architecture design: embedded voice mode

Status: implementation-ready architecture for the first honest slice

## Decision

Ship voice as an optional, experimental, command-backed provider seam. Thegn
owns the state machine, lifecycle, safety boundary, focused-pane injection,
doctor report, keybinds, and documentation. A configured external command owns
microphone capture and speech-to-text. No whisper, ggml, cpal, hound, model,
audio, GPU, cloud, or C/C++ dependency enters the workspace.

The first UX is toggle-to-talk: press the toggle action to start recording and
press it again to stop and transcribe. `Esc`/cancel discards the utterance.
True key-release push-to-talk and always-on listening are deliberately deferred:
the current terminal input path exposes key presses, not portable key releases,
and always-on capture would violate the explicit-consent and 0%-idle posture.

The command contracts are deliberately boring and portable:

```text
[voice] capture_command = ["program", "arg", "..."]
  start: program writes one complete 16-bit PCM WAV to stdout
  stop/cancel/max: thegn terminates the process; partial data is discarded

[voice] command = ["program", "arg", "..."]
  stdin: one complete WAV from capture_command
  stdout: UTF-8 transcript, with no protocol wrapper
```

Arguments are argv arrays, never shell strings. Vendor names may appear in
provider implementation documentation/examples only; they do not appear in
core, the capability catalog, or a vendor-specific config enum.

## Repository audit and draft verification

The draft under `openspec/changes/add-voice-mode/` was treated as a proposal to
verify, not as an implementation plan. The following repository facts govern
this design:

- The workspace has no voice leaf and no audio dependency: the workspace
  members are listed in `Cargo.toml:1-18`, and host features/dependencies are
  in `crates/thegn-host/Cargo.toml:31-46`. Adding `thegn-voice`, cpal, hound,
  whisper-rs, ggml, or a native feature would widen the cross/MSRV/build
  surface contrary to the issue framing.
- The substrate-free seam vocabulary already exists in
  `crates/thegn-core/src/seam.rs:22-24,26-80,82-173`: object-safe sync probe
  contracts, `SeamError` classification, `ProbeReport`, and `Kind`. The new
  voice contract belongs beside this vocabulary and must not import tokio,
  termwiz, audio, or process APIs.
- Runtime provider discovery is centralized in
  `crates/thegn-svc/src/seam/registry.rs:24-40`; doctor projects those generic
  reports in `crates/thegn-host/src/cmd/doctor.rs:309-331,1318-1394`. Voice
  adds one registry projection, not a second doctor surface.
- The capability catalog is already singular in
  `crates/thegn-core/src/capability.rs:1-17,106-150,211-214`. `sessions.input`
  is the existing `Verb::SendInput` row. Voice is a local UI action and adds
  no verb, MCP method, control-schema field, completion value, or
  `--allow-session-input` bypass. If a future remote voice caller is added, it
  must reuse that existing interlock (`crates/thegn-core/src/mcp/state.rs:253-260`).
- Injection is already hardened and nonblocking:
  `crates/thegn-host/src/run.rs:228-241` delegates to
  `pane_writer::build_paste_bytes`, whose marker neutralization is at
  `crates/thegn-host/src/pane_writer.rs:182-205`; pane writes queue to the PTY
  or daemon stream at `crates/thegn-host/src/pane.rs:738-785`. Voice calls this
  path, never writes terminal bytes directly, never appends a newline, and
  never auto-submits.
- The 0%-idle and off-loop patterns are established by the media worker
  (`crates/thegn-host/src/media_ctl.rs:74-164`) and the CPU-slice wrapper
  (`crates/thegn-core/src/sandbox_cpucap.rs:568-574,610-633`). Capture and
  transcription use channel + `TerminalWaker`, run only after explicit start,
  and invoke the configured commands under the existing background CPU slice.
- Help and action registries are already ratcheted:
  `crates/thegn-host/src/keymap.rs:2137-2194` checks action/spec round trips,
  `crates/thegn-core/src/help/frontmatter.rs:1-41` defines action claims, and
  `crates/thegn-host/src/help/pages.rs:9-45` is the page registry. The new
  actions must be registered and claimed together.
- `FrameModel` is a large renderer-agnostic model
  (`crates/thegn-host/src/chrome.rs:362-680`) with many fixture literals. The
  statusbar's left mode chip is already pure and capability-aware at
  `crates/thegn-host/src/statusbar_left.rs:24-39`; decorate that existing chip
  while recording rather than add a field and force fixture/schema churn.

Already satisfied by existing code and therefore not to be reimplemented:

1. The hardened focused-pane paste/daemon relay path.
2. The one capability catalog and its session-input interlock.
3. Core seam/probe/error vocabulary and the generic doctor projection.
4. Channel/waker and background CPU-slice patterns.
5. Keymap/action/help ratchets and terminal capability glyph/color chokepoints.
6. No state-db migration is needed; voice state is transient.

The proposal's cpal capture, native `whisper_rs`, `whisper_cli` default kind,
managed direct-URL artifacts, model table/store/download CLI, recording files,
silence RMS logic, grants, `thegn-voice` leaf, host feature, and microphone
permission wrapper are cut. They are not harmless optional details: they add
audio/C-build dependencies, model supply-chain and retention policy, new
managed-tool semantics, filesystem state, or platform-specific surface before
the seam has an implementation. The generic external command is the complete
first provider; a later provider can be registered without changing core or
the UI lifecycle.

## Shape of the change

### Core: pure contract and state machine

Add `crates/thegn-core/src/voice.rs` with no substrate imports:

- `VoiceProvider`: object-safe, synchronous, `Send + Sync`, with
  `caps()` and `transcribe_push_to_talk(&self, wav: &[u8])`; it also exposes
  the existing `Probe`/`Kind` information through the concrete provider. The
  operation returns a classified `VoiceError` implementing `SeamError`.
- `VoiceState`: `Idle`, `Recording { pane_id, started_at }`, and
  `Transcribing { pane_id, request_id }`.
- Pure events/effects for start, stop, capture completion, transcript success,
  failure, cancel, and maximum duration. The reducer is the only authority for
  legal transitions. It records the originating pane and a monotonically
  increasing request id so stale results cannot land after cancel/focus change.
- The reducer emits effects (`StartCapture`, `StopCapture`, `Transcribe`,
  `Inject`, `Notify`) and performs no I/O, sleeping, allocation policy, terminal
  work, or process spawning. Unit tests cover every transition, duplicate
  events, cancel, max-duration, stale-result rejection, and empty/whitespace
  transcript.

Add `crates/thegn-core/src/config_voice.rs` with only these keys:

| key               | default     | contract                                                     |
| ----------------- | ----------- | ------------------------------------------------------------ |
| `enabled`         | `false`     | Experimental feature gate; no workers when false.            |
| `kind`            | `"command"` | Only the generic command provider is implemented.            |
| `capture_command` | `[]`        | External argv producing one WAV on stdout.                   |
| `command`         | `[]`        | External argv consuming WAV stdin and producing text stdout. |
| `max_seconds`     | `30`        | Hard upper bound, clamped to a safe finite range.            |

An empty command or capture command is not an error in config parsing. It is a
valid unconfigured seam: the provider reports `Unavailable`/not configured,
actions explain the missing setting, and doctor shows the repair. Strict config
validation still rejects unknown enum values using the existing `config_enum!`
rules. `Feature::Voice` is `Experimental`; stable-channel clamping disables it
with the normal channel note.

### Service/provider edge

Add a small `thegn-svc` command provider. It checks argv configuration and the
first executable for doctor, then runs the transcriber synchronously when called
by the host worker. It must not contain audio libraries or a shell. Vendor
aliases, if documented, are confined to this implementation module. The probe
is cheap and offline; it never starts a transcriber or captures a microphone.

Add `voice_probes(cfg)` to the existing service seam registry. When voice is
disabled and unconfigured it may stay quiet like other disabled optional
providers; when enabled or partially configured it must produce one generic
`voice`/`command` report explaining ready, missing capture command, missing
transcriber, or missing executable. This automatically reaches text and JSON
doctor projections.

### Host lifecycle and event loop

Add a host voice controller and an I/O-free loop drain handler. The handler owns
the reducer, request id, originating pane, child cancellation handles, and
result channel. The loop performs only reducer transitions, model/status
updates, and the existing pane injection. It never calls `Command`, waits,
reads audio, or blocks on a mutex.

On toggle start, snapshot the focused pane id and dispatch a background capture
job. The job starts `capture_command` under
`thegn_core::sandbox_cpucap::wrap_background_argv`, uses the `Utility` QoS
class, drains bounded stdout/stderr, and sends only lifecycle/results through a
channel plus `TerminalWaker`. On toggle stop or max duration it terminates the
child and hands the complete in-memory WAV to the transcriber worker. Cancel
kills/discards without transcription. Both commands have bounded output,
termination on all cancel/error paths, and no transcript/audio logging.

The worker invokes the service provider under the same background CPU slice.
No audio is persisted. A failed command, malformed/empty output, timeout, or
missing provider returns to `Idle` with a short status/toast; it never wedges
the loop. A successful transcript is trimmed, dropped if empty, and injected
only if the original pane is still live and focused. It uses the existing paste
helper with no newline. If focus changed, the text is discarded with a visible
status rather than sent to a surprising pane.

The `voice-toggle` and `voice-cancel` actions are added to `Action`, key
round-trip, `ACTION_SPECS`, and the default keymap only after the default-chord
dispatch ratchet passes. Use a free `Alt v` toggle chord and a free cancel chord
(or Esc while recording); do not steal an existing chord. The action arm in
`run.rs` delegates immediately to the new handler so the pinned file does not
accumulate provider logic.

While `Recording`, decorate the existing mode chip with a short recording token
using `crate::caps::glyph(...)`/the active glyph set and the existing accent
slot. Restore the ordinary mode chip in every other state. This is a persistent
statusbar signal, not a transient status message, and it requires no new
`BarBadge`, fit priority, `FrameModel` field, or raw glyph literal. The
`THEGN_E2E` freeze must force voice disabled/idle, and the handler must have a
unit test proving idle frames remain unchanged.

### Docs and ratchets

Add `docs/help/voice.md`, register it in `help/pages.rs`, and document:

- toggle-to-talk lifecycle and cancel;
- `Alt v`/the actual registered chord as resolved by the keymap;
- both argv contracts and the fact that thegn ships no STT/model/audio;
- no cloud guarantee from thegn itself, but explicit warning that an arbitrary
  user command can send audio elsewhere;
- no newline/auto-submit and focused-pane safety;
- `thegn doctor` and configuration repair.

The help page must also enumerate and explain all five `[voice]` keys, while
`config/config.toml.example` remains the complete schema-facing reference.
Update the
env-overlay ratchet for the five structured keys (no environment aliases are
added; vector commands and explicit voice consent are config-only). The
completion-slot, control-schema, surface-gap, help-context, and panel-prose
ratchets remain unchanged because this slice adds no CLI, wire verb, panel, or
external capability. The coder must run their tests and record that no delta
was required rather than inventing a no-op catalog row.

## Invariants and failure behavior

| situation                                 | behavior                                                               |
| ----------------------------------------- | ---------------------------------------------------------------------- |
| disabled, stable channel, or unconfigured | no child, no mic, no wake source; status explains how to configure     |
| capture command missing/fails             | return to idle; preserve the pane; doctor names the missing executable |
| transcriber missing/fails/non-UTF-8/empty | return to idle; status/toast only; never inject partial output         |
| max duration                              | terminate capture, transcribe the complete bounded WAV if available    |
| cancel                                    | terminate, discard bytes and result; stale worker output is ignored    |
| focus/pane changes                        | discard transcript rather than inject into another pane                |
| command emits too much output or hangs    | cap/drain/terminate off-loop; loop remains responsive                  |
| terminal has ASCII glyphs                 | recording token degrades through `caps`, with no mojibake              |

There is no model download, SQLite schema change, daemon protocol change,
MCP/control capability, cloud credential, or platform `cfg` addition in this
slice.

## Chunk order and integration

Two chunks are intentionally serial and file-disjoint. Chunk 2 consumes the
public `VoiceConfig`, `Feature::Voice`, reducer, and provider contract from
chunk 1, so the Lead must run it after chunk 1. No other coder should edit the
listed files concurrently. Each chunk below is self-contained and names its
own scoped tests and exact commit subject.
