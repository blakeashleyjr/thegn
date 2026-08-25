# Design — voice mode

## Event loop, rendering, damage

- **Wake path**: two off-loop workers, both channel + `TerminalWaker`, both
  QoS-classed (`platform::qos`):
  - the **capture thread** (`Utility` — user-visible) owns the cpal input
    stream and an in-RAM ring buffer; it sends _state transitions only_
    (started / stopped / level-out-of-silence / error) to the loop — never
    per-buffer audio events, so a 30 s recording wakes the loop a handful of
    times, not thousands;
  - the **transcribe worker** (`Utility`) runs once per utterance (spawn the
    subprocess or the native call), sends one `VoiceMsg::Transcript {..}` /
    `VoiceMsg::Failed {..}`, and exits.
    While `[voice] enabled = false` (default) or the `voice` feature is not
    compiled, neither exists — the 0%-idle contract is untouched, and
    `idle_poll` gains no new timed poll site.
- **Damage channels**: the statusbar REC badge is chrome ⇒ state transitions
  mark the master dirty (a `Full` frame) — on start, stop, transcript-landed,
  and error only. An elapsed-seconds readout, if shown, rides the existing 2 s
  ticker rather than adding a wake source. The injected transcript itself is
  pane input ⇒ the ordinary `Panes` path.
- **e2e**: the REC badge and any elapsed counter are new volatile chrome —
  they must be pinned in `e2e_freeze.rs` (`THEGN_E2E=1` forces the
  no-recording state) or the 45 snapshots flap.
- **No SQLite change.** Voice persists nothing; the model store is files under
  `~/.thegn/models/whisper/` with a checksum marker, mirroring the managed
  tool layout.
- **Help context**: no new zone or panel section. The two action ids are
  claimed by a new `docs/help/voice.md`; the config keys surface on the
  generated config-reference page.

## Toggle-to-talk, not push-to-talk (the honest keybind)

Terminals deliver key _presses_; release events exist only under the kitty
keyboard protocol's progressive enhancement, which thegn's termwiz input path
does not negotiate and most terminals don't support. So "push-to-talk" is
specified as **toggle-to-talk**:

- `voice-toggle` (ACTION_SPECS; suggested default chord `Alt+v`, final chord
  chosen at implementation against `declared_default_chords_actually_dispatch`)
  starts capture; pressing it again stops capture and hands the buffer to the
  transcriber.
- Capture also stops on: `max_seconds` reached (hard cap, default 30), or —
  when `silence_stop` is enabled — a trailing window of RMS below threshold
  (the decision function is pure core logic over per-frame RMS values, so it
  is table-tested; the leaf crate only computes RMS).
- `voice-cancel` (and Esc while recording) discards the buffer without
  transcribing.

Hold-to-talk via kitty key-release reporting is a future enhancement and is
called out as a non-goal.

## The transcriber seam

`kind` config_enum on `[voice]`, every value implemented or reserved
(house seam rules; `Probe` rows in `thegn doctor`'s Providers section):

| kind          | status in this change | what it is                                                                                            |
| ------------- | --------------------- | ----------------------------------------------------------------------------------------------------- |
| `whisper_cli` | implemented (default) | subprocess: `whisper-cli -m <model> -f <wav> ...` (whisper.cpp); vendor CLI only inside its impl file |
| `command`     | implemented           | user-configured argv; contract: WAV path as arg (or stdin), transcript on stdout                      |
| `whisper_rs`  | **reserved**          | native in-process whisper.cpp bindings, later phase behind a `voice-native` cargo feature             |

Shape: an object-safe sync trait (`&self`, blocking — it always runs on the
transcribe worker, never on the loop), `SeamError` classification so a missing
binary reads `NotInstalled` (doctor guidance: PATH tier / managed pull), and a
probe that reports the resolved binary, the resolved model, and the selected
kind. `config validate --strict` rejects `whisper_rs` until it lands
(reserved), and a build without the `voice` feature reports the whole seam as
not compiled with install guidance rather than silently missing.

**whisper-cli resolution reuses the managed-tools three-tier order** verbatim:
`[managed_tools.whisper-cli] path` override → `whisper-cli` on PATH (nixpkgs
`whisper-cpp` / homebrew ship it) → managed download. Note honestly: upstream
whisper.cpp GitHub releases do not reliably ship per-platform binaries outside
Windows, so on Linux/macOS the PATH tier is the realistic path and doctor says
so; the managed GithubRelease asset map is filled only for platforms that have
real assets.

### whisper-rs vs subprocess — the judgment, recorded

Judged against the substrate-free-core + seam rules, the subprocess wins as
the _default_:

- **For subprocess**: zero build weight ever (satisfies "default build carries
  zero whisper weight" trivially); no cmake/C++/submodule in the nix build,
  `check-cross`, or MSRV surface; PATH-tier reuse of distro packages; GPU
  (Metal/Vulkan/CUDA) becomes the binary's problem; vendor confinement to one
  impl file. Cost: the model is loaded per invocation — ~sub-second for
  tiny/base/small, seconds for large-v3 cold — acceptable for batch
  toggle-talk, and mitigable later by a resident `whisper-server` kind.
- **Against whisper-rs as foundation**: the GitHub repo is archived
  (maintained on Codeberg — a dependency-health flag `cargo deny` should
  track); it vendors whisper.cpp as a submodule built by cmake, which fights
  the crane source allowlist and the mingw cross lane; and it would be the
  only C++ build in the workspace. As an _opt-in seam impl_ (`voice-native`
  feature, resident `WhisperContext`, per-platform GPU features) it is a fine
  phase 2 — the seam means adding it is a registration, not a rewrite.

## Model acquisition (managed-tools extension)

- **New managed-tools source: `UrlArtifact`** — a direct-URL, non-executable
  file with a pinned `sha256` and a declared `size_bytes`. Core describes it
  purely (URL template, checksum, size — unit-tested); the host downloads,
  verifies the checksum before moving into place, and writes the version
  marker. Generic on purpose (grammars/fonts later), specced as a
  managed-tools delta.
- **A pinned model table in core** maps model names → HF URL
  (`huggingface.co/ggerganov/whisper.cpp`), size, SHA256:
  `tiny` (75 MiB), `base` (142 MiB), `small` (466 MiB), `medium` (1.5 GiB),
  `large-v3` (2.9 GiB), `large-v3-turbo` (~1.6 GiB), plus `-q5_0` quantized
  variants. Checksums come from upstream's `download-ggml-model.sh` table.
- **Grant-checked**: the download is refused unless a `download_file` grant
  (existing capability-grants kind) covers the URL — the example config ships
  a commented `[[voice.grants]]` with scope
  `https://huggingface.co/ggerganov/whisper.cpp/**`. No new grant kind.
- **Size warning**: any download above a threshold (500 MB) requires explicit
  confirmation (`--yes` on the CLI; a confirm prompt in-UI), and every pull
  prints the size up front. The warn/confirm decision is pure core logic.
- **Resolution**: `[voice] model_path` override → managed store
  `~/.thegn/models/whisper/ggml-<name>.bin` (checksum-markered). No PATH tier
  — models are not commands. `thegn voice model list|pull|rm` manages the
  store; `thegn doctor` reports the resolved model, its tier, and
  pinned-vs-installed state exactly like a managed tool.
- Fetches run on the CLI path or a background worker — never the loop
  (managed-tools "core decides, host fetches" requirement applies unchanged).

## Injection path

The transcript is injected with the existing `run.rs::paste_text_into_pane`:
one bracketed-paste-wrapped write built by `pane_writer::build_paste_bytes`
(embedded paste markers neutralized — the hardening already exists), queued
non-blocking to the pane's writer thread (in-process PTY) **or relay task
(daemon Stream)** — so under `[daemon] enabled` the text lands in the
daemon-owned session that survives detach, with no new plumbing. Rules:

- **Never append a newline; never auto-submit.** The transcript arrives as
  reviewable text; execution is always the user pressing Enter. There is
  deliberately no `submit` config key.
- Target = the focused pane at _injection_ time; if focus moved or the pane
  died mid-transcription, the transcript goes to the status line / a
  notification instead of a surprise pane — never to a pane the user isn't
  looking at.
- Headless/external injection already exists (`Verb::SendInput`,
  `thegn session send`); voice adds **no capability-catalog row**.

## Feature gating and build shape

- New leaf crate `crates/thegn-voice/` on the `thegn-media` model: core-free,
  no SQLite/C-build deps, per-OS concerns confined inside it so the
  platform-cfg ratchets stay clean. Deps: `cpal` (capture — it already
  abstracts CoreAudio/WASAPI/ALSA; hand-rolling per-OS capture in
  `thegn-host/src/platform/` would duplicate it, so the "platform backends"
  live behind cpal inside the leaf; SDL2 was judged and rejected as a C dep),
  `hound` (WAV encode). Config is lowered from core into a plain
  `VoiceOpts`-style struct, so the leaf never depends on core.
- `thegn-host` gains an off-by-default `voice` cargo feature (precedent:
  `profiling`) carrying the optional `thegn-voice` dep and the handlers.
  Without it: no audio deps, actions unregistered, `[voice]` keys accepted by
  config (never launch-blocking) but reported inert by doctor.
- **Channel**: voice registers as `Stability::Experimental` — the stable
  channel's config clamp forces it off with the standard one-line note.
- Pure logic in `thegn-core` (`voice.rs`, `config_voice.rs`): model table,
  resolution + warn decisions, silence-stop decision, transcript post-filter
  (trim, strip trailing newline, drop whisper noise tokens like
  `[BLANK_AUDIO]`) — all under the 95% gate. Capture/subprocess are
  smoke-territory.

## Config (`[voice]`, every key documented in config.toml.example)

| key                | default       | meaning                                                                     |
| ------------------ | ------------- | --------------------------------------------------------------------------- |
| `enabled`          | `false`       | master switch (experimental-clamped)                                        |
| `kind`             | `whisper_cli` | transcriber seam kind (`whisper_cli` \| `command` \| reserved `whisper_rs`) |
| `model`            | `base`        | name from the pinned model table                                            |
| `model_path`       | unset         | override: use this ggml file, skip the managed store                        |
| `language`         | `auto`        | forced language code or autodetect                                          |
| `device`           | unset         | input device name (default: system default)                                 |
| `max_seconds`      | `30`          | hard capture cap                                                            |
| `silence_stop`     | `true`        | auto-stop after `silence_ms` below threshold                                |
| `silence_ms`       | `1200`        | trailing-silence window                                                     |
| `keep_recordings`  | `false`       | persist WAVs under `recordings_dir` instead of deleting                     |
| `recordings_dir`   | state dir     | where opted-in recordings go                                                |
| `command`          | unset         | argv for `kind = "command"`                                                 |
| `[[voice.grants]]` | none          | capability grants gating model downloads                                    |

## Security

- **Microphone access & permission UX.** macOS: mic access is TCC-gated and
  attributed to the _hosting terminal app_ (thegn is a TUI inside Terminal /
  Ghostty / etc.), so the OS prompt names the terminal — doctor and the help
  page must say this explicitly or the denial is undebuggable; the
  `macos-app-launcher` .app wrapper needs `NSMicrophoneUsageDescription` when
  voice is enabled. Windows: WASAPI capture respects the OS microphone
  privacy toggle; a denied device surfaces as a clear probe failure. Linux:
  ALSA/PipeWire generally prompt nothing — the statusbar REC badge is the
  _only_ recording indicator, which is why capture is strictly
  explicit-start, hard-capped, and badge-visible; portal-mediated capture in
  sandboxed installs is out of scope and reported by the probe.
- **On-device by default; audio is ephemeral.** Samples live in RAM; the
  transcriber consumes a temp WAV written 0600 under `$XDG_RUNTIME_DIR`
  (fallback: the state dir) and deleted immediately after transcription —
  deletion is attempted on _every_ exit path including failure
  (best-effort `let _ =` with the sanctioned comment). Persistence only under
  `keep_recordings = true`. The transcript itself is never logged at info
  level (it is user speech; tracing may record lengths/timings, not content).
- **Injection blast radius.** Transcribed text entering a shell is the sharp
  edge: mis-transcription + auto-execute would run arbitrary commands. Locked
  by spec: no trailing newline, no auto-submit, bracketed-paste marker
  neutralization via the existing hardened paste builder, focused-pane-only
  delivery on an explicit user gesture. The transcript is data, never parsed
  into thegn actions.
- **Download integrity.** HTTPS to a pinned host, per-model SHA256 verified
  before install, size declared up front with a confirm gate, and the whole
  fetch refused without a matching `download_file` grant — same
  least-privilege posture as MCP server installs. No secrets are involved
  anywhere in the feature (no tokens, no SecretRef needs).
- **`command` kind is user-consented egress.** An arbitrary transcriber
  command can ship audio anywhere; that is the user's explicit configuration
  (same trust model as `[[agents]]`/`[[tools]]`), and `thegn doctor` labels a
  `command` transcriber as user-provided so the on-device claim is never
  silently false.
- **Sandbox interaction.** Capture and transcription run in the UI/host
  process, not inside pane sandboxes; the model store and temp WAVs are not
  bind-mounted into sandboxed panes. New write surfaces: the model store dir
  and the temp WAV — both user-local, no repo writes.

## Open questions

1. **Review-before-inject overlay?** An optional confirmation popover showing
   the transcript before it touches the pane (default off) — deferred; the
   no-auto-submit rule already keeps execution in the user's hands.
2. **Resident `whisper-server` kind** for large-model latency (localhost HTTP,
   model stays warm) — worth a reserved enum value now?
3. **Default chord** for `voice-toggle` — `Alt+v` suggested; must clear the
   chord-collision test against the live keymap.
4. **Nix packaging**: should the flake's full package enable `--features
voice` once stable, or stay minimal? (Build-weight vs out-of-box UX.)
5. **Quantized default**: ship `base` as default or `base-q5_0` (smaller,
   near-par accuracy)?
