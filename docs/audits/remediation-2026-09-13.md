# September 13 audit remediation

Tracking: [THE-614](https://linear.app/blakeashley/issue/THE-614). Baseline:
`f4c1355bf2dce35b78502e1fe2023ab6054e19ac`. This is an implementation/review
record, not a claim that the running build contains these changes.

## Plan review and authorization

The user authorized issue creation, investigation by subagents, primary review
and revision of plans before implementation, code and regression-test review,
independent adversarial review, and local-main landing only after those gates.

Three implementation plans were reviewed and greenlit by the primary agent:

- Session/sandbox/teardown (THE-615–618): authoritative same-source absence,
  bounded exact-session reconnect, final-outcome floor checks, accurate probe
  wording, and an exact-version termwiz destructor patch. Revisions required
  primary API evidence, strict roster decoding, bounded input/cancellation
  ownership, preserved dependency provenance and actual private PTY tests.
- Logging/connectivity/forge (THE-619–624): payload-free diagnostics, coherent
  in-place connectivity updates, public-origin admission before token lookup,
  typed fallback classification, bounded helper ownership, stable suppression
  and bounded diagnostic deduplication. Revisions required action-payload tests,
  descendant cleanup, explicit-filter compatibility and no guessed auth cause.
- Performance/log tail (THE-625–627): explicit measurement boundaries/counts,
  active-loop accounting, separate worker CPU, bounded writer aggregates and
  rotation-aware tailing. Revisions required bounded retired-file draining,
  no new idle wake, no CPU double counting and no unsupported optimization claim.

The primary agent's build/merge plan:

1. Reuse the existing unlanded merge-safety integration only as a review
   candidate. Review the committed-outcome boundary, atomic queue observations,
   canonical history admission, repository ownership, no-force cleanup and
   refusal bookkeeping. Exercise failed gate, CAS, retained-row and foreign-repo
   fixtures; require independent adversarial review before approval.
2. Reuse THE-613's staged upgrade with private backup, explicit operator-controlled
   quiescence and preserved migration policy. Carry forward the current wrapper
   override into both release-profiling and the staged build, and retain log
   suppression. Do not execute a real upgrade as a test.
3. Implement THE-575 using Git-resolved HEAD/ref paths and existing-path watches.
   Run the unchanged production script in tiny real Cargo fixtures, including
   ref-only changes, packing, linked/detached checkouts and archives.
4. Reuse THE-586's bounded delivery-index correction and register all new changes
   reciprocally. Preserve unrelated delivery rows and existing issue ownership.
5. Assemble isolated changes, resolve overlap explicitly, run focused runtime
   regressions plus aggregate source/format checks, then have another agent
   adversarially review the combined code. Review every requested revision.

Existing implementation provenance: `fix/merge-queue-safety-integration`
at `cee879db`, `fix/live-upgrade-safety` at `4ab7786a`, and
`fix/delivery-inventory-severity` at `dfbc017d`. Existing branches were not
modified in the canonical repository. New issues and matching existing issues
are listed in `remediation-2026-09-13.json`.

## Verification in progress

- Upgrade helper: 21 private tests passed. The sandbox maps host-owned `/` and
  `/tmp` to uid 65534; supported-path tests normalize only those fixture
  ancestors, and a separate test verifies production still refuses unmapped
  ancestry. No actual controller shutdown/install/migration was performed.
- Git build metadata: three real Cargo scenario tests passed, covering multiple
  builds/commits and unchanged-build freshness. No application release build.
- Assembled existing merge/build/delivery candidate: delivery validation,
  fixtures and source ratchets passed before new runtime branches were added.
- Actual gate-path module tests: two passed and four failed in this sandbox.
  Positive ownership admission is blocked by uid 65534 ancestry; Unix socket
  creation returns EPERM. These remain failed validation requirements, not
  skipped/passing tests. Production ownership checks were not weakened.
- Runtime tests, combined-code review and adversarial verdict remain pending.

## Execution limits and landing

The canonical `.git` is read-only; creating a branch failed with a read-only
filesystem error. The signing agent is also unavailable in this sandbox.
Review checkpoints in the private clone are unsigned; user/global Git settings
are unchanged. Canonical local-main landing and signed final commits are not
claimed. The original working tree, live processes and application state remain
untouched. Final artifacts and remaining validation will be recorded here.

The audit's healthy database checks, zero new crash count, conservative stale
worktree retention and negotiated keyboard capabilities are observations, not
new defects to fix. No stale worktree cleanup or repository credential change
is authorized by the tests in this remediation.
