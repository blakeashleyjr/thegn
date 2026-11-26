---
id: voice
title: Voice mode
order: 13
actions: [voice-toggle, voice-cancel]
---

# Voice mode

## Quick start

1. Voice is a stable, opt-in feature: nothing records or loads a model until
   you turn it on. The one step is `thegn voice setup --language en`, which
   writes `[voice] enabled = true` for you (or set it by hand).
2. Use a build with native dictation. The Nix package (`.#default`, Home
   Manager) on Linux and the macOS Apple silicon release (Homebrew or tarball;
   macOS 14.2+, Metal with CPU fallback) include it. The Linux release tarball
   (glibc 2.28 build) and the musl build do not, because they cannot link ALSA;
   `thegn voice status` then says native voice is not built into this binary.
   With those, use an HTTP provider or `capture_command`, or install via Nix.
3. Setup offers to download the Whisper model (about 141 MiB for `base.en`),
   with progress, in that shell (never inside the UI), and verifies its
   checksum. The model is not bundled. Pressing `Alt v` before setup tells you
   to run it. The command palette lists `Voice: toggle recording`.
4. Run `thegn voice status`. The `Ready:` line names the next missing step;
   `thegn doctor` shows the same line. Readiness never opens the microphone.
5. Press `Alt v` in a pane. On macOS, allow Microphone for your terminal app
   (System Settings > Privacy & Security > Microphone) when prompted, then
   restart the terminal if you had denied it. On Linux, make sure the default
   PipeWire/PulseAudio/ALSA source is your microphone.

With no microphone or a missing model, recording stops with an error in the
status line and nothing is pasted. A denied macOS microphone permission usually
records silence instead, so the status line shows `Voice: no speech detected`;
check the Microphone privacy setting for your terminal app. Disabled voice starts no
worker and opens no audio device.

Voice mode is an opt-in toggle-to-talk feature. Press `Alt v` to
start recording from the focused pane, press it again to stop and transcribe,
or press `Esc`/`Alt V` to cancel and discard the utterance. If focus moved or
the pane died before transcription finished, the transcript is held in memory
and the status shows a preview. `Alt v` inserts it into the focused pane;
`Alt V` discards it. A newer transcript replaces it; it is never saved and a
config change clears it. Text is pasted without a newline and is never
auto-submitted.
Line breaks and control characters in a transcript are inserted as spaces.
Whisper annotations such as `[BLANK_AUDIO]`, `[Music]` or `(laughter)` are removed,
along with one trailing subtitle credit such as `Thanks for watching!`. A bare
`Thank you.` is kept.

`[voice] replacements` is an ordered list of up to 200 find/replace rules applied
to every final transcript, whether it came from the command, HTTP, or embedded
provider. Each rule is a literal phrase, matched case-insensitively and only on
word boundaries, so `open code` does not match inside `reopen code`:

```toml
[voice]
replacements = [{ from = "open code", to = "OpenCode" }, { from = "um", to = "" }]
```

When phrases overlap, the longest match wins, then the earlier match, then the
earlier rule. An empty `to` deletes the phrase and one adjacent space. Replacement
does not change capitalisation or punctuation beyond that. Partial previews are
not rewritten; only the final text is.

Run `thegn voice setup --language en` to configure a provider and model. Use
`--kind embedded` (the default), `--kind http`, `--kind command`, or `--kind hex`; setup saves
voice as enabled for that kind. For a catalogue model in the store, embedded
setup verifies the installed model's size and SHA-256, or offers to download it.
Setup shows download size
and asks before downloading a missing Whisper model. Use
`--language auto` for detection, `--model tiny.en` for speed, or `--model small.en`
for accuracy. Non-English languages use multilingual `tiny`, `base`, or `small`.
Use `--model-path /path/to/model.bin` for a local GGML model. Downloads finish
before capture; recordings remain in memory. On first use, Alt v reports setup
or build requirements if the provider is unavailable.

Native dictation is the default `voice-native` Cargo feature on Linux glibc
(CPU Whisper, ALSA via cpal; needs `libasound.so.2` at runtime, which the Nix
package carries) and macOS (CoreAudio, Metal with CPU fallback, 14.2+). It runs
in a disposable worker process, so a machine with no audio device or no model
still starts normally: voice reports unavailable and nothing else changes. On
macOS the microphone permission belongs to the hosting terminal app; allow it
in System Settings > Privacy & Security > Microphone. On Linux check the default
input and ALSA/PipeWire routing. Disabled voice creates no worker or audio
device.
musl-static and Windows builds, and the glibc 2.28 Linux release tarball, are
built with `--no-default-features` and omit native voice. There, default
embedded setup stops with a "not built into this binary" message; HTTP setup
requires `[voice] capture_command`, while command setup requires `[voice]
command` and also a capture command. Source builds on Linux glibc and macOS
include it by default; `--no-default-features` opts out. Hex setup requires
`[voice] capture_command` in builds without native voice.

Native embedded and HTTP providers show revisable text in the status line while
recording. Silence and low-level steady noise are skipped before transcription;
the final embedded pass keeps 200 ms of context around detected speech. Previews
use their full rolling windows so their offsets remain aligned. Only the final
full-recording transcript reaches a pane. Stopping closes the device before
finalization. Cancellation, config replacement, or pane closure during capture
discards the current preview. A completed final transcript that loses focus is
held for explicit insertion as described above.

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
Values outside `1..=300` for `timeout_seconds` are clamped to that range.

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

## Use Hex for faster local models

Select Hex with `thegn voice setup --kind hex` or set `[voice] kind = "hex"`.
The built-in Whisper provider remains the default. thegn captures audio using
native capture or `[voice] capture_command`, writes the WAV to a private,
short-lived temporary file, and runs `hex transcribe <file>`; Hex prints the
transcript to stdout. Hex must be installed and available on `PATH`. Install it
with Homebrew (`brew install --cask anomalyco/tap/hex`), AUR (`yay -S hex-bin`),
Nix (`nix profile add github:anomalyco/hex`), or the [Linux install
script](https://anoma.ly/hex/install-linux.sh). Hex may download its selected
model on first use. Hex transcriptions are final-only and use the configured
voice deadline.

`thegn voice status` (or `thegn voice status --json`) and `thegn doctor`
report build and configuration readiness without opening the microphone.
`thegn voice model list [--json]`,
`pull NAME [--yes]`, and `rm NAME` manage pinned artifacts under
`$XDG_DATA_HOME/thegn/voice/models` (default `~/.local/share/thegn/voice/models`).
Pull verifies exact size and SHA-256 and installs atomically; Ctrl-C cancels,
and rerunning retries. An interrupted or corrupt download never replaces an
installed model. Recordings are not retained in the database or on disk; Hex
uses a private temporary WAV that is removed after transcription.
Models come from a pinned whisper.cpp revision on Hugging Face; only model pull
and embedded setup access that source. The catalogue is `tiny`, `tiny.en`,
`base`, `base.en`, `small` and `small.en`, about 74, 141 and 465 MiB by size;
the `.en` models are English-only. `voice model list` shows each model's size
and whether it is installed. `voice model rm NAME` removes its model,
verification marker and abandoned partial downloads, and succeeds if absent.

`vocabulary` lists names Whisper should spell (repos, tools, people), for
example `vocabulary = ["T3 Code", "Ottobit"]`. The first terms that fit in 500
characters are sent as the Whisper prompt on the final pass (embedded) or as the
HTTP `prompt` field; live previews never use it, and `command` providers ignore
it. If the whole transcript is only the vocabulary echoed back (two or more
terms), it is discarded.

All `[voice]` settings are in `config/config.toml.example`: `enabled`, `kind`,
`capture_command`, `command`, `max_seconds`, `language`, `model`, `model_path`,
`endpoint`, `credential`, `live_preview`, `timeout_seconds`, `replacements`, and
`vocabulary`. Voice is available on the stable channel and stays off until you enable it.
