## ADDED Requirements

### Requirement: Overview landing tab

The system monitor SHALL place Overview first, assign it key 1 and use it as the
default tab for new users. Existing saved tab slugs SHALL continue to restore
the corresponding tab.

#### Scenario: Open the monitor with default preferences

- **WHEN** a user opens the system monitor with no saved last tab
- **THEN** Overview is selected
- **AND** the first tab is reachable with key 1

### Requirement: Overview source-aware panels

Overview SHALL show summary panels for CPU, load and pressure, memory,
network, disk, thermal, power when present, and process/container counts.
Panels without an available source SHALL be omitted and SHALL NOT add their
collectors to sampler demand. Missing readings SHALL use the terminal absent
mark rather than a numeric zero.

#### Scenario: Power source is absent

- **WHEN** no power supply reading is present
- **THEN** the power panel is not placed
- **AND** the power-supply collector is not demanded by Overview

### Requirement: Frozen Overview frame

The monitor SHALL copy a sampled frame into overlay-owned Overview state before
the sampler recycles the frame. A paused overlay SHALL ignore incoming frames.
