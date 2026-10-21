# Design

Use one Git environment-scrubbing helper for both the embedded SHA and metadata
paths. Resolve the worktree HEAD, symbolic branch ref and existing packed-refs.
For a packed branch, watch the nearest existing refs directory for creation of a
loose ref. Never register missing paths or recursively watch the object store.
Detached HEAD watches its actual HEAD. Source archives omit unavailable identity.
Build time remains the time Cargo runs this script; unchanged builds remain fresh.

Verification uses the exact production script inside a dependency-free Cargo
workspace in private Git repositories. It covers ref-only commits, branch switches,
packing/unpacking, detached HEAD, linked worktrees, outer hook variables and archives.
