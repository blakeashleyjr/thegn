# Event loop

## ADDED Requirements

### Requirement: Terminal history search runs off the event loop

The terminal history search overlay SHALL perform matching on an overlay-owned
worker over an owned snapshot of the in-scope histories, retain the existing
scope filters and result ordering, deliver ranked results in bounded batches,
and discard work from superseded queries. Each request SHALL carry the
generation stamped when it was queued, and every result SHALL be tagged with the
generation of the request that produced it. Closing the overlay SHALL
invalidate active and pending work so the worker stops at its next scan batch.
No terminal history search worker SHALL run while the overlay is closed.

#### Scenario: A newer query replaces an active search

- **WHEN** the user enters another query while a history scan is active
- **THEN** the earlier scan stops at its next batch and none of its results are shown

#### Scenario: Results arrive behind a searching status

- **WHEN** the worker has results ready
- **THEN** the overlay shows a searching status until the final batch arrives

#### Scenario: Enter before the first batch

- **WHEN** the user presses Enter before the typed query has any result
- **THEN** the overlay jumps to the best hit once it arrives, or dismisses if nothing matched

#### Scenario: The overlay closes during a scan

- **WHEN** the user dismisses the overlay during a scan
- **THEN** the active scan is invalidated and no further work is scheduled

#### Scenario: The worker dies

- **WHEN** the worker thread stops unexpectedly
- **THEN** the overlay shows an error and scores on the loop instead of respawning per keystroke
