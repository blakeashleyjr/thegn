# Terminal search off the event loop

## Why

The history overlay re-scored every line in scope on the event loop on each
keystroke, up to about 400,000 lines at workspace scope, so typing and
redrawing stalled behind the scan (THE-2244).

## What Changes

- Matching runs on an overlay-owned worker over an owned, cheaply shared
  snapshot of the in-scope histories (history lines are `Arc<str>`).
- Each request carries the generation stamped under the submit lock; results of
  a superseded request are never shown.
- Ranked results arrive in bounded batches behind a `searching…` status.
- Enter pressed before the first batch jumps to the best hit once it arrives.
- A worker that cannot start or dies surfaces an error and the overlay scores
  on the loop from then on instead of respawning per keystroke.

## Impact

Terminal search overlay, `HistoryBuffer` storage, help page `search`.
