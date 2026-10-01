## ADDED Requirements

### Requirement: Authenticated tracker operations share bounded HTTP resources

Linear, Jira, and Kaneo adapters SHALL share one process-wide capacity budget of
eight logical operations. Each logical operation SHALL use one absolute
20-second deadline across capacity admission, request serialization, every
nested request, response streaming, and bounded synchronous decoding. Dropping
the provider future MUST release its capacity and owned response body. A later
request in the same operation MUST NOT restart the deadline or acquire a
second capacity permit.

#### Scenario: a mutation requires several requests

- **WHEN** a provider resolves a status, submits a mutation, and refreshes an issue
- **THEN** all requests use the original operation permit and deadline
- **AND** expiry prevents a subsequent mutation or refresh from being dispatched

#### Scenario: cancellation occurs while waiting or reading

- **WHEN** an operation future is dropped while waiting for capacity or streaming a response
- **THEN** its resources are released so another operation can proceed

### Requirement: Tracker HTTP requests retain their admitted account origin

Authenticated requests SHALL retain the configured scheme, host, effective
port, and base path. Explicit self-hosted HTTP and HTTPS base paths SHALL remain
supported. Redirects, origin/base-path escapes, and response decompression MUST
be disabled. Nonidentity content encodings MUST be refused. Request bodies
SHALL be limited to 512 KiB during serialization and response bodies SHALL be
streamed through a 1 MiB limit before JSON decoding. JSON responses SHALL use a
JSON media type. Diagnostics MUST NOT include credentials, query values, or
arbitrary response bodies.

#### Scenario: a response redirects to another route

- **WHEN** a tracker responds with a redirect
- **THEN** the operation refuses it without sending credentials to the target

#### Scenario: a response streams beyond the body limit

- **WHEN** the response exceeds 1 MiB, including without Content-Length
- **THEN** reading stops with a bounded error and the operation releases its resources

#### Scenario: a configured service uses a base path

- **WHEN** Jira or Kaneo is configured under an HTTP or HTTPS base path
- **THEN** all API requests preserve that admitted base path
