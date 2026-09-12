# Tasks and dependencies

- [x] THE-595: read-only discovery and selected-only local snapshot admission.
- [x] THE-595: implement nonzero missing-confirmation refusal and private native dry-run harness (native execution pending).
- [x] THE-595: queued/unqueued dirty UI tests and private CLI harness cases (native execution pending).
- [x] THE-595: fail-closed status, branch/HEAD/common-dir and registry tests.
- [ ] Native fixed-binary proof of failed gate, atomic final status, no sweep and retained successful land.
- [x] Independent source review and scoped gates before component commit.
- [ ] THE-588/THE-589/THE-591 combined integration and normal gate before THE-586 retry.

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
