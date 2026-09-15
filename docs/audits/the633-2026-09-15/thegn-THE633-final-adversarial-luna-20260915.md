# THE-633 standalone adversarial source/evidence review — 2026-09-15

Candidate: `/tmp/thegn-THE633-impl-luna-20260915`

Candidate HEAD: `fad5d61736d74faa9a1b451600a91fa9ea178f5b`; standalone base:
`59a9c944bcc94107ded859932864ffaf9be1ffb3`. The candidate’s THE-633
implementation ancestry is `774f3d78536387b6f103ad76311259448f12ce07`, with
test-only fixture corrections `ae68f518f0cbe04d01fb7b33a5780235ef33d948` and
`53d9a0c6f5fe8ca402497f7fc38cca52dcb30457`.

The standalone source boundary is clear. It has six production paths:
`bounded_git_probe.rs`, `devcontainer_probe_cache.rs`,
`devcontainer_provider.rs`, `hydrate.rs`, `platform/unix.rs`, and
`platform/windows.rs`; and six test paths: the bounded Git/cache/capability,
identity, execution, and hydration workload fixtures. There is no
`devcontainer_startup.rs`, and no THE-639 startup, monitor, renderer, sampler,
or process-performance implementation import. The hydration workload file
contains only the four-line `probes == 0` assertion in an existing ignored
fixture. The existing THE-617 Unix `sandbox_floor_preflight_tests` registration
is retained; THE-633 additionally registers its identity tests.

I compared all 12 changed Rust paths against the current maintenance06 source
at `9553d36878b8e4726524997d2cd7e05e3b4b1f93`. Eleven match byte-for-byte. The
only mismatch is `devcontainer_provider.rs`: maintenance06 includes the
separate THE-639 `devcontainer_startup` module, `prepare_start` API, startup
ownership path, and migrated startup fixtures. The standalone candidate has
the pre-THE-639 provider API and only the THE-633 demand-cache additions. This
is a required source bridge for reusing combined05/06 evidence; it invalidates
the prior metadata wording that all 17 selected source files were exact. The
candidate actually changes 12 Rust paths, and its source hash manifest also
contains 12 entries while claiming 17. The 17-file universal exact claim must
be corrected to 11-of-12 current06 exact plus a scoped provider bridge.

The current candidate/provider bridge is limited to THE-639 startup extraction;
the THE-633 cache, classifier, bounded capture, identity, hydrate demand, and
test-fixture hunks are unchanged. The candidate’s 19 exact THE-633 selectors
are represented in the supplied full05 excerpt: seven bounded capability
tests, eight cache tests, two selection/classifier tests, and two Unix identity
tests. The copied receipt reports source `33c0db71a3efd91a5637be47fd2d353fd628056a`,
8465/8465 passed, 26 skipped, exit 0. Receipt SHA-256 is
`c473ac0ace3701da34b0cc845ff1c99f32b6cfc9a62e7c93cbfd1664e85ad310`; full-log
SHA-256 is `303b96ce47969ced77bf3d54598d98f70812bd861ed7e1262dbf30f68a08f25b`.
The 19-line excerpt is internally consistent and hashes to
`d4ea948cdd83966b8247aa5edeabb0547adac8b5806c14d32842a2eb5947fa44`.

The focused current06 receipt is separate combined evidence: source
`27c0ccad5399c84612fa35aea7841b0d4209458c`, 121 passed and 8220 filtered, exit
0, log SHA-256
`94e6693c851cc1d62de35c092483b5d6fc26110b91f728826b7b89e9b5cf88c3`. It is
not a THE-633 test count. The corrected maintenance06 gate receipt’s 16 native
passes and strict lint pass are similarly separate THE-603 evidence and do not
establish a new standalone THE-633 run.

The retained hydration evidence says the existing actual `build_model` fixture
made 9 unrelated helper calls at each of 1/8/32 worktrees before the demand
gate and 0 after it. This is a qualified probe-removal observation, with no
release-speed, equivalent-performance, or whole-application timing claim; no
new workload was run.

The candidate’s durable audit and OpenSpec files are narrowly scoped, and its
delivery additions are reciprocal THE-633 records only. Root’s current
metadata revision now records 12 compared files, 11 exact Rust matches, and
the explicit provider bridge in `source-bridge.diff`; the 8465 receipt hash is
verified as `c473ac0ace3701da34b0cc845ff1c99f32b6cfc9a62e7c93cbfd1664e85ad310`.
The standalone source/spec/ratchet gates are recorded as passing, with the
initial formatter failure and its two-Markdown correction retained. The
remaining local-main landing and final delivery readback are root-owned.
No source/build/native/provider/Linear operation was performed in this review.

## Candidate/current06 source hashes

| Path | Candidate SHA-256 | Current06 SHA-256 | Result |
| --- | --- | --- | --- |
| `crates/thegn-host/src/bounded_git_probe.rs` | `164b4db61316ab4e0abf661bb469d113cc4a8a074c187c3655bbc5a67f5a6e1f` | same | exact |
| `crates/thegn-host/src/bounded_git_probe_tests.rs` | `a71abb302c1acb75ab1af0079b3e301508757c7efbd35537f9616fef29dd637c` | same | exact |
| `crates/thegn-host/src/devcontainer_probe_cache.rs` | `4da7e442e736f82b2f7d4f6ffafae138a49ae0d5960594777061e542a5608e57` | same | exact |
| `crates/thegn-host/src/devcontainer_probe_cache_tests.rs` | `af40517c0478524df619a6cb5db3a0303e701b325ac46b2e250e06de9712fb81` | same | exact |
| `crates/thegn-host/src/devcontainer_provider.rs` | `92367923702299cb54fa8d6c0ecc2003801c4ccd54fff575a7aeafe318149033` | `44e2c12a45e69bc686c4e30f22258ef5963dedd640a1bd4501683d91e8b84ea3` | THE-639 bridge |
| `crates/thegn-host/src/hydrate.rs` | `4455278cb82dedfe4a3588c433ddbd91e3b98cdf155fbcb2647e1315a971ad65` | same | exact |
| `crates/thegn-host/src/platform/bounded_capability_tests.rs` | `5289c57d3c975a2f9ebedb0f5f3d85502efe1faf5e4f646519e3a3fa3a66917b` | same | exact |
| `crates/thegn-host/src/platform/capability_identity_tests.rs` | `aa6477cdf951185058242054413be7be7c940fb20e786d2fb9ff2ac9afeb3994` | same | exact |
| `crates/thegn-host/src/platform/devcontainer_probe_execution_tests.rs` | `8269a812f5330cb3086f789fffd3d674d22bb172c75f54af9c8855d979d4fc5f` | same | exact |
| `crates/thegn-host/src/platform/perf_workloads_hydration.rs` | `dea955ed0b9749520b5ae9c59bef4a43f12206bd29e4798d5f5cab8427f1b299` | same | exact test assertion |
| `crates/thegn-host/src/platform/unix.rs` | `c5ae8c7b8fa02c5b7653076e5ade81dc1d0cb7c029800fddb1d0354e98e05e64` | same | exact |
| `crates/thegn-host/src/platform/windows.rs` | `1e6ffd5022e7dd983cca3282553808de3ccd55706cbaff9748252ecd4d5362f9` | same | exact |
