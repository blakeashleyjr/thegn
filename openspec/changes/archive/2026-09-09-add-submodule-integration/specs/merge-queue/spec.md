# Merge queue

## ADDED Requirements

### Requirement: Submodule pointer conflicts are explicit

A gitlink fold conflict SHALL be classified as a submodule pointer conflict,
name the path and both competing SHAs in operator/agent-facing output, and MUST
NOT be sent through text conflict drivers or automatic rerere resolution.

#### Scenario: Two pointer updates conflict

- **WHEN** queue folding finds different gitlinks for the same submodule path
- **THEN** the disposition reports `submodule pointer conflict` with both SHAs
  and requires an explicit resolution
