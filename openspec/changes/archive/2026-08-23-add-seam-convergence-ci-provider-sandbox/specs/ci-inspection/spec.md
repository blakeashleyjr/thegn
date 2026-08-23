## ADDED Requirements

### Requirement: The CI provider is a sync, object-safe seam

`CiProvider` SHALL use plain `&self` methods (every implementation is process-bound), SHALL implement `Probe` and report its `CiSystem`, and provider selection SHALL return `Box<dyn CiProvider>` — no delegation enum and no runtime handle at call sites.

#### Scenario: A CI call from a blocking thread

- **WHEN** the host fetches runs from `spawn_blocking`
- **THEN** it calls the provider directly with no `block_on`
