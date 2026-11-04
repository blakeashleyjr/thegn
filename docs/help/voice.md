---
id: voice
title: Voice mode
order: 13
actions: [voice-toggle, voice-cancel]
---

# Voice mode

Voice mode is an experimental, opt-in toggle-to-talk feature. Press `Alt v` to
start recording from the focused pane, press it again to stop and transcribe,
or press `Esc`/`Alt V` to cancel and discard the utterance. If focus moved or
the pane died before transcription finished, the transcript is held in memory
and the status shows a preview. `Alt v` inserts it into the focused pane;
`Alt V` discards it. A newer transcript replaces it; it is never saved and a
config change clears it. Text is pasted without a newline and is never
auto-submitted.
Line breaks and control characters in a transcript are inserted as spaces.

Run `thegn voice setup --language en` to configure a provider and model. Use
`--kind embedded` (the default), `--kind http`, or `--kind command`; setup saves
voice as enabled for that kind. Embedded setup also verifies the installed
model's size and SHA-256, or offers to download it. Setup shows download size
and asks before downloading a missing Whisper model. Use
`--language auto` for detection, `--model tiny.en` for speed, or `--model small.en`
for accuracy. Non-English languages use multilingual `tiny`, `base`, or `small`.
Use `--model-path /path/to/model.bin` for a local GGML model. Downloads finish
before capture; recordings remain in memory. On first use, Alt v reports setup
or build requirements if the provider is unavailable.

Native dictation requires a build with `voice-native` (Linux CPU) or `voice-metal`
(Apple silicon macOS 14.2+, Metal with CPU fallback). It runs in a disposable
worker process. Check microphone permission for the hosting terminal on macOS;
check the default input and ALSA/PipeWire routing on Linux. Native distribution
enablement remains gated on real-device verification. Disabled voice creates no
worker or audio device.
Release tarball and flake builds omit `voice-native`. In those builds, default
embedded setup stops with a native-voice build requirement; HTTP setup requires
`[voice] capture_command`, while command setup requires `[voice] command` and
also a capture command. Native dictation is available in source builds with
`voice-native` (or `voice-metal` on Apple silicon).

Native embedded and HTTP providers show revisable text in the status line while
recording. Only the final full-recording transcript reaches a pane. Stopping
closes the device before finalization. Cancellation, config replacement, or pane
closure during capture discards the current preview. A completed final transcript that loses
focus is held for explicit insertion as described above.

For an HTTP service compatible with multipart WAV transcription:

```sh
thegn voice setup --kind http --language en --model Systran/faster-whisper-small \
  --endpoint http://127.0.0.1:8000/v1/audio/transcriptions \
  --credential env:STT_TOKEN
```

Credentials use the existing secret broker (`env:`, `file:`, `keyring:`). HTTP
previews send overlapping audio repeatedly and may incur repeated billing; pass
`--final-only` during setup or set `live_preview = false`. Authentication,
rate-limit, timeout, malformed-response, and connection failures are displayed;
thegn never switches providers automatically. Provider-specific WebSockets and
whisper-server native protocols require a command adapter. The HTTP provider
sends multipart `file` (WAV), `model` and `response_format=json` fields, plus
`language` except for `auto`. It requires a JSON reply with a string `text`
field, capped at 1 MiB; redirects are not followed. Endpoints must use HTTP(S)
without user/password or a fragment. Loopback requests ignore proxy environment
variables; status, timeout and connection failures are reported separately.
The configured `timeout_seconds` must be between 1 and 300.

When a credential is configured, HTTP endpoints must use `https://` unless the
host is loopback (`127.0.0.0/8`, `::1`, or `localhost`). Setup and runtime both
refuse credentialed plain HTTP to other hosts and tell you to use HTTPS.
Configuration validation reports the same error for `kind = "http"`.

The existing command configuration remains supported:

```toml
[voice]
enabled = true
kind = "command"
capture_command = ["my-recorder", "--wav-to-stdout"]
command = ["my-transcriber", "--stdin-wav"]
max_seconds = 30
```

Commands run directly as argv arrays. Capture writes one complete PCM WAV;
transcription reads WAV on stdin and writes UTF-8 text on stdout. Leaving
`capture_command` empty selects native capture when compiled. Command adapters
produce a final transcript only. External capture may also feed the HTTP
provider. Inspect configured commands: they may send audio elsewhere.

`thegn voice status` (or `thegn voice status --json`) and `thegn doctor`
report build and configuration readiness without opening the microphone.
`thegn voice model list [--json]`,
`pull NAME [--yes]`, and `rm NAME` manage pinned artifacts under
`$XDG_DATA_HOME/thegn/voice/models` (default `~/.local/share/thegn/voice/models`).
Pull verifies exact size and SHA-256 and installs atomically; Ctrl-C cancels,
and rerunning retries. An interrupted or corrupt download never replaces an
installed model. No recordings are stored in the database or on disk.
Models come from a pinned whisper.cpp revision on Hugging Face; only model pull
and embedded setup access that source. The catalogue is `tiny`, `tiny.en`,
`base`, `base.en`, `small` and `small.en`, about 74, 141 and 465 MiB by size;
the `.en` models are English-only. `voice model list` shows each model's size
and whether it is installed. `voice model rm NAME` removes its model,
verification marker and abandoned partial downloads, and succeeds if absent.

All `[voice]` settings are in `config/config.toml.example`: `enabled`, `kind`,
`capture_command`, `command`, `max_seconds`, `language`, `model`, `model_path`,
`endpoint`, `credential`, `live_preview`, and `timeout_seconds`. Voice remains
experimental and disabled on the stable channel; use `THEGN_CHANNEL=dev`.
