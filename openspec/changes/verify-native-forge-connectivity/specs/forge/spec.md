## ADDED Requirements

### Requirement: Native SDK outcomes preserve shared connectivity evidence

The native forge request path SHALL record global reachability for typed
repository, authentication, rate-limit and server answers, while preserving
operation classes and intended fallback. Actual transport errors and request
deadlines SHALL record global failure evidence.

#### Scenario: Partial GraphQL answer follows an offline observation

- **WHEN** automatic connectivity has prior failure evidence and the SDK returns data with GraphQL errors
- **THEN** shared connectivity becomes online and clears its failure count
- **AND** the real fallback ladder invokes its CLI layer exactly once

#### Scenario: Typed final error still proves reachability

- **WHEN** the native SDK receives an authentication, rate-limit or server response
- **THEN** shared connectivity records reachability while the typed operation remains failed
- **AND** the fallback layer is not invoked

#### Scenario: Actual transport failure follows successful connectivity

- **WHEN** the native SDK transport fails or the request deadline expires
- **THEN** shared connectivity records failure evidence and the operation remains Offline
- **AND** fallback is not attempted as though the native implementation were absent
