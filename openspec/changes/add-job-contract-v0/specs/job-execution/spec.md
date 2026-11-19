## ADDED Requirements

### Requirement: Portable job inputs and results

The submission parser SHALL validate the version, reject unknown execution fields and ambiguous environments or commands, expose positive one-based shards and require one offering to satisfy all required features. The result schema SHALL represent a terminal rejection with its reason and zero attempts.

#### Scenario: Runnable work and unsupported work

- **WHEN** a job requires an OCI image, shared memory, services and JUnit parsing
- **THEN** the authority derives the complete feature set and can reject each offering missing any required feature

#### Scenario: Unknown fields and malformed commands

- **WHEN** a job names an unknown execution field or supplies both a script and an argv vector for one step
- **THEN** submission fails before placement

### Requirement: Canonical snapshot identity

The CAS library SHALL encode REAPI Directory objects with entries sorted within each kind, unique names across kinds, and SHA-256 digests including byte size.

#### Scenario: Different enumeration order

- **WHEN** two encoders receive the same files, executable bits, child directory references and symlinks in different orders
- **THEN** they produce identical bytes and the same root digest

### Requirement: Fenced publication

The authority SHALL require an exact worker, run and epoch match in the committing phase of a live lease for first publication. The authoritative store SHALL serialize a shard claim, renewal, revocation or publication against competing transitions. It SHALL return the receipt of a previously accepted identical publication on replay.

#### Scenario: Reassignment after expiry

- **WHEN** a worker misses renewal and another worker claims the shard at the next epoch
- **THEN** the old attempt cannot advance, renew or publish and the current attempt can publish once

#### Scenario: Acknowledgement is lost

- **WHEN** an authenticated worker resends its accepted commit after expiry
- **THEN** the same publication returns its stored receipt and a conflicting publication fails

### Requirement: Bounded guest framing

The guest and host SHALL negotiate a common protocol and spec version and use a four-byte big-endian protobuf frame length bounded by both endpoints and their bootstrap limit.

#### Scenario: Excessive advertised frame

- **WHEN** a header announces a frame larger than the accepted maximum
- **THEN** the reader rejects it before allocating or reading the payload
