# THE-321 tracker HTTP policy audit

Date: 2026-09-15
Candidate: private Luna implementation from canonical `f293594d4a8516f940540b4eda60a9a98ac1d7a1`

The candidate centralizes authenticated Linear, Jira, and Kaneo HTTP calls in
`thegn_svc::issue::http`. `TrackerHttpBudget::process` provides one process-wide
`Arc<Semaphore>` with eight permits. Each public provider operation starts one
20-second deadline before serialization/queueing and passes one operation through
all internal requests, response streaming, decode, and cleanup. The account
origin and authorization remain account-local.

The reqwest client pins `Policy::none`, `no_gzip`, `no_brotli`, `no_deflate`, and
`no_zstd`, plus five-second connect/read limits and a defensive 20-second
request ceiling. Origins accept explicitly configured HTTP or HTTPS hosts,
including LAN hosts, while refusing credentials, path/query/fragment state, and
request-origin escapes. Response status, identity-only encoding, JSON MIME, and
content length are checked before a cap-checked `bytes_stream`; JSON mutation
bodies use a 512 KiB serde writer. Errors avoid copying response bodies and use
static provider/phase policy messages.

Source fixtures cover explicit LAN HTTP admission, valid JSON, redirect refusal,
non-identity encoding refusal, wrong MIME, auth status, oversized streamed
response, and oversized request serialization using an in-process Axum server.
The source-only candidate was formatted and passed `git diff --check`. Cargo,
compiler, lint, tests, native/provider calls, authenticated requests, and
canonical/Linear mutations were intentionally not run; root and independent
review must perform the compiled gates.

The OpenSpec change is `openspec/changes/centralize-tracker-http-policy/` and
its reciprocal delivery metadata is sorted in `delivery/issues.json` and
`delivery/index.json`. Pagination, account authority, identifier grammar,
plugin HTTP clients, and host refresh lifecycle remain outside THE-321.
