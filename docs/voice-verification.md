# Dictation verification and release gates

The first-party implementation is experimental. Command capture/transcription
predates this work. Neither historical THE-59 nor the old native proposal is
evidence that native dictation was delivered. THE-148 owns reconciliation in
Workspace Product & Observability. See the rewritten OpenSpec tasks and local
tracker reconciliation below. No release dates are asserted.

## Build and distribution matrix

| Target                    | Candidate feature | Runtime                             | Release state            |
| ------------------------- | ----------------- | ----------------------------------- | ------------------------ |
| Linux desktop             | `voice-native`    | CPAL default input; Whisper CPU     | Verification pending     |
| Apple silicon macOS 14.2+ | `voice-metal`     | CPAL/CoreAudio; Metal, CPU fallback | Verification pending     |
| Static remote bridge      | neither           | No microphone/Whisper dependencies  | Keep native features off |

The native leaf pins CPAL 0.18.2 and whisper-rs 0.16.0. Linux builds require ALSA
development files/pkg-config, CMake, a C++ toolchain, and libclang for bindings.
Apple builds require Xcode tools, CMake, the 14.2+ SDK/deployment target, and Metal
frameworks for `voice-metal`. Do not enable these features in desktop releases
until the matrix below has actual evidence. Current release/bridge flags stay
unchanged. The stable channel continues to clamp `[voice] enabled` off.

## Required evidence

The lockfile now resolves the pinned native dependencies online. Locked Linux
native-host compilation, focused tests, and the workspace all-features check
passed. The development/CI shells supply CMake, pkg-config, ALSA headers on
Linux, and libclang. Native distribution flags remain off.

Reproducible checks (enter `nix develop` first):

```sh
cargo check --workspace --all-features --locked
cargo test --locked -p thegn-core -p thegn-svc -p thegn-host --features thegn-host/dev,thegn-host/voice-native voice
cargo test --locked -p thegn-voice --features native
cargo build --locked -p thegn-host --features dev,voice-native
python3 test/voice-native-worker.py target/debug/thegn
# Apple silicon: MACOSX_DEPLOYMENT_TARGET=14.2 cargo build --locked -p thegn-host --release --features dev,voice-metal
```

The service HTTP loopback tests are ordinary tests, including multipart fields,
auth/rate/server errors, redirect refusal, malformed/excess responses, refused
connections, cancellation and a body that stalls after headers. The Linux
worker fixture uses an ALSA null input, never the physical default microphone.
It checks final-only request counts, serialized previews, final replacement,
stop/EOF/crash teardown, native-command WAV compatibility, preflight and disabled
behavior. Synthetic audio does not prove real routing or microphone permissions.

For each platform record machine/OS/CPU/RAM, model and SHA-256, build command,
coding workload, audio device/routing, test corpus, raw timings, and peak worker
RSS. Measure warm previews from captured-audio end to display, report p50/p95,
and target p95 <= 3 seconds (warm range 1–3 seconds). Measure model load and final
full-recording finalization separately. The worker records decode/finalization
timings through tracing; collect display arrival alongside host events. The
current implementation reloads the model per utterance; no warm-across-utterance
claim is made.

Exercise silence, noise, multilingual speech, identifiers, long utterances,
overlap duplication, and inference slower than capture. Verify one request at a
time and no work backlog. Confirm stop closes the device before final inference;
Esc/config replacement/UI exit kill the process; stale results do not inject;
focus changes hold final text; pane closure cancels active capture; a final already transcribing is held. Remove the
device, crash the worker, revoke macOS microphone permission, and disable voice.
There must be no worker, capture device, timer, or network activity while disabled.

Model tests must include corrupt, short, excessive, interrupted, and cancelled
streams, preserving an existing installation. HTTP tests must cover 401/403,
429, timeout, refused connection, malformed JSON, excess response size, stalled
body, cancellation and final-only behavior. Confirm credentials are absent from
argv, diagnostics, and protocol errors. Test Linux ALSA/PipeWire and macOS TCC on
real devices; containers and mock tests do not satisfy permission/routing gates.

## Linux supplied-audio measurements (2026-10-28)

Raw [timings](voice-evidence/linux-2026-10-28/inference.tsv),
[machine/build/source metadata](voice-evidence/linux-2026-10-28/machine.json), and
[worker fixture results](voice-evidence/linux-2026-10-28/worker-fixtures.txt)
are checked in. Machine: Ryzen AI 9 HX 370, Linux 7.2.9, about 94 GiB RAM;
`base.en` with the catalog SHA-256; fixed upstream 11-second JFK sample, last
8 seconds used for previews. Native host test compilation ran concurrently.

| Measurement                                                     | Result                                |
| --------------------------------------------------------------- | ------------------------------------- |
| Model load                                                      | 149 ms                                |
| Resident-model preview decode, 20 calls, nearest-rank p50 / p95 | 2,375 / 2,914 ms                      |
| Maximum preview decode                                          | 3,035 ms                              |
| Preview p95 after excluding first call (19 calls, nearest rank) | 3,035 ms                              |
| Full 11-second recording final decode                           | 2,378 ms                              |
| Peak inference process RSS                                      | 349,820 KiB (about 342 MiB)           |
| Pre-cancel / digital silence                                    | Passed; pre-cancel returned in < 1 ms |

These are decoder measurements, not capture-to-display acceptance or a broad
corpus. The tail is close to (and individual calls exceed) the 3-second target.
Physical Linux and Apple reports remain required.

The reference build explicitly used `GGML_NATIVE=ON`, `GGML_CCACHE=OFF`, and
`NIX_ENFORCE_NO_NATIVE=0` in a separate target directory. This is specific to
this CPU, not a portable distribution configuration. Nix's `SOURCE_DATE_EPOCH`
disables ggml's native/SIMD defaults; its compiler wrapper also rejects
`-march=native` by default. A successful generic smoke run took about 19 seconds
per preview and 18 seconds for the final pass, so generic packaging does not
meet the latency target. CPU optimization/dispatch and release-profile results
must be reviewed before enabling native desktop packages. Upstream build flags
are not all tracked by Cargo: use a fresh target directory when changing them.

To measure another supplied PCM16 mono/16 kHz corpus without opening a mic:

```sh
cargo run --locked -p thegn-voice --features native --example verify -- MODEL.bin AUDIO.pcm en 20
```

Record exact compiler/backend flags and use `metal` on the Apple reference host.
Do not extrapolate this single English clip to multilingual/noise/identifier
accuracy, real capture latency, model reuse across utterances, or macOS behavior.

A separate [default Linux input check](voice-evidence/linux-2026-10-28/default-input.json)
produced 2,560 frames of mono/16 kHz/PCM16 audio, with stop requested at 250 ms
and the worker reaped at 262 ms. Samples were discarded in RAM, without
transcription or network transmission. This confirms basic default-input
open/capture/stop on this machine; it does not establish the routing/corpus matrix.

The [CLI smoke checks](voice-evidence/linux-2026-10-28/cli-fixtures.txt) exercise
HTTP final-only setup, local embedded/command setup, status/model list/rm,
comment/table preservation and rejection of invalid language/missing local models
without changing the prior configuration. All CLI paths use isolated XDG roots.

## Tracker reconciliation

Read the live project, THE-148, THE-59, THE-449, existing voice/dictation issues
and milestones on 2026-10-28 before creating this tree in
[Workspace Product & Observability](https://linear.app/blakeashley/project/workspace-product-and-observability-fb0f99380fed):

| Issue                                                     | Scope                                                       | Depends on                  |
| --------------------------------------------------------- | ----------------------------------------------------------- | --------------------------- |
| [THE-1510](https://linear.app/blakeashley/issue/THE-1510) | First-party dictation parent                                | All children                |
| [THE-1511](https://linear.app/blakeashley/issue/THE-1511) | Feasibility: Linux CPU and Apple Metal measurements         | none                        |
| [THE-1512](https://linear.app/blakeashley/issue/THE-1512) | Contracts/UI, stale/cancel/focus/pane and safe final paste  | THE-1511                    |
| [THE-1513](https://linear.app/blakeashley/issue/THE-1513) | Native capture, isolated worker, inference, setup/models    | THE-1512                    |
| [THE-1514](https://linear.app/blakeashley/issue/THE-1514) | HTTP multipart/credentials/billing/final-only and failures  | THE-1512, THE-1513          |
| [THE-1515](https://linear.app/blakeashley/issue/THE-1515) | Real-platform latency, permissions, corpus and distribution | All four preceding children |

Created the milestone “Dictation: verified native and HTTP” with no target date
and assigned this tree and THE-148 to it. Parent THE-1510 relates THE-148,
historical THE-59 and legacy lifecycle THE-449. THE-148 now records the accepted
native/HTTP scope while preserving its superseded command-only description as
history. The parent and implementation/feasibility children remain In Progress;
the release child remains Backlog. No completed native-delivery claim is made.
`delivery/index.json` and `delivery/issues.json` map the same owners bidirectionally.
Attach a landed source revision and platform reports before closing the gates.

Sources: [whisper-rs API](https://docs.rs/whisper-rs/latest/whisper_rs/),
[CPAL requirements](https://docs.rs/crate/cpal/0.18.2),
[upstream streaming example](https://github.com/ggml-org/whisper.cpp/tree/master/examples/stream),
[model manifest at immutable revision](https://huggingface.co/ggerganov/whisper.cpp/tree/5359861c739e955e79d9a303bcbc70fb988958b1),
[Speaches multipart contract](https://speaches.ai/usage/speech-to-text/).

## Implementation evidence in this workspace

The unmodified workspace now resolves and builds the optional native dependency
graph. The focused native-host run passed 13 core tests, 19 service tests
(including all three live HTTP fixtures), and 18 host voice/protocol tests.
Six native audio/window/abort tests, config/reference/environment coverage, CLI intent tests
and crate-boundary tests supplement those checks. The Linux all-features check
also exercises `voice-metal` without enabling a Metal backend on Linux.
The 28 existing fenced config-writer tests also pass. The new setup regression
preserves table/key comments and clears removed optional fields. Repository ratchets, strict OpenSpec validation and delivery validation pass.

The CLI downloaded and verified `base.en` (147,964,211 bytes) into an isolated
XDG test store; personal voice configuration was not changed. Corrupt, short,
excessive, interrupted and cancelled acquisition fixtures preserve an existing
artifact. The worker fixtures pass with ALSA null and a local HTTP server. A shared
short-lived HTTP runtime bounds shutdown as well as the request future, so
blocking OS DNS cleanup cannot hold up a cancellation or deadline. A deterministic
blocking-resolver regression covers this; the OS lookup itself may finish in
the background, while the cancelled request retains no audio or credentials.

Actual inference exposed a defect in whisper-rs 0.16.0's safe abort helper: it
stores a double-boxed trait object but casts it back to the concrete closure
in its trampoline. The leaf uses a documented concrete FFI callback context
owned across the synchronous native call instead, with a regression test for
cancellation/deadline/ownership. The supplied-audio `verify` example exercises
real inference without opening a microphone. Metal startup also preflights
compute-state allocation before capture so a state-allocation failure can use
the CPU fallback. Apple behavior is unverified.

Native release acceptance remains open: broader ALSA/PipeWire routing, device
removal, macOS TCC/Metal/CPU fallback and packaging, broader speech/noise corpus,
and capture-to-display warm p95 under a recorded coding workload still require
platform reports. The current worker reloads the model per utterance. Native
desktop distribution flags remain off and the experimental channel remains.

The locked native-host build, workspace all-features check and native all-target
clippy gate passed. Final config-comment and HTTP-runtime refinements received
targeted core/service test and clippy runs. Physical/matrix release gates above
remain open.
