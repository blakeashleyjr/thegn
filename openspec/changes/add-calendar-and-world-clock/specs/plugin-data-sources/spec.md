# Plugin Data Sources

## ADDED Requirements

### Requirement: An external program can act as a data source

thegn SHALL be able to run a configured program and read structured data back
from it, so an integration can be written in any language without being built
into thegn. The program's output SHALL be newline-delimited JSON messages using
the existing plugin API's message framing, so one contract serves both a
run-once poll and a long-lived producer.

The query SHALL be supplied through the program's environment, so a plugin has
to _print_ JSON but never parse it. This is what makes a shell script a viable
plugin, and it mirrors how thegn already passes context to hook commands.

#### Scenario: A single-line shell plugin

- **WHEN** a program prints one JSON message and exits zero
- **THEN** its data is accepted

#### Scenario: The query reaches the program

- **WHEN** thegn runs a data-source program
- **THEN** the requested window, the last cursor, the user's timezone, and the
  API version are present in the program's environment

### Requirement: A plugin declares what it needs and is granted only that

A plugin MAY declare a manifest naming its identity, the API version it speaks,
the extension point it contributes, and the capabilities it wants. thegn SHALL
negotiate that manifest against what the configuration grants it, MUST accept
only extension points valid for the surface it was configured on, and MUST
record every denied capability.

A denied capability MUST NOT abort the run — a plugin that asks for more than it
was given should still deliver what it can. An incompatible API version MUST be
refused, since neither side can interpret the other.

A plugin that declares no manifest MUST be treated as requesting nothing.

#### Scenario: A capability that was not granted

- **WHEN** a plugin's manifest requests a capability the configuration does not
  grant
- **THEN** the denial is recorded and the plugin's data is still accepted

#### Scenario: An incompatible API version

- **WHEN** a plugin declares an API version thegn cannot speak
- **THEN** the run is rejected with an error naming both versions

#### Scenario: A contribution to an unrelated surface

- **WHEN** a plugin configured as a calendar source declares a contribution to a
  different extension point
- **THEN** that contribution is ignored and recorded

### Requirement: A misbehaving plugin cannot harm the host

Running a plugin MUST be bounded in time, memory and output. Exceeding the
output limit MUST truncate and report truncation rather than terminating the
program mid-write. A plugin that exceeds its time limit MUST have its entire
process group terminated, not just the process thegn started, so a program that
spawns children cannot outlive the run.

The plugin's error output MUST be captured and surfaced when it fails, rather
than discarded. Output that is not valid JSON MUST be reported as such rather
than silently dropped, since a stray debugging line is the most common mistake.

Inherited version-control environment variables MUST be removed, so a plugin
that shells out does not operate on whatever repository thegn happened to be
viewing.

Plugin runs MUST NOT be able to exhaust the background work lane.

#### Scenario: A plugin that produces unbounded output

- **WHEN** a plugin writes more messages than the limit allows
- **THEN** the accepted messages are kept, truncation is reported, and the
  plugin still exits normally rather than being killed by a broken pipe

#### Scenario: A plugin that hangs

- **WHEN** a plugin exceeds its timeout
- **THEN** its process group is terminated and the failure is treated as
  retryable, leaving any cached data intact

#### Scenario: A plugin that fails

- **WHEN** a plugin exits non-zero
- **THEN** the error names the exit status and includes its error output

#### Scenario: A stray non-JSON line

- **WHEN** a plugin writes a line that is not JSON to its output
- **THEN** the line is reported as unexpected output and the valid messages are
  still accepted

### Requirement: The plugin data format is forward and backward compatible

The data a plugin emits SHALL require only the fields without which the item is
meaningless, defaulting everything else. Unrecognised fields MUST be ignored, so
a plugin written for a newer thegn still works against an older one, and a
plugin written for an older thegn keeps working as fields are added.

#### Scenario: A minimal item

- **WHEN** a plugin emits an item with only its required fields
- **THEN** it is accepted and the remaining fields take their defaults

#### Scenario: An unrecognised field

- **WHEN** a plugin emits an item containing a field thegn does not know
- **THEN** the field is ignored and the item is accepted

### Requirement: The control plane exposes calendar data to authorized clients

The control API SHALL let an authorized client read the merged calendar over a
date window and the resolved world clocks, and SHALL let a client push events
into a named source's cache. Reading MUST require the read scope and pushing the
write scope. Pushing MUST be ingest into thegn's own cache for that source, not
a write to any upstream provider.

#### Scenario: Reading the calendar

- **WHEN** a client with the read scope requests events for a date range
- **THEN** the merged, expanded events for that range are returned

#### Scenario: Pushing without the write scope

- **WHEN** a client with only the read scope attempts to push events
- **THEN** the request is refused and nothing is stored
