# THE633 hydration probe-count evidence

This compact record carries forward the reviewed actual `build_model` fixture
measurement. The helper-present baseline executed nine unrelated capability
probes across each nine-build run; the demand-gated candidate executed zero.
The same qualified behavior was recorded at 1, 8 and 32 private worktrees:

| Worktrees | Baseline helper-present calls / 9 builds | Candidate helper-present calls / 9 builds |
|---:|---:|---:|
| 1 | 9 | 0 |
| 8 | 9 | 0 |
| 32 | 9 | 0 |

The candidate also asserts `probes == 0` in the existing ignored
`platform::unix::perf_workloads_hydration::controlled_full_hydration_workload`
fixture. This is owned Git/DB fixture behavior and proves removal of unrelated
probe calls. It is not a new performance run and does not establish a release
speed improvement, equivalent whole-application performance, or a causal
timing guarantee. The original cold paired timing flags and qualified repeat
disposition remain in the independent report below.

Provenance:

- Full native receipt: `33c0db71a3efd91a5637be47fd2d353fd628056a`, 8465/8465
  passed, 26 skipped; receipt SHA-256
  `c473ac0ace3701da34b0cc845ff1c99f32b6cfc9a62e7c93cbfd1664e85ad310`.
- Independent hydration measurement review:
  `/tmp/thegn-batch03-hydration-performance-review-20260914.md`, SHA-256
  `359ef8afe85a71e2beecc673a7cfe39f3c9c46f6b9a910e45ccc3c39bddc8780`.
- The retained full workload audit is
  `docs/audits/full-hydration-workload-2026-09-13.md` from tested05, SHA-256
  `10446d12571beb3437d6b419374382af2f73d9b1a7258dbbefb9404416fe28ab`.

The current candidate has not run this workload or a new performance suite.
