# Rolling bug work — completed local landing

Canonical local main is `7014a49696a8baa3d2ef36e4186e6bce853a9e4d` after the
reviewed source candidate `0e726cb7` and signed delivery reconciliation. The
signature verifies. Thirteen scoped Linear issues are Done with fresh readback:
THE-377, THE-483, THE-591, THE-593, THE-594, THE-595, THE-597, THE-600,
THE-606, THE-634, THE-635, THE-636 and THE-637. See
`rolling-linear-status-20260914.json` and `rolling-bug-fixes-2026-09-14.md`.

All 8,354 selected workspace tests passed, with 26 configured skips; the exact
selected names match the prior failed run. After final test-only lint cleanup,
all 139 affected tests passed. Strict host/core/service all-target Clippy,
source/delivery ratchets, formatting, all 157 strict OpenSpec items, and configured
contract/live/build-metadata stages passed. Counts overlap and are not summed.
Independent source and exact-receipt reviews are retained. Raw failed runs remain
in the archive for provenance; their pending conclusions are superseded by this
final receipt, not retroactively relabeled as passes.

THE-545 remains open for actual credential/account-generation binding and
THE-154 for native Windows/macOS and complete escaped-tree containment. THE-630
has a reviewed investigation/plan but no landed sampler implementation;
THE-631/632/633 remain queued performance follow-ups. No live restart or push
occurred. Saved justfile/audit byte hashes were verified after merge and signing.

`rolling-local-main-20260914.bundle` contains this round through signed main and
requires the existing `1ef8228f` baseline. Git verified it successfully.
`rolling-final-validation-20260914.tar.gz` retains full logs and exact test-name
receipts; `rolling-private-cli-106-commands-20260914.tar.gz` retains the owned CLI
fixture proof (13 Thegn invocations among 106 setup/probe/test commands).
`manifest.json` inventories the current artifacts; the previous landing manifest
is retained separately. Private candidate commits were unsigned as described in
the historical record; the canonical delivery receipt is signed.

---

## Historical prior landing receipt

# Audit review handoff — landed September 14

The reviewed candidate `ea807922` is now merged into local main. The signed
landing receipt is commit `407e7d77`. All 99 formerly blocked tests now pass,
and independent review approved scoped landing. No live restart occurred; the
user's wrapper override and both justfile comments are preserved. See
`local-main-landing-2026-09-14.md` and `native-unblocked-validation.tar.gz`.

## Preserved candidates

- `batch-01-review.bundle`: private ref `audit/batch01-review` at
  `110e8714718b4b97f504162ece3a70eaa0faa92b`.
- `batch-02-review.bundle`: private ref `audit/batch02-review` at
  `ea807922f760474e4e35b92cb361ea3bb2186762`, including batch-01 history and the selected ten
  maintenance candidates tracked by THE-629.

Both bundles require the canonical baseline and are verified with Git. Private
review commits were unsigned because the signing agent was unavailable during
the restricted session. The September 14 landing-receipt commits are signed. They are review
artifacts, not approval to bypass outstanding gates.

## Review and validation

All selected batch-02 scoped implementations received primary plan/code review,
requested revisions, and independent adversarial source review. Final assembled
regressions passed 249 host, 120 core, 110 service (including 48 plugin), and four
fake-LSP integration tests. The affected six-crate clippy -D warnings gate passed.
`maintenance-batch-02-review.md` records the final post-lint rerun, source/spec/
format gates, exact checkpoints and overlapping test counts. Raw final evidence
and independent harnesses are in `batch-02-final-validation.tar.gz`.

**Scoped canonical landing is complete.** All 97 previously failed host tests
and two service tests passed under unrestricted execution, with independent
exact-test-set verification. The 48 plugin tests also passed again. Earlier
restricted-environment reports remain as historical evidence, superseded by the
September 14 receipt. THE-154 native Windows/non-Linux runtime evidence and full
escaped-descendant containment remain open; scoped landing does not claim those
guarantees. No push, live provider action or database migration occurred.

## BTOP and performance

`btop-parity-and-performance-2026-09-13.md` compares features, UI, collection and
render behavior; `btop-140x42.png` is an actual isolated BTOP layout capture.
THE-628 fixes process snapshot/selection instability. THE-630/631/632 track
hidden-idle sampling, relevant-view rebuilding and stable columns. THE-633 tracks
repeated devcontainer probing found by the private 1/8/32-worktree hydration
workload. These follow-ups are queued; no new dashboard/collector features were
added. The workload report explicitly distinguishes unoptimized fixture timings
from release/live performance and makes no BTOP speed comparison claim.

`manifest.json` records each artifact's byte count and SHA256 digest. Earlier
checkpoint archives remain for provenance; their counts are not additive.

Final acceptance snapshot (main `1ef8228f`): seven selected issues are Done;
THE-483, THE-545 and THE-154 remain In Progress for the exact obligations in
`local-main-landing-2026-09-14.md`. The full batch is not claimed complete.
