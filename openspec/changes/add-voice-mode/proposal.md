# First-party dictation with embedded Whisper and HTTP providers

Linear: THE-1510 with children THE-1511–THE-1515; THE-148 owns reconciliation in Workspace Product & Observability.
THE-59 is historical. This proposal replaces the stale native/whisper-cli draft;
it does not call historical plans delivered functionality.

Extend the existing command adapter with optional native capture and embedded
Whisper in a same-binary worker process, utterance-scoped revisable previews,
pinned downloadable models, explicit setup/model management, and HTTP multipart
transcription using broker credentials. Alt v starts/stops; final normalized
text uses the existing pane paste path and never submits. Cancel discards text.

Linux CPU and Apple silicon macOS 14.2+ Metal with CPU fallback are the initial
native targets. CUDA, Vulkan, Core ML, provider WebSockets, native whisper-server
protocol, always-on capture, and voice commands are excluded. Retain the
experimental channel gate and native-off remote bridges. Enable native desktop
features only after real-platform evidence in docs/voice-verification.md passes.

The native leaf holds CPAL and whisper-rs; core stays free of audio/inference
substrates. Model acquisition is separate from executable downloads (base exceeds
their 128 MiB cap). Recordings stay in RAM; only model artifacts persist.
