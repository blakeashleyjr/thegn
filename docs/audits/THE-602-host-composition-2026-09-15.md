# THE-602 host composition — final evidence checkpoint

Date: 2026-09-15
Scope: standalone THE-602 source, review, and existing native evidence assembled in a private candidate. No new Cargo/native execution was performed for this checkpoint. Landing remains pending root review.

## Candidate and source identity

The private candidate starts at `bb97ec24e6d679400b4e97615e4e27d2c4a97039` and merges canonical `b025ec5d877f191d65cc831c95035c493c49c7f6` with a clean three-way merge. The resulting merge commit is `8c0bb31f...`; its pre-audit tree is `f8b878a4...`. The merge introduces canonical b025 metadata/evidence alongside the host-composition work. The diff contains no THE603, THE639, THE640, or performance paths.

The THE-602 implementation and compatibility pins are byte-identical to the tested combined05 source `33c0db71a3efd91a5637be47fd2d353fd628056a`:

| Path                                                | SHA-256                                                            |
| --------------------------------------------------- | ------------------------------------------------------------------ |
| `crates/thegn-core/src/host_config_checked.rs`      | `5000805c4f94ce76d7dc7b21f618a6636c9b74988e8729ddd54d32630e2d9996` |
| `crates/thegn-core/src/host_db_snapshot.rs`         | `c8ef1d84a6560506aff462527c6f909490a7440acce3a80a65e1226dd521eab0` |
| `crates/thegn-core/src/host_definition_snapshot.rs` | `d814d0c3c9cac8f76577cca8e819db571daa00955bf887215d5da46fe8574c05` |
| `crates/thegn-core/src/plugin_api.rs`               | `ab18e52bfffa1258e6a6e3694f275019299ca428da26e65f80ce015f0ad7137c` |
| `docs/api/plugin-api-0.3.json`                      | `dd472746494661fa8975c0fae4f80e5f919a4e955555fec66a7eced4d55ce543` |
| `test/env-overlay-ratchet.txt`                      | `52dbd42ca6eddf08635f1834f80932e915083d05fd554f4ce7764598cf826ad3` |

## Evidence and review

- Combined05 full native receipt, source-bound to `33c0db71`, reports 8465/8465 passed, 26 skipped, exit 0. Receipt SHA-256: `c473ac0ace3701da34b0cc845ff1c99f32b6cfc9a62e7c93cbfd1664e85ad310`. Full log SHA-256: `303b96ce47969ced77bf3d54598d98f70812bd861ed7e1262dbf30f68a08f25b`. The complete THE-602 identity set is 22 host-config checks, 17 DB-snapshot checks, 6 definition-snapshot checks, 3 plugin wire checks, env-overlay coverage, and schema snapshot coverage.
- The preserved historical source `16996e29049e2ab9ef1e097f20841478ec035760` reports 4203/4205 passed, 2 failed, 2 skipped. The failures are the stale ApiVersion object snapshot and missing `plugins.api` pin. Corrected metadata source `560a7beefb497f32d8498e7277d23d1a07310899` reports 4/4 passed; its native log SHA-256 is `0f77a9cdc8143da3faff4d531d515a390690236fb6e6e3d5a6712dd82e32934d`.
- Primary and independent source reviews are retained externally at `/tmp/thegn-THE602-final-source-independent-review-20260914.{md,json}` and `/tmp/thegn-THE602-independent-source-review-20260914.{md,json}`. They approve bounded capture/composition mechanics and explicitly keep launch adapter integration, external-store construction, and runtime containment separate.
- Existing source gates are retained by path and hash in the manifest companion. The 8465-test result is reused only because the candidate’s relevant source and pins match its source commit exactly; no new native build is implied.

## Acceptance boundary

The candidate records THE-602’s strict persisted capture and bounded checked composition as complete for this scoped delivery. The result is a prerequisite for parent THE-592 and does not itself grant launch/provider authority. THE-592 adapter wiring, THE-603 opener/absence handling, THE-604 external-store compatibility, and runtime containment remain separate obligations. OpenSpec and delivery status therefore record gates complete with reviewed local landing pending; no issue closure or Linear mutation is asserted.

Root additionally compared all 15 changed Rust/schema/env files against frozen source `33c0db71`; all are byte-identical. The final manifest expands the six-file checkpoint above to that complete changed-source set. Current canonical THE-640 delivery ancestry `475bccb1` was merged without changing any THE-602 Rust source. The ApiVersion string-schema prerequisite and compatibility pins resolve the earlier review’s plugin schema finding; the corrected full native receipt includes the previously failing schema and environment-pin controls. Compact raw receipts/reviews are retained beside the manifest; the full 8465-test log remains an external hash-pinned artifact.

## Reviewed local-main landing

Canonical local main fast-forwarded to `a0f9730892931ce56c7c22aac18c0bfd390b892a` after root and independent review. Earlier candidate landing holds are superseded. Final source ratchets and specification checks passed. The formatter corrected one Markdown table; the subsequent targeted formatting check passed without changes. No new native build or test run was required. User edits and original untracked files remain preserved; no live state writes, provider dispatch, push or restart occurred.
