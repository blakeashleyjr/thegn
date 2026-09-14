## ADDED Requirements

### Requirement: Native requests use the matching forge host

The public GitHub native layer SHALL only serve origins on github.com and SHALL
reject unsupported hosts before credential lookup. Enterprise origins SHALL
use a host-aware implementation rather than a same-name public repository.

#### Scenario: Enterprise repository shares a public owner and name

- **WHEN** an enterprise repository has the same owner/name as a public repository
- **THEN** the native public GitHub layer does not query or return that repository

#### Scenario: Foreign URL path resembles a GitHub authority

- **WHEN** a foreign origin contains `@github.com` after its URL authority ends
- **THEN** native admission rejects it before token lookup, using the same strict parse for authority and repository identity
- **AND** supported HTTPS, SSH URL and SCP GitHub origins retain their exact owner and repository identity

### Requirement: SDK error types determine fallback and connectivity

GraphQL error envelopes, including partial responses with errors, SHALL use the
intended CLI fallback. Server error text SHALL NOT be interpreted as transport
failure based on words in a repository name. Authentication, rate-limit, and
transport errors SHALL preserve their operation classes.

#### Scenario: Repository name contains connect

- **WHEN** GitHub returns a GraphQL error for that repository
- **THEN** the native layer falls through without adding global offline evidence

#### Scenario: Service returns an HTTP error

- **WHEN** GitHub returns an authentication, rate-limit, or server-error response
- **THEN** the operation remains failed and the answer establishes reachability

### Requirement: Credential helpers have bounded output and lifetime

Credential lookup SHALL bound output and time spent awaiting both process exit
and stdout completion. On timeout or output overflow the owned helper group
SHALL be terminated, including descendants retaining stdout. Credential values
and helper stderr SHALL NOT be included in diagnostics.

#### Scenario: Parent exits while a descendant retains stdout

- **WHEN** a helper parent exits and its descendant keeps the output pipe open
- **THEN** lookup times out and the descendant is terminated instead of pinning
  the refresh worker indefinitely
