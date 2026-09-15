# Design

`thegn_svc::issue::http` owns `TrackerHttpBudget`, account-local
`TrackerHttpClient`, and `TrackerHttpOperation`. `TrackerHttpBudget::process`
uses `OnceLock<Arc<TrackerHttpBudget>>`; tests inject a finite budget. A
`TrackerHttpOperation` starts its deadline before serialization and permit wait,
acquires one owned semaphore permit, and retains it through headers, bounded
streaming, synchronous decode, and cleanup.

`TrackerHttpClient` parses an origin once and compares every joined request's
scheme, host, and effective port with it. Its reqwest builder pins
`Policy::none`, all four `no_*` decompression methods, five-second connect/read
limits, and a defensive twenty-second request ceiling. Response status,
content type, content encoding, and content length are checked before the
bounded `bytes_stream` reader. A bounded serde JSON writer prevents oversized
outbound bodies before request dispatch.

Linear `gql`, Jira `get/post/put`, and Kaneo `get/send_body/delete_req` accept a
mutable operation. Public provider operations construct one operation and pass
it through all nested calls. Kaneo search delegates to an operation-aware list
helper; Linear status lookup and mutation share the operation; Jira transition,
summary, and refresh calls share the operation.

The test fixture is an in-process Axum server covering valid JSON, redirect
refusal, non-identity encoding refusal, oversized response refusal, explicit
LAN HTTP origin admission, and bounded request serialization. Additional
short-budget permit/stream fixtures should be extended with deterministic
synchronization before the compiled gate; production constants remain fixed.
