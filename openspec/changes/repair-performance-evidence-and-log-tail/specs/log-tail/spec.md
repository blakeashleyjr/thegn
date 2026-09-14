## ADDED Requirements

### Requirement: Live log following recovers across file generations

The log follower SHALL skip pre-existing history on initial attachment, read a
replacement file from its start, and restart at zero on detected truncation.
Partial records MUST NOT combine across generations. Missing paths SHALL be
retried, and retired-file draining SHALL be bounded so replacement progress is
not starved by a writer that continues appending to the retired file.

#### Scenario: Rename and recreate rotation

- **WHEN** the active path is replaced while the reader owns the old descriptor
- **THEN** the reader drains a bounded amount of old content and follows the new
  file without replaying already delivered complete records

#### Scenario: A record is written in parts

- **WHEN** a record is incomplete at EOF and the same file later completes it
- **THEN** the completed record is emitted once with its original prefix

#### Scenario: A file disappears and returns

- **WHEN** the active path is temporarily absent
- **THEN** the follower remains available and resumes when it can open the path
