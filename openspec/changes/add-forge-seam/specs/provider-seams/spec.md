## MODIFIED Requirements

### Requirement: Provider seams share one vocabulary

`thegn-core` SHALL provide a pure `seam` module (no tokio/termwiz dependency) defining the vocabulary every provider seam uses: a `BoxFuture` alias, an `ErrorClass` classification (`Unsupported`, `NotInstalled`, `NotConfigured`, `Auth`, `Transient`, `NotFound`, `RateLimited`, `Other`), a `SeamError` trait exposing `class()` and a constructor `unsupported(op)`, an `Availability` state (`Ready`, `Degraded`, `Unavailable`), a `ProbeReport` record, a `Probe` trait, and a `Kind` trait (`ALL`, `as_str`, `is_reserved`). Every seam trait SHALL be object-safe: a seam is **sync** (plain `&self` methods) when every implementation is process-bound or wraps its own async client and its callers run on blocking threads (git, forge, sandbox, editor); it is **async** (`BoxFuture` methods) only when a native async client is the primary path and callers are async (control API).

#### Scenario: Errors classify for ladders

- **WHEN** a seam error is asked whether a degradation ladder should fall through past it
- **THEN** `Unsupported`, `NotInstalled` and `NotConfigured` answer true and every other class answers false

#### Scenario: A blocking seam is sync

- **WHEN** a seam's implementations are all subprocess- or block_on-based
- **THEN** its trait uses plain `&self` methods and `Ladder::try_each_sync`
