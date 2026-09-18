# Primary plan review: implementation approved

Implement the investigated calendar transport defect end to end. Read the investigation 473.md and full issue. Preserve tracker HTTP guarantees while extracting only genuinely shared transport primitives; retain tracker adapters and tests.

Decisions and required refinements:

- Disable all redirects and reject every 3xx without reading/logging Location or replaying request bodies.
- HTTP/HTTPS and ICS webcal only; normalize webcal to HTTPS; reject userinfo and malformed URLs. Subscription query credentials may be accepted but never disclosed in errors, Debug, logs, or persisted diagnostics.
- Add an explicit trusted calendar account security option `allow_private_network`, default false, only if required to retain intentional LAN calendars. This is a permission boundary for this security fix. Deny private, loopback, link-local, unspecified, multicast and other non-global IPv4/IPv6 destinations by default, including literals, mapped forms and DNS answers. When explicitly permitted, allow intended unicast local addresses but still reject unspecified/multicast. Validate every actual DNS resolution used by the connector; do not validate once then let reqwest resolve independently. Mixed forbidden/allowed answers fail closed. Test rebinding and numeric/IPv6 forms.
- Disable ambient proxy use explicitly for calendar transport, document this security policy, and test proxy env cannot bypass destination admission. No proxy configuration feature in this fix.
- Retain process-shared bounded client pools for public/private policies, request-local auth, no cookie store or default auth. Avoid unbounded per-origin caches.
- Prefer the existing safe HTTP identity-encoding policy: request identity, disable automatic decompression, reject unsupported Content-Encoding before reading. Document that compressed responses are rejected; do not claim decompressed support. Bound streaming success bodies at 32 MiB before growth and drop error bodies without buffering. Test cap/cap+1, chunked, false Content-Length and encoded bombs.
- Distinct connect/read-idle/total deadlines; CalDAV token fallback must reuse the same absolute operation deadline and have a bounded retry count. Bound sync token/request body size before XML allocation. Never expose secrets via parse/network errors.
- Meaningful deterministic fixtures for all redirect statuses, no target requests/body replay, address/proxy policy, stream limits/deadlines, repeated router reuse and account isolation. Preserve existing cache/error behavior and documented MIME compatibility.

Authorized files: service shared HTTP module/lib wiring, issue HTTP adapters needed for extraction, calendar module/backends/tests; core calendar config/validation/tests; schema/example/docs changes for security compatibility; host calendar hydration only if needed. Coordinate: THE-465/THE-458 have not started implementation; do not implement their separate work. Cargo dependency additions only when already justified by a pure URL/parser or existing resolver seam; no vendor/network research required.

Do not compile. Primary will run focused and full Rust gates centrally after source review, minimizing builds. Commit implementation, tests and handoff; report acceptance coverage and exact unrun commands. No issue closure until adversarial review, primary review, full combined gate and native landing.
