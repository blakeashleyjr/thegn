# Tasks and dependencies

- [x] THE-595: read-only discovery and selected-only local snapshot admission.
- [x] THE-595: implement nonzero missing-confirmation refusal and private native dry-run harness.
- [x] THE-595: queued/unqueued dirty UI tests and private CLI harness cases.
- [x] THE-595: fail-closed status, branch/HEAD/common-dir and registry tests.
- [x] Native fixed-binary proof of failed gate, atomic final status, no sweep and retained successful land.
- [x] Independent source review and scoped gates before component commit.
- [x] Complete selected-only snapshot/atomic persistence integration and the combined private native gate.
- [ ] THE-588 destructive cleanup acceptance and actual THE-586 native queue retry remain separate.

## Scoped evidence

The frozen component passed 14 core merge-outcome tests and 65 host tests,
including all 12 candidate/snapshot regressions. Nextest runs:
`f91df1e6-a8e1-4e48-a39d-930b6b3b847f` (core) and
`b201061f-157c-423d-b656-8cf266cff547` (host). The first host command incorrectly
requested a nonexistent library target and exited before compilation; the
corrected command passed without source changes. Builds used two CPU cores,
two Cargo jobs and two test threads with private XDG/TMP state.

The native harness has passed syntax/source review only. It must run against the
reviewed combined fixed binary before any native queue retry; no native result,
full gate, main advancement or cleanup approval is implied by these scoped tests.

## Native acceptance — 2026-09-14

The reviewed normal CLI built from local main `1ef8228f` passed the private
106-command regression in `test/integrate-outcome-native.py`. Evidence is retained
at `/tmp/thegn-integrate-native-tw5m0z40/evidence`: the result, complete command
journal, effective private configuration and pinned binary SHA-256 are recorded.
The strengthened dirty preview and confirmation-refusal cases compare target and
both source HEADs, indexes, content, all repository refs and logical DB contents.
Actual selected-only snapshots, failed gates without speculative landed state,
and successful retained folds also passed. Independent review accepted this
THE-595 proof. The earlier syntax-only limitation above is superseded; the full
combined gate is now complete; other issues' acceptance remains separate.
