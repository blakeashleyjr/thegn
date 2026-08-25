# Merge Queue

## ADDED Requirements

### Requirement: Submodule pointer conflicts are named, never auto-resolved

When a fold conflicts on a gitlink entry, the drain outcome and the
agent-handoff prompt SHALL name it as a submodule pointer conflict with the
path and both candidate commits, and thegn MUST NOT auto-resolve it — pointer
conflicts are excluded from any merge-driver or rerere routing, deferring to
the agent prompt or the human.

#### Scenario: Both sides moved the pointer

- **WHEN** a queued branch and the target both moved the same submodule
  pointer
- **THEN** the branch defers with `submodule pointer conflict: <path>
(<ours> vs <theirs>)` recorded, and no automatic resolution is attempted
