## ADDED Requirements

### Requirement: Isolation floors apply to the final execution outcome

Every successful launch preparation SHALL compare the actual final isolation
class with the demanded floor. Host fallback and remote bare execution MUST NOT
bypass fail-closed admission, including after a stronger runtime fails setup.

#### Scenario: No runtime satisfies the launch

- **WHEN** all candidates are unavailable or fail setup and the final host class misses a fail-closed floor
- **THEN** preparation fails before a workload can spawn

#### Scenario: The floor permits degradation

- **WHEN** the final host or remote bare class misses a floor whose policy is degrade
- **THEN** preparation returns the actual class with a floor-specific warning

### Requirement: Backend availability diagnostics state observed evidence

Runtime diagnostics SHALL distinguish executable presence from successful
availability probing. Failed probes MUST NOT be described as proof that a
service is stopped or that a local executable exists on a remote host.

#### Scenario: An installed runtime fails its probe

- **WHEN** the executable exists locally but the availability probe fails
- **THEN** the warning reports unavailability and directs diagnosis of runtime health and access
