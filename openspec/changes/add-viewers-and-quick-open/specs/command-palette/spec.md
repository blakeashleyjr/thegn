# Command Palette

## ADDED Requirements

### Requirement: Quick-Open two-pass ranking

Fuzzy file open SHALL rank candidates in two passes so that tracked files appear
in a first pass and gitignored or untracked files surface in a second pass after
them, ensuring a tracked file outranks a gitignored file at an equal fuzzy score
while still keeping gitignored files reachable; this ranking uses the existing
fuzzy matcher and depends on no AI/agent layer.

#### Scenario: Tracked files rank before gitignored at equal score

- **WHEN** a tracked file and a gitignored file have the same fuzzy score for the
  query
- **THEN** the tracked file is listed before the gitignored file

#### Scenario: Gitignored files surface in the second pass

- **WHEN** the query matches only gitignored or untracked files
- **THEN** those files are still listed, appearing in the second pass

#### Scenario: No gitignored matches

- **WHEN** the query matches only tracked files
- **THEN** the results contain just the first-pass tracked files with no empty
  second segment shown
