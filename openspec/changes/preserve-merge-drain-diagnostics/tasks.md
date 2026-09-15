# Tasks and dependencies

- [x] THE-608: additive typed exact nullable status replacement with private DB regressions.
- [x] THE-608: route driver paths, diagnostics and result OIDs into their correct fields.
- [x] THE-608: exact panel progress fields and truthful gate-error summary.
- [x] THE-610: terminate isolation-held retry without agent execution or budget consumption.
- [x] Independent source review and normal source ratchets.
- [x] Focused core/driver/transport/panel regressions followed by full configured gate.
- [ ] Fixed CLI build, private native proof and reviewed local queue integration.

Discovered by the source4 full THE-606 gate: 4746 passed, one failed, 3401 not
run, 23 skipped; the failure correctly rejected prose in conflict_paths.

Focused core: 19/19 passed, run af590f63-22ac-44b1-b3b1-836fd27f025e.
Initial focused host: 27/28 passed; one old named-agent fixture failed without
adequate diagnostics. Its exact diagnostic-only rerun passed, so the original
failure cause remains unproven. The seven old agent fixtures now retain owned
private state/registry, Git settings and non-login shell startup through cleanup;
all original outcomes remain unchanged. Source3 focused host: 28/28 passed,
run 0f6a21e9-1e1e-4222-ac9e-9563f9e6af34 (2m24 build, 5.435s tests), including
the unchanged provider transport assertions. This is not a full gate or native
proof. THE-586 remains held; no push. THE-610 is the implementation slice for
the earlier THE-219 finding; its broader acceptance record remains open.

Combined configured `just test` subsequently passed all three contract stages
(2 + 2 + 1) and 8158/8158 workspace tests, with 23 existing configured skips.
Workspace run: `3c0c9464-54f4-4773-ba05-2a6d01f713de`; build 4m51s, tests
201.228s. All 29 candidate hashes matched before/after the gate. Log:
`/tmp/thegn-audit/tasks/the608-full3-just-test.log` (SHA256
`318aed8cf9ca3916fe2428c4a864686687a22c59fdcb3a66ff7536527aadd764`).
This evidence-only task update follows testing; no Rust source changed. The
fixed CLI/native rehearsal and live queue outcome remain unverified.

## THE-219 current acceptance follow-up (cce73f40)

- [x] Reuse the existing THE-610 terminal InfraHold repair; preserve all production retry behavior.
- [x] Add reviewed actual driver/SQLite scripted conflict and red-gate hold/recovery, no-op budget and GateError continuation tests.
- [x] Run current Part A status module:8/8 passed, nextest816c3109-e2ee-4d3f-87dd-1b14cce6a09b, retained `/tmp/thegn-the219-part-a-native-retry-20260914.log`.
- [x] Execute bounded old InfraHold break-to-continue counterfactual:2/2 intended second-fold assertion failures, retained `/tmp/thegn-THE219-counterfactual-native-receipt-20260914.json`.
- [x] Audit all branch-local retry/continue arms and direct CLI/UI dispatch; record the progress argument and its limits in `docs/audits/THE-219-retry-progress.md`.
- [x] Author and independently review Part B owned actual CLI conflict/red-gate hold, next-item and independent plain-drain recovery fixtures.
- [x] Compile and execute both actual Part B CLI tests with exact binary/config/SQL/cleanup receipts.
- [x] Complete current candidate strict lint, scoped specs/reciprocal delivery checks and required native gates.
- [x] Review and land the complete THE-219 acceptance candidate on local main with its exact receipts.

These follow-up boxes concern THE-219 only. They do not complete THE-608,
THE-232 process lifetime, other launch/containment obligations or live queue
acceptance. Earlier shared-change receipts above remain historical evidence for
their stated sources, not substitutes for current Part B/native gates.

THE-219 current Part B execution is retained without reduction in [the shared evidence manifest](../../../docs/audits/maintenance-04-2026-09-15/manifest.json). Strict Clippy and final metadata checks passed; reviewed implementation landed on local main at `c0d3d860a22db0a7fddafb3d338bf40b25248ade`. Tracker closure/readback remains pending. The shared historical live-queue gate and THE-608/THE-610 acceptance are not completed by this update.

## THE-608 scoped acceptance reconciliation — 2026-09-15

- [x] Independently review the current diagnostic driver, exact nullable SQLite replacement, legacy None behavior and panel/transport assertions against the full issue.
- [x] Verify seven current source hashes and nine exact core/driver/panel/authenticated-transport PASS records from native623; retain its unrelated full-run failure.
- [x] Reconcile the fixed normal CLI Part B receipts for stale-field clearing and same-row recovery, preserving the distinction from injected deferred-conflict mapping and authenticated gate-error proof.
- [x] Retain narrowly scoped original review/issue/evidence payloads and exact hashes in the dated audit manifest.
- [x] Verify current33c0 full native:8,465/8,465 passed,26 skipped; nine exact diagnostic selectors and seven current-source hashes match. Retain original failures and exact receipt/log hashes.
- [x] Complete final metadata review, source checks and local landing at bd738963; read back THE-608 Done at 2026-09-15T02:40:29.889Z.

[The dated diagnostic acceptance audit](../../../docs/audits/THE-608-diagnostic-acceptance-2026-09-15.md) records this layered evidence. The old shared fixed-CLI/private-native/live-queue checkbox remains historical and unchecked: these scoped THE-608 receipts do not claim live user queue integration or complete THE-610. No provider, merge, cleanup or push action is authorized by this metadata update.
