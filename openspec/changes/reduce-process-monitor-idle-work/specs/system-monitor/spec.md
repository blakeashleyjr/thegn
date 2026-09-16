## ADDED Requirements

### Requirement: Hidden process sampling is parked and owned

The process sampler SHALL use fallible tracked thread admission, indefinite
hidden/paused parking, and at least two seconds between enabled scan starts.
It SHALL retain at most one unpublished snapshot and preserve a separate
in-flight collection. Cancellation SHALL revoke stale publication, and a parked
acknowledgement SHALL follow any earlier collection, wake attempt and reset.
An unfinished OS call SHALL remain owned with a Held cleanup outcome under the
application deadline. Failure SHALL be observable without requiring a sample.

#### Scenario: Processes is hidden or paused

- **WHEN** the worker acknowledges the current hidden request
- **THEN** it schedules no timed wait, collection or publication until a new enabled request
- **AND** consumer closure wakes cancellation without requiring another scan

#### Scenario: Hide and reopen race with collection or terminal notification

- **WHEN** a prior-generation collection or wake remains in flight across hide and reopen
- **THEN** the old result is not delivered and the reset is not skipped
- **AND** the first fresh sample is unprimed while scan starts respect the minimum interval

#### Scenario: Cleanup deadline precedes actual thread return

- **WHEN** cancellation occurs during an OS call or after a completion receipt but before thread return
- **THEN** cleanup reports Held and retains the exact handle
- **AND** new worker admission cannot replace it until exact finished-handle retirement

### Requirement: Monitor invalidation follows relevant content

The monitor SHALL avoid rebuilding inactive process/disk rows and avoid
refreshing unchanged Processes content for unrelated stats publications.
Publication revisions SHALL cover all sampled values, ownership and birth
identity and survive hydration. Graph time progression and displayed disk ages
SHALL remain independent of row ordering caches.

#### Scenario: An unrelated stats snapshot arrives

- **WHEN** process data, view inputs and viewport remain unchanged
- **THEN** Processes does not rebuild rows or request repaint
- **AND** a visible graph still advances according to its existing time and history inputs

### Requirement: Process columns have stable cell boundaries

Only the Processes table SHALL use fixed viewport-derived column boundaries.
It SHALL measure and clip the same sanitized display cells, preserving sampled
selection and signal-confirmation identity for the rendered row.

#### Scenario: Sampled ranks and Unicode names change at a fixed viewport

- **WHEN** CPU, memory, ownership labels, process ranks or Unicode names change
- **THEN** process column starts remain fixed and narrow cells clip safely
- **AND** the selected rendered row and signal target refer to the same sampled identity
