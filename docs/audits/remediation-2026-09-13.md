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
- Combined final core tests: logging 16, config diagnostic cache 4 and merge
  state/outcome/policy regressions 119 passed. Combined host test build passed (3m24s).
- Session candidate: actual-source recovery/relay 8, patched real PTY 4,
  isolation-floor 10 and decoder 2 tests passed. HTTP adapter test failed at
  socket bind with EPERM; native Windows evidence remains pending.
- Forge candidate: 24 tests passed; actual input-module privacy tests 13 passed.
- Performance candidate: 20 actual-source boundary tests and 8 svc log tests
  passed. Full model hydration attribution remains unresolved (THE-627).
- Independent adversarial review found three issues: cross-repository sweep
  selection before TTL, close-under-backpressure deadlock, and unbounded
  Windows taskkill on credential cancellation. Each was sent for revision and
  the revised source was approved by the independent reviewer. Foreign Git
  identities are now filtered before policy, pane owner lifetime independently
  wakes the relay and unsupported credential helpers are refused before spawn.
  Host execution found 179 passed, 97 failed and one pre-existing ignored test
  across 277 selected cases. Failures are concentrated in canonical-history,
  gate, cleanup and socket/signing fixtures; strict ownership admission rejects
  unmapped uid65534 ancestors before the tests reach their intended assertions.
  These are not accepted as green or silently skipped. Remaining native
  validation prevents final landing approval.
- Existing residuals explicitly outside completed scope: direct symbolic fold
  target admission (THE-596), configured merge-gate output bounds (THE-601),
  native Windows gate admission and full hydration root-cause evidence.

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

## Combined host results

Tests ran individually in private XDG directories, four concurrent workers,
90-second per-test deadlines, and exact names against the compiled production
test binary. No application/live terminal was started. Per-test outputs and
machine-readable results are retained with the final handoff artifacts.

| Module            | Passed | Failed | Ignored |
| ----------------- | -----: | -----: | ------: |
| agent             |      4 |      0 |       0 |
| canonical_history |      8 |      8 |       0 |
| compositor        |     15 |      0 |       1 |
| daemon            |      5 |      1 |       0 |
| frame_writer      |      7 |      0 |       0 |
| input             |     13 |      0 |       0 |
| integrate         |     21 |     64 |       0 |
| merge_lifecycle   |     22 |     12 |       0 |
| merge_sweep       |      4 |      8 |       0 |
| pane              |     29 |      0 |       0 |
| pane_recovery     |      5 |      0 |       0 |
| perf              |     15 |      0 |       0 |
| perf_timing       |      5 |      0 |       0 |
| platform          |      6 |      4 |       0 |
| render_plan       |     20 |      0 |       0 |

Controlled synthetic workload passed in the unoptimized test profile:
config-load medians 3.44–3.77ms; effective-environment resolution 0.215ms
for one row to 1.482ms for 32 rows. One-row surface diffs measured
46/101/149µs at 80×24/160×48/240×72 versus full resync 310/1204/2657µs,
with equivalent reconstructed cells. These are scoped candidate measurements,
not before/after speedups or full hydration/terminal latency evidence.

Final assembled svc test binary: 39 selected tests passed; two control-client endpoint tests failed on forbidden socket creation (EPERM). This includes the final forge, log-tail and strict-roster parser sources. Source ratchets, ten delivery fixtures and all three runtime OpenSpec strict validations passed.
