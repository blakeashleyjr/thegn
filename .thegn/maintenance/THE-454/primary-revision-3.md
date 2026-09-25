Primary review of adversarial 488 and central compile: revisions required

Read .thegn/pipeline/THE-454/maintenance-adversarial/488.md and prior scope instructions. Implement its two findings and the focused validation repairs below. No new features or event/parser-admission expansion. No worker Cargo/builds; primary centrally reruns after handoff. Source artifacts must accurately say tests remain pending.

1. Set an explicit small finite idle connection bound per host and finite idle lifetime for both shared policy pools. Preserve cross-router reuse and request-local credentials. Add a behavioral reuse/bounded-connection regression rather than only builder-text assertions. Do not claim this alone globally bounds concurrent accounts; THE-465 owns aggregate admission.
2. Distinguish DNS destination-policy refusal from ordinary resolver/network failure at CalendarHttpError/CalendarError. A typed resolver error marker inspected through the source chain is preferable to matching display strings, a mutable shared flag, or re-resolving DNS separately (which would race/rebind). Preserve actual admitted-address resolution as the connection path. Assert same-client changed DNS and mixed-answer policy classification and non-transience, while genuine DNS/network failure remains correctly classified. Keep diagnostics redacted.
3. Add a deterministic connect-stage timeout fixture via a test resolver/connector seam, distinct from total/header and read-idle timeouts. No public network or unroutable-address timing guesses. Add actual CalDAV exact/oversized success and error response coverage at its body-read seam. Replace the axum false Content-Length/empty body fixture with raw HTTP where framework normalization cannot hide the asserted limit. Preserve primary shared 250 ms deadline test where each response individually fits but together exceed the operation budget.
4. Central focused build at frozen f05d1120 failed before tests. Exact log is /home/blake/code/thegn/target/maintenance-review-20260917/next-native-batch/calendar-focused.log. Fix all errors/warnings in that log:
   - CountingListener::local_addr trait not in scope (calendar/tests.rs around674).
   - DNS fixture usize ports need checked u16 conversion (http.rs around668/675).
   - issue/http.rs test first.chain(never) lacks futures_util::StreamExt after shared-reader extraction.
   - redirect fixture reads request URI after into_body moves it (calendar/tests.rs around239/246); capture request parts before consuming body.
   - unused request in shared-deadline fixture.
   - parse_multistatus is now only used by tests; gate the wrapper appropriately instead of suppressing unrelated lints.
     Do not edit the frozen gate checkout or clear Cargo caches. Compilation and tests are unverified until the next central result. Every source change remains subject to primary and fresh independent review.
