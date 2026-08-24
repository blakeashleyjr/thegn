# Managed Tools

## ADDED Requirements

### Requirement: difftastic is a known managed tool

thegn SHALL register `difft` (difftastic) as a known managed tool sourced
from its GitHub releases at a pinned version, with `difft` on PATH as a
fallback, resolving through the standard three-tier order (config override →
PATH → managed). Its probe SHALL appear in `thegn doctor`, and the
structural-diff feature MUST degrade to the internal viewer when resolution
fails rather than erroring.

#### Scenario: PATH install wins over acquisition

- **WHEN** the user already has `difft` on PATH and `structural_diff` is
  active
- **THEN** the PATH binary is used and no download occurs

#### Scenario: Unresolvable tool degrades the feature

- **WHEN** `difft` resolves through no tier
- **THEN** doctor reports the tool unavailable and diff surfaces use the
  internal viewer
