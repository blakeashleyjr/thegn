# THE-617 standalone final adversarial review

Verdict: **approved for root landing review; no blocker found**.

The candidate is `/tmp/thegn-THE617-impl-luna-20260915` at
`3692d065fb1bd7283275838e50f7507b08e89214`, based on
`088f8d5d7a29671121465a9bccdeeb1ceb15d8ad`. Its five-file delta contains the
Linux fixture, module registration, scoped audit, sandbox scenario and task
record. There are no production implementation changes and no THE-633,
THE-639, THE-603 or performance source changes.

The fixture is byte-identical to the tested05 fixture from
`11c81a35555a59e10cfdc3f88ec601e85de4eafd`:
`6d341918f5677c598058a2e020aaef3acdf8a9b6759b6925005fd42af1ddaf7e`. The
candidate's Linux registration is also byte-identical to tested05
(`f997c840d596faa37bb0c4977b1b0966582a8360b24cdbf62b85927386b67d00`). The
three core dependencies (`sandbox_floor.rs`, `sandbox.rs`, and
`sandbox_backend.rs`) match the tested source byte-for-byte. Full
`crates/thegn-host/src/agent.rs` equivalence is not established: the candidate
hash is `a0737888aea0560fe4d3db026f1a03efcae109f91f89c9deb6b14049e7e70dd6`,
while the tested05 source identified by receipt source `33c0db71` is
`0789aee60728050f74cc580db1b4bbaaa21a77c8f4c11db03bdb791ba9192909`.
The reviewed bridge diff scopes that mismatch to the later THE-639
devcontainer-startup branch in `prepare_sandbox_env`; this THE-617 fixture
sets `DevcontainerMode::Off` and passes no devcontainer input, so that branch
is unreachable for the exercised path. The source comparison therefore
supports the fixture's floor/preparation seam only, not a universal full-file
claim.

The fixture exercises actual `agent::prepare_sandbox_env` with a private fake
Podman executable. Its four exact argv cases prove stronger-candidate
resolution, image/inspect/ensure, deliberate exec-preflight failure, final
host-floor fail/degrade/off behavior, and explicit-runtime refusal. Paths,
environment, command journal, probe cache and cleanup are fixture-owned; no
provider, OCI process, mount or interactive pane is started. The audit's
claims stay bounded to preparation control flow and do not claim real OCI
isolation or native macOS/Windows behavior.

The reused evidence is internally consistent. The maintenance05 log SHA is
`303b96ce47969ced77bf3d54598d98f70812bd861ed7e1262dbf30f68a08f25b`; the
receipt SHA is `c473ac0ace3701da34b0cc845ff1c99f32b6cfc9a62e7c93cbfd1664e85ad310c`.
The receipt reports source `33c0db71a3efd91a5637be47fd2d353fd628056a`,
8,465/8,465 passed and 26 skips. The raw log contains the exact selector
`thegn-host::bin/thegn platform::unix::sandbox_floor_preflight_tests::stronger_runtime_preflight_failure_rechecks_final_host_floor`
as pass 6703/8465.

Delivery ownership is already correctly reciprocal: THE-617 owns the active
`repair-session-recovery-and-terminal-teardown` change under Runtime Security &
Host Architecture. No duplicate change or issue record is needed. The task
record keeps standalone compile/native/scoped-gate and landing work pending;
the dated audit accurately distinguishes reused native evidence from this
source extraction.

No build, test, native process, provider, Linear write or canonical edit was
performed during this review. Root owns final landing and any decision to run a
current-candidate gate.
