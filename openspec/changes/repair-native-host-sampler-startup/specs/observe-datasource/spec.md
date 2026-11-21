## ADDED Requirements

### Requirement: Self-sampling host source defers probes until active

The self-sampling host source (`HostSource::try_new` and
`HostSource::try_new_with`) SHALL keep native collector construction and
sampling on its dedicated worker. If the source is parked before the worker
starts collection, it MUST park without initializing platform collectors.
Activation MUST initialize the sampler on that worker. After the blocking
factory returns, the worker MUST recheck stop and park state before its first
sample or resume priming read, retaining the initialized sampler across park
wakeups. Resume MUST preserve CPU warm-up and publish host samples into the
shared Observe history. Parking is cooperative at the next worker state check;
activation and stop MUST wake a parked worker. Drop normally joins it through
a reaper away from the UI event loop and joins inline only if reaper-thread
creation fails.

This requirement covers the self-sampling constructors. `HostSource::fed` and
Observe's fed history path are supplied by the separate host stats ticker and
do not construct a native collector in the Observe data source.

#### Scenario: Self-sampling host source is hidden before collection starts

- **WHEN** the host source is inactive before its worker begins collection
- **THEN** the worker parks without performing native hardware probes and
  initializes the sampler only after activation

#### Scenario: Source is hidden or stopped during collector construction

- **WHEN** parking or stop is requested while the blocking sampler factory is
  running
- **THEN** the worker observes that state before sampling or resume priming;
  it parks with the constructed sampler retained, or exits without a sample
