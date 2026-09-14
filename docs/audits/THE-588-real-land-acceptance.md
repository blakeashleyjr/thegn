# THE-588 actual-land cleanup acceptance

Existing local-main repairs enforce repository and worktree identity, target
ancestry, conservative status/resource checks, exclusive destroy admission,
no-force removal, verified removal reporting and retained refusal bookkeeping.
The independent acceptance review found one missing positive test: older cleanup
fixtures seeded a landed row without performing a new successful target advance.

Commit `b21cb20d` adds a Unix fixture that creates two private repositories and
linked worktrees under one isolated environment guard. The selected candidate
contains a distinct, unmerged commit. Real `run_fold` advances main through the
production CAS while leaving the selected checkout and queued row unchanged.
Only the subsequent production `persist` call commits the landed outcome and
runs configured automatic removal. The test verifies physical removal and Git
unregistration, the retained source branch and explicit THE-596 queue hold,
preserved attempts/timestamp, and unchanged foreign row, refs, registrations and
file bytes. It uses no live repository, runtime teardown or remote provider.

Primary and independent adversarial source review approved the fixture and its
test-only Git-environment isolation. The native run passed 38/38 selected host
tests in 11.109 seconds, including the new real-land case and failed-fold
negative coverage. Earlier full-workspace receipts establish the unchanged
32-test scoped cleanup/sweep acceptance set; they retain their original source
provenance. Evidence is under `THE-588-real-land-artifacts/`.

Strict host/all-target Clippy, source ratchets and strict change validation now
pass. Canonical local main advanced to
`bb8ff6d77ba85ec292494957653b6c9d9047b8ed` with the reviewed fixture and all
prior strict-origin fixes. The user justfile and 143 pre-existing untracked files
were rehashed unchanged after landing. This does not establish native non-Unix cleanup behavior, stronger
post-observation atomic file protection (THE-370), or restored automatic branch
deletion (THE-596). Those broader obligations remain separate and open.
