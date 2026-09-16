# THE321 HTTP budget and provider sequence — independent adversarial review

Date: 2026-09-15
Reviewer: Luna high

## Verdict

**Scoped approval for the root-controlled compiled gates.** The shared tracker HTTP
budget, operation deadline, bounded response/request handling, cancellation
ownership, and current Linear/Jira/Kaneo sequence fixtures in maintenance08 are
source-consistent and have no new blocker in this review. This is an HTTP/sequence
review only; the separately pending Linear production revision remains outside
this verdict, so this report must not be used as final Linear issue closure.

No Cargo, compiler, test, native, provider, authenticated HTTP, Linear, database,
canonical, or main-tree mutation was performed.

## Exact provenance

- Reviewed checkout: `/tmp/thegn-maintenance08-combined-20260915`
- Reviewed commit: `834f324245f8840b34c4eefb7a210fbbf638bcef`
- Reviewed tree: `f985e848f45686fa2b51d87442d1f556d0231339`
- HTTP/provider integration anchor: `71c8354b986d39a4c5718b49a109053b7f222576`
- Baseline: `fadd7164ce46b684e1e23ee2174aa876fc619d42`
- Root disposition read: `/tmp/thegn-maintenance08-primary-adversarial-disposition-20260915.json`

Current source SHA-256 values:

- `crates/thegn-svc/src/issue/http.rs` — `77b13e8c927c51158ba7c19012d5bc9ff2ff42861a5378f6479d54ae1c43bc86`
- `crates/thegn-svc/src/issue/linear.rs` — `c410bdcd16df3208d3f329d50a1813d5c6dee4423a7592b612b649fbe9e0fef3`
- `crates/thegn-svc/src/issue/jira.rs` — `1dc3a770ef7a0144d85098603acb9945520a35b80bcb6cc95ab632f54f905eb9`
- `crates/thegn-svc/src/issue/kaneo.rs` — `86c7047f2d3956b468d7b48af9b6118e5a33471ab375ab870aed7ba0beb82f21`
- `crates/thegn-svc/src/issue/mod.rs` — `a5e24e75a19d7887069335313c4ddad0bbfa2755e820204ec007709e68cad13c`
- `crates/thegn-host/src/hydrate_tracker.rs` — `83d69ac900d8c2deadf3fc0b002b4c761efe267b32753accfbf09d76980e8278`
- `crates/thegn-host/src/hydrate_tests.rs` — `708a409c20f9b80cabef440ed0c49baa4d6455e9af0d7577648f85aa65691b45`

## Source findings

`TrackerHttpBudget::process` owns a process-wide `OnceLock<Arc<TrackerHttpBudget>>`
and the injected constructor supports isolated fixtures. Each
`TrackerHttpOperation` acquires one `OwnedSemaphorePermit`, computes one absolute
20-second deadline, retains the permit through send, bounded streaming, decode,
and post-decode deadline checks, and releases it by normal RAII on error or future
drop. `operation_with_timeout` is test-only while production `operation` is
available in normal builds.

The request path checks the deadline before serialization and again before send;
the bounded serde writer caps request bytes. Responses reject redirects,
non-identity encodings, non-success/auth statuses, oversized declared bodies, and
wrong/missing JSON MIME before a streamed cap check. `read_bounded` drops the
response on stream, cap, or deadline failure, and JSON decoding is performed only
after the bounded buffer is held by the operation. The implementation maps
malformed JSON through the typed parse error. The fixture matrix directly covers
redirect refusal with zero target hits, encoding/MIME/auth refusal, declared and
chunked body caps, and bounded request serialization.

The deterministic response-consumption hook expires only the active operation
after a successful response. This is sufficient to distinguish a fresh-deadline
mutant: the Linear status lookup, Jira create follow-up, and Kaneo workspace
expansion each observe one first request and refuse the second before server I/O.
The queue-abort and pending-stream-abort fixtures drop pending futures/tasks while
a permit is held, then acquire the same one-permit budget successfully. Fixture
servers are explicitly aborted and joined.

All inspected Linear, Jira, and Kaneo public issue methods create one operation
and pass it through nested helpers. The reviewed current Kaneo conversion fix
returns `task_to_domain(...)` directly so fallible conversion is propagated. Its
best-effort optional session/comments/label reads retain existing behavior; the
Kaneo expansion fixture specifically propagates a timeout before the next project
fetch. The 551fda53 origin normalization fix preserves root origins and configured
self-hosted paths, and 71c8354b’s explicit HTTPS-port case rejects a foreign
authority port.

The 834f3242 host test is a valid ancillary regression: it populates an in-memory
cache with one valid Jira row plus traversal, provider-mismatch, and javascript URL
rows, asserts only the valid row reaches `PanelData`, and asserts the stale cache
JSON is unchanged. It exercises the existing `validate_issue_identity` boundary
without adding a write or network effect.

## Qualifications retained

- The source review does not claim that every disconnected Axum handler or
  detached hydration worker supplies a cancellation token. The demonstrated
  cancellation contract is future drop plus the absolute operation deadline;
  detached hydration remains timeout-bound.
- The current generic HTTP fixture maps malformed JSON correctly, but does not
  include a separate malformed-body route. This is a test coverage note, not a
  source blocker, because the bounded decode/error path is explicit and the
  compiled gate remains required.
- Linear production changes called out by the root disposition are a separate
  pending revision and require their own source/native admission. This review
  covers only the shared HTTP operation and the current provider sequence wiring.
- The existing Jira/Kaneo provider-derived structured path/query identifier
  encoding limitation remains outside THE321 and is not reclassified here.
