## ADDED Requirements

### Requirement: Self-sampling host source defers probes until active

The self-sampling host source (`HostSource::try_new` and
`HostSource::try_new_with`) SHALL keep native collector construction and
sampling on its dedicated worker. The public constructor starts active. At
the worker's state check before entering the factory, if the source is
observed parked, the worker MUST park without initializing platform
collectors. The sampler initializes on that worker when a state check observes
the source active. A park request after the worker has passed this check does
not cancel an in-progress factory; construction may finish before the next
state check. After the blocking factory returns, the worker MUST recheck stop
and park state before sampling or resume priming: it exits if stop is observed,
parks with the
constructed sampler retained if inactive is observed, and is eligible to
sample if active is observed. This initialization guarantee applies only up
to the first active check when no collector exists. Resume MUST preserve CPU
warm-up and publish host samples into the shared Observe history. Parking is
cooperative at the next worker state check; activation and stop MUST wake a
parked worker. Drop normally joins it through a reaper away from the UI event
loop and joins inline only if reaper-thread creation fails.

This requirement covers the self-sampling constructors. `HostSource::fed`
accepts any supplied `HostFeed` and does not construct a native collector. The
shipped host app registry supplies the process-wide host stats ticker's feed
to this constructor.

#### Scenario: Worker observes an initially parked source before collection

- **WHEN** the worker observes the host source inactive at its state check
  before entering the sampler factory
- **THEN** the worker parks without performing native hardware probes and
  initializes the sampler only after activation

#### Scenario: Source becomes inactive or stopped during collector construction

- **WHEN** the source becomes inactive or stop is requested while the blocking
  sampler factory is running
- **THEN** after construction, the worker rechecks state before sampling or
  resume priming; it exits if stop is observed, parks with the constructed
  sampler retained if inactive is observed, or may sample if active is
  observed

#### Scenario: Shipped registry supplies its ticker feed to Observe

- **WHEN** the shipped host app registry builds Observe's `HostSource::fed`
  source
- **THEN** the shipped registry supplies the process-wide host stats ticker's
  feed; `HostSource::fed` remains able to accept any caller-supplied
  `HostFeed` and does not construct a native collector itself
