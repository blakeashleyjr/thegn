# Dictation design

Core allocates identity at recording start. Partial/final events carry pane and
utterance identity; stale events do nothing. Previews affect chrome only. Final
text replaces preview, normalizes control/whitespace, and follows the existing
paste/held-transcript path. Pane closure during capture cancels; focus changes or pane closure during
finalization hold the final transcript. Configuration replacement and exit cancel native work.

Native capture/inference runs through the same binary's private --voice-worker
entry, before ordinary config/UI startup. A bounded JSON pipe carries config and
broker-resolved HTTP credentials. The parent monitors cancellation/deadlines,
terminates the grouped worker, and reaps it. The worker owns CPAL in a device
thread; stop closes it independently of slow inference. CPU on Linux; optional
Metal with load-time CPU fallback on macOS. The leaf has no core dependency.

Audio is downmixed/resampled to 16 kHz mono, bounded by max_seconds (1..300), in
RAM. Decode the latest overlapping eight-second window with a one-second step.
At most one inference/HTTP request runs; skip obsolete pending windows. Replace
revisions to a window; reconcile text overlap on advancement. Stop aborts a
preview, closes capture, and transcribes the complete recording authoritatively.
Command adapters remain final-only; native capture can supply their WAV input.

Managed tiny/base/small variants use an immutable Hugging Face revision, exact
size and SHA-256, bounded streaming, same-directory temporary files, sync and
atomic install. An explicit local model path bypasses the catalog. Setup asks
language/model and confirms download size; capture never initiates acquisition.
CLI pull handles Ctrl-C and retry. No model extraction or executable downloader.

HTTP sends multipart file/model/language/response_format and requires JSON text.
No redirects or silent fallback; credentials are resolved by the host broker and
sent over stdin, never argv. Request/response sizes and deadlines are bounded;
dropping the future cancels network work. Live HTTP repeats audio; setup warns
about billing and exposes final-only. Command capture can feed final-only HTTP.

No capability catalog operation is added: voice only uses existing pane input.
Status/doctor inspect configuration/build/model presence without opening a mic.
Release flags remain unchanged pending the platform measurements and packaging
matrix documented in docs/voice-verification.md.
