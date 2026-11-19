## Decisions

- One maintainer: `locks/db-maintain.lock` taken with `File::try_lock`; the daemon never maintains; `thegn db vacuum` takes the same lock and waits at most 30 s.
- Cadence: a pass at most every 10 minutes, the first at least 120 s after the first frame, on the existing disk worker thread.
- A pass runs bounded retention, the state-file sweep, then one of: convert a legacy database of at most 256 MiB live (free-space check first, 3 attempts per process); toast for a larger one; free 2,048 pages per step (16 steps, 250 ms stop) when an incremental database has at least `max(8 MiB, 10%)` free.
- No soak-harness change: a soak window is shorter than one LOC sweep, so the pure planner simulation is the regression gate.
