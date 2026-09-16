# THE-321 tracker HTTP policy audit

The authenticated Linear, Jira, and Kaneo providers now share
`thegn_svc::issue::http`. A process-wide semaphore permits eight concurrent
logical operations. Each operation starts one 20-second deadline before
queueing and serialization and retains its permit across follow-up requests,
response streaming and decode. Account origin and authorization remain local
to each configured client.

The HTTP client refuses redirects and disables gzip, Brotli, deflate and zstd
decoding. Connect/read limits are five seconds. Responses are capped at 1 MiB,
serialized mutation bodies at 512 KiB, and dynamic inputs at 64 KiB. Status,
identity encoding and JSON content type are checked before bounded streaming.
Errors use bounded policy descriptions and redact underlying network details.

Explicit HTTP and HTTPS endpoints, LAN hosts and self-hosted base paths remain
supported. Origins reject credentials, query and fragment state; request paths
cannot escape the configured origin/base path. Jira and Kaneo construct dynamic
paths and queries through URL encoding. Linear status lookup and mutation,
Jira create and follow-up retrieval, and Kaneo workspace/project expansion each
share a single operation budget.

Native fixtures use local fake HTTP servers with synthetic credentials. They
cover 301/302/307/308 redirects to the same origin, another origin, and a loop,
for GET and POST; all 24 cases require one authenticated source request and zero
target requests. Other fixtures cover byte/encoding/MIME/status limits, queued
serialization, cancellation while queued or streaming, permit recovery and
provider-specific multi-request deadlines. The final source, gate results,
artifact hashes and qualifications are recorded in
[Maintenance 08](maintenance08-2026-09-15.md).

The OpenSpec change is `centralize-tracker-http-policy`. Account-generation
authority, pagination, GitHub CLI process limits and provider-specific business
semantics remain separate issues. No live provider operation or live process
restart is claimed by this verification.
