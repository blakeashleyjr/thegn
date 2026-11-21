### Requirement: Native host sampler defers probes until active

The built-in host source SHALL keep native collector construction and sampling
on its dedicated worker. If the source is parked before the worker starts
collection, it MUST park without initializing platform collectors; activation
MUST initialize the sampler on that worker, preserve CPU warm-up on resume, and
publish host samples into the shared Observe history. Parking and drop MUST
remain wakeable, with drop joining away from the UI event loop.

#### Scenario: Host source is hidden before collection starts

- **WHEN** the host source is inactive before its worker begins collection
- **THEN** the worker parks without performing native hardware probes and
  initializes the sampler only after activation
