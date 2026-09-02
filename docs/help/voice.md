---
id: voice
title: Voice mode
order: 13
actions: [voice-toggle, voice-cancel]
---

# Voice mode

Voice mode is an experimental, opt-in toggle-to-talk feature. Press `Alt v` to
start recording from the focused pane, press it again to stop and transcribe,
or press `Esc`/`Alt V` to cancel and discard the utterance. The transcript is
inserted into the same pane only while it is still focused. It is pasted
without a newline and is never auto-submitted.

thegn ships no microphone integration, audio library, speech-to-text engine,
or model. Configure two argv arrays under `[voice]`:

- `enabled` — explicit consent; defaults to `false`.
- `kind` — the provider kind, currently only `"command"`.
- `capture_command` — a program and arguments that write one complete
  16-bit PCM WAV to stdout. It is stopped when recording ends.
- `command` — a program and arguments that read one WAV from stdin and write
  UTF-8 transcript text to stdout.
- `max_seconds` — the hard recording limit, clamped to a safe finite range.

Commands are argv arrays, not shell strings. thegn itself does not send audio
to a cloud service, but an arbitrary command you configure can send the audio
elsewhere; inspect commands before enabling voice. Audio is held in memory only
and is not written to the state database or a recording file.

Run `thegn doctor` to see whether both commands are configured and available.
The doctor output names missing executables and the configuration repair. Voice
does nothing while disabled, unconfigured, or on the stable channel when the
experimental feature is clamped off.
