## ADDED Requirements

### Requirement: Bounded linear corner APC framing

The corner relay SHALL examine newly received bytes in amortized linear time, retain at most 4 MiB for one APC, and emit completed pieces incrementally.

#### Scenario: Bytewise APC delivery

- **WHEN** an APC arrives one byte at a time with ST split between reads
- **THEN** the relay SHALL detect completion without rescanning or copying its retained prefix

#### Scenario: Oversized APC

- **WHEN** an APC exceeds the complete-sequence byte cap
- **THEN** the relay SHALL discard through its ST and SHALL NOT expose its suffix as emulator text or a graphics command

#### Scenario: Pane replacement

- **WHEN** the relay is reset for a replacement pane
- **THEN** partial and discard state SHALL be released
