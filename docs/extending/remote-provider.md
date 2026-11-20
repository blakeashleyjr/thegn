# Add a remote sandbox provider

The checklist below collects the rules Sprites, Vercel, Fly, and VPS were fixed to obey. Each rule names what enforces it, so “did I forget something?” has a mechanical answer.

## Wire it up

Start with the [Remote sandbox providers section](provider-impl.md#remote-sandbox-providers) of the general provider recipe:

- Implement `RemoteProvider` and add the `Provider` arm.
- Register the kind in the provider factory and `EnvProviderKind`, whose capability methods provide the caps.
- Use the hidden `<kind>-exec` self-bridge when there is no SSH, and keep `exec_api()` false until the resident bridge is verified.
- Record intent before create in the `vps::registry` ledger, including the account fingerprint.
- Add a lifetime reaper whose `tick` returns early when no environment uses the kind.
- Refuse unsupported options at admission, including `host_cache` and `connect = "ssh"` when the provider cannot implement them.

## Rules and what enforces them

The shared cases live in [`provider_conformance.rs`](../../crates/thegn-svc/src/provider_conformance.rs); provider-specific tests and bridges fill in the behavior that a fake or live API can observe.

| rule | enforced by | backed today by |
| --- | --- | --- |
| The ledger record exists before the create request. | `create_records_before_request` | Sprites ([ledger](../../crates/thegn-svc/src/sprites/ledger.rs)), Vercel ([provider tests](../../crates/thegn-svc/src/vercel/provider_tests.rs)) |
| Destroying a missing sandbox is success. | `destroy_missing_is_ok` | all four: Sprites, Vercel, Fly, and VPS |
| Never destroy a sandbox this host did not record; send no DELETE. | `destroy_unrecorded_refused` | Sprites, Vercel |
| Keep the record until absence is confirmed. | `destroy_keeps_record_until_gone` | all four |
| Destroy is idempotent. | `destroy_is_idempotent` | all four |
| An inventory is `Complete` only when the walk finished; `list()` refuses anything else; a `Partial` keeps every row it read. | `unfinished_listing_is_never_complete`, `inventory_partial_keeps_created`; use [`crate::vps::Inventory`](../../crates/thegn-svc/src/vps/mod.rs) and [`PartialReason`](../../crates/thegn-svc/src/vps/mod.rs), and never invent a third completeness type. | Sprites, Vercel, VPS |
| Reads (`list`, `inventory`, `fleet cost`) never wake or resume. | `reads_never_resume` | Sprites, Vercel |
| A one-shot exec returns the real exit code and output. | `run_captures_exit_and_output` | Sprites, Vercel |
| A stream without an exit status is a failure, never 0; undocumented frames are typed errors. | `exec_without_exit_fails`; for example, Sprites’ `ExecProtocolError` | Sprites; Vercel enforces it in client tests |
| A call refused because the session stopped is retried at most once, only when waking is allowed, and never after the request was accepted. | `stopped_run_is_retried_once`; `Wake::Never` and the after-acceptance half are enforced by Vercel `provider_tests.rs` (`run_exec_under_never_reports_stopped_without_retry`) and by review. | Vercel |
| Respect `max_instances`. | `create_respects_cap` | Vercel |
| Stop is idempotent and resume works. A 410 on stop is success; an undocumented 410 on delete keeps the record. | `stop_resume_round_trip` | Vercel |
| Egress never widens: refuse what the provider cannot express, never fall back to allow-all. | `egress_never_widens` | Sprites, Vercel |
| Never delete user state on the user’s behalf because of an expired or unknown state; return a typed refusal that says what to do. | Review the refusal path, such as Vercel `SnapshotExpired`, and tell the user to review it. | Vercel |
| A slow wake prints a notice. | `with_slow_notice`, `WAKE_NOTICE_AFTER`, and a `<kind>_wake_line` | Host [`sprite_bridge.rs`](../../crates/thegn-host/src/sprite_bridge.rs), also used by [`vercel_bridge.rs`](../../crates/thegn-host/src/vercel_bridge.rs) |
| Keep typed errors private to the provider, and downcast at the call site. A shared vocabulary waits until a caller outside the provider needs one. | Provider-specific error types and call-site downcasts | Sprites, Vercel |

## Opt into the conformance suite

Add a fixture test in [`crates/thegn-svc/tests/provider_conformance.rs`](../../crates/thegn-svc/tests/provider_conformance.rs), or in the provider’s module tests for a fake API (as Vercel does). `Fixture` derives `Default`; start from `Fixture { created, missing, …, ..Default::default() }`, fill every optional field the fake can back, and pin `report.skipped` with `assert_eq!`.

Name each fixture field for the case it backs, following the [`Fixture` doc comments](../../crates/thegn-svc/src/provider_conformance.rs):

- `created`: the sandbox returned by create and owned by the provider; it backs `inventory_contains_created` and is used by inventory, exec, stop/resume, and teardown cases.
- `missing`: a name with no sandbox; it backs `destroy_missing_is_ok`.
- `foreign`: a visible sandbox this provider did not create; it backs `destroy_unrecorded_refused`.
- `supports_run` and `run_expect`: whether exec is available and its expected `(exit code, output substring)`; they back `run_captures_exit_and_output`.
- `ledger_at_create`: whether the ledger existed when create arrived; it backs `create_records_before_request`.
- `deletes_for`: DELETE counts; it proves `destroy_unrecorded_refused` sent no DELETE.
- `resumes`: resume-request counts; it backs `reads_never_resume`.
- `cap`: the configured `max_instances`; it backs `create_respects_cap`.
- `supports_stop`: whether suspend and resume are backed; it opts into `stop_resume_round_trip`.
- `hold_after_delete` and `recorded`: hold the sandbox present after DELETE and probe the ownership record; they back `destroy_keeps_record_until_gone`.

Every fixture test pins its exact skipped list. Adding a case means updating all four lists, so an accidentally unbacked case cannot disappear silently.

## Write the live check

For a new `test/<kind>-live.sh` (see [`sprites-live.sh`](../../test/sprites-live.sh)), do nothing without the spend flags and `THEGN_CHANNEL=dev`; print the plan in that mode. Source [`test/lib/live.sh`](../../test/lib/live.sh), use `step` (the wrapper over `live_step`), `live_preflight_capture` or `live_preflight_check` for the credential line, `live_listing` for provider-neutral presence and absence, and `live_finish` for the summary.

- Declare `LIVE_STARTUP` for cold create, the first command or prompt, stop, and resume or wake.
- Prove that stop and resume reached the provider by polling row state; Vercel uses `8s` and `8r`.
- If the provider accepts a memory limit, prove it in the guest; Sprites uses `5d`. Emit `WARN`, not `FAIL`, when the provider accepts but does not enforce the limit.
- Teardown keeps the ledger when absence is not proven. `live_listing` counts absence only from a complete listing, while a row is present from any listing.
- Add plan-line and stub-run assertions to [`test/live-plan-gate-test.sh`](../../test/live-plan-gate-test.sh).

Code comments cite the documented operation, for example “OpenAPI: POST …/stop, 410”; they do not cite a saved local file URL.

See the [extension index](README.md) for the neighboring recipes.
