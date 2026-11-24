# Platform: native Windows

## ADDED Requirements

### Requirement: Windows sandbox helper requests use a bounded portable wire protocol

Core and the standalone Windows sandbox helper SHALL share the request, probe,
status and peer types of the serde-only `thegn-sandbox-proto` crate. The
protocol SHALL use version 1 and reject unknown request fields
(`deny_unknown_fields`) so version skew fails closed instead of dropping a
field. The request JSON SHALL travel in numbered `THEGN_WINSB_REQ_0..N`
environment entries: each chunk at most 8 KiB, the whole JSON at most 1 MiB, and
chunk boundaries on Unicode scalar values so a UTF-16 conversion never meets
half a surrogate pair. Decoding SHALL remove every request-prefixed entry from
the supplied environment before it validates anything, including when decoding
fails, so the request and any proxy URL or lease token it carries never reach
the workload environment.

#### Scenario: Request transport is bounded and scrubbed

- **WHEN** the helper decodes a chunked version-1 request
- **THEN** it receives the original Unicode request and the workload
  environment contains no `THEGN_WINSB_REQ_*` values

#### Scenario: Malformed transport is rejected without environment leakage

- **WHEN** request chunks are missing, duplicated, named with a non-decimal
  index, oversized or not valid JSON
- **THEN** decoding fails with a fixed error that carries no request content,
  and all request-prefixed values are already removed from the environment

### Requirement: Windows argv quoting matches the Windows parser contract

`win_cmdline::join` SHALL apply the MSVCRT/`CommandLineToArgvW` rules: quote an
argument that is empty or contains a space, a tab or a quote; double the
backslashes before an embedded quote and add one escape backslash; double
trailing backslashes inside a quoted argument; refuse NUL; and refuse a command
line longer than 32,766 UTF-16 units. `win_cmdline::split` SHALL serve only as
the pure fixture oracle until the Wine and native Windows comparisons are
recorded, so its agreement with `join` is not evidence about the real API.

#### Scenario: Quoting preserves argv fixtures

- **WHEN** argv fixtures containing empty strings, whitespace, quotes,
  backslashes and Unicode are joined and parsed by the fixture oracle
- **THEN** the original argv is recovered
