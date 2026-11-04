# Dictation implementation and verification

## Contracts and UI

- [x] Allocate utterance identity at start; partial/final/cancel/stale reducer contract.
- [x] Reuse paste-without-submission, normalization, held transcripts and focus handling.
- [x] Display revisable previews; cancel active work on pane closure/config replacement.
- [x] Run focused reducer/controller/handler regression tests; record native-host evidence in docs/voice-verification.md.

## Native and models

- [x] Optional substrate-free native leaf; CPAL 16 kHz mono in-memory capture.
- [x] Same-binary worker isolation; one rolling request and full-recording final pass.
- [x] Whisper CPU and optional Metal with CPU fallback; deadlines/cancellation.
- [x] Setup/status/model list/pull/rm, local model path, language/model/size prompts.
- [x] Immutable model revision, pinned size/SHA-256, atomic separate artifact acquisition.
- [x] Resolve pinned dependencies online; run the unmodified locked Linux all-features/native-host matrix.
- [x] Compile native feature; exercise native worker/HTTP/command lifecycle fixtures using ALSA null.
- [x] Record Linux supplied-audio timings/RSS with exact build settings and raw measurements.
- [ ] Complete physical Linux capture/routing, corpus, latency/RSS and cancellation acceptance.
- [ ] Compile Metal feature and verify Apple silicon macOS 14.2+ permissions and packaging.

## HTTP

- [x] Multipart WAV/JSON provider, broker credential references, classified bounded errors.
- [x] Live overlapping requests plus final pass; billing disclosure and final-only setting.
- [x] Run loopback failure, cancellation, malformed/oversize response, refused connection and stalled-body deadline fixtures.

## Release and reconciliation

- [x] Replace historical THE-59 draft; retain THE-148 ownership and delivery ledger mapping.
- [x] Document config/help/doctor, native-off bridges, platform/build/release gates.
- [x] Read connected Linear state; create THE-1510 with children THE-1511–THE-1515, dependencies and undated milestone; reconcile THE-148 and repository ledgers.
- [ ] Measure warm live p95 <= 3 seconds under coding workload; report finalization separately.
- [ ] Exercise corpus, device removal, crash, silence/noise/multilingual/identifiers/long/slow cases on real devices.
- [x] Complete scoped repository checks and source-bound verification evidence; retain the real-platform release gates.
- [ ] Verify portable CPU SIMD/dispatch and release-profile performance; generic Nix builds miss the latency target.
- [ ] Enable desktop native distributions only after real-device gates pass; retain experimental channel.
