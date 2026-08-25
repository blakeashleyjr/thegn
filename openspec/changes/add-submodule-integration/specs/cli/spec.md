# CLI

## ADDED Requirements

### Requirement: `wt new` initializes submodules through the shared pipeline

`wt new` SHALL initialize submodules exactly as the TUI wizard does (one
shared core pipeline, honoring `[git] submodules` and the trust gate), and
because headless creation prints the worktree path as its only plain output,
initialization progress and any failure notice MUST go to stderr while the
path contract on stdout is preserved. A submodule-initialization failure
MUST NOT change the exit code of an otherwise successful creation.

#### Scenario: Headless creation stays scriptable

- **WHEN** `thegn wt new fix-parser --repo <root>` runs in a submodule repo
- **THEN** stdout is exactly the new worktree path, submodules initialize
  (or their failure is reported on stderr), and the exit code reflects only
  the worktree creation
