# Maintenance 08 verification — 2026-09-15

THE-321 and THE-322 share reviewed source
`759cd1ad252cb131c87cf9078923c31203cdfeb5`, based on local main
`fadd7164ce46b684e1e23ee2174aa876fc619d42`.

## Changes

**THE-321:** Linear, Jira and Kaneo use one bounded HTTP policy with a
process-wide eight-operation budget, one absolute deadline per logical
operation, redirects disabled, decompression disabled, response/request byte
limits, origin checks, and redacted errors. Explicit self-hosted HTTP endpoints
and base paths remain supported. Multi-request operations retain one budget.

**THE-322:** Built-in identifiers are validated before CLI operands or encoded
URL segments are constructed. GitHub repository/number and response authority
checks reject option/path confusion. Jira and Kaneo paths and query values use
structured encoding. Provider and plugin responses are checked before use;
Linear workflow-state values are bounded and escaped before GraphQL mutation.
Control paths encode the complete identity once and the actual server validates
it after decoding. Plugin keys retain bounded opaque UTF-8 semantics. Malformed
cached identities are filtered before panel use and refresh diffing.

## Verification

The final evidence contains **107 distinct passing tests**: 104 service tests
and three host tests. The final 24-case redirect matrix exercises
301/302/307/308 × same-origin/cross-origin/loop × GET/POST, requiring typed
refusal, exactly one authenticated source request, and zero target requests.
Other fixtures exercise bounded HTTP bodies, encoding/MIME/status refusal,
queued serialization, cancellation and permit recovery, each provider's logical
multi-request deadline, provider identity/schema compatibility, actual control
router round trips and zero-effect refusals, plugins, and real cache hydration.

Strict workspace Clippy with all targets and `-D warnings` passed at `4256d237`.
The later changes affect only two test functions; their production prefixes are
byte-identical. Both corrections passed scoped service Clippy. A shared
svc/host test build was followed by two service-only harness builds for the
fixture corrections, with no release build. Each build used one Cargo job,
niceness 10 and a verified one-CPU quota. Focused tests used isolated processes,
private XDG directories, one test thread and a 30-second outer timeout.

The manifest maps each test to its actual binary and tested source: three
unchanged host tests at `4256d237`, 103 unchanged service tests at `f46e043d`,
and the expanded redirect fixture at `759cd1ad`. The initial 107-test run had
one fixture-construction failure (missing required `updated_at_ms`); after
correction, all 104 service tests passed. Final acceptance review then expanded
the redirect matrix and reran that changed fixture successfully. Earlier lint
failures and the failed fixture receipt remain preserved, with no relabeling.

## Review and limits

Root and independent Luna reviews cover the HTTP policy, identifier admission,
Linear mutation escaping, integration/lint corrections, test corrections and
host wrapper behavior. Two host branches were source-reviewed but not directly
executed: invalid registry-row refusal and malformed-old-cache filtering inside
the refresh worker. Their underlying service contracts and the separate actual
panel hydration path are tested; this is not an exhaustive coverage claim.

This is Linux native evidence with synthetic credentials and local fake
providers. It does not claim Windows runtime, live provider, production DB,
full-workspace native, release performance or coverage-percentage results.
THE-315 still owns GitHub creation repository precedence; THE-324 owns exact
account/generation authority, THE-323 pagination, and THE-314 CLI process bounds.
Provider-specific transition semantics remain separate. No push or live process
restart is part of this delivery.

[The evidence manifest](maintenance08-2026-09-15/manifest.json) records source,
binary, test, raw-log and review hashes. Final delivery gates precede local-main
landing; Linear completion is recorded only after the actual landing.
