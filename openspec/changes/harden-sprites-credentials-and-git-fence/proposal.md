## Why

Sprites sandboxes forwarded model credentials with no way to withhold them, destroy was
treated as done on an accepted DELETE, provider exec had no overall deadline, close
forgot a worktree before resolving it, and search defaulted to fuzzy matching.

## What Changes

- `model_credentials` accepts `none` or `upload` for Sprites (THE-801).
- An unset Sprites `model_credentials` withholds until the project answers a
  one-time forward/not-now/never question; headless launches withhold with a hint (THE-2448).
- Sprites destroy polls for a definite 404 before forgetting the record.
- `run_exec` gets a 31 minute overall deadline (THE-533).
- Close snapshots location, repo root and env before the worktree is forgotten.
- Search is literal smart-case by default; fuzzy is `Alt+f`.

The git hook fence for thegn's own Git runs (THE-225 slice) is held and not part of this change.
