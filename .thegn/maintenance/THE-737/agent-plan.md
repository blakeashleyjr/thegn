# THE-737 plan

Scope: GixGit::is_dirty (the only in-process gix read on the glyph path; ahead_behind, branch diffs use bounded CLI) gets a cooperative deadline = git_read_timeout().
Approach: with_read_deadline parks a watchdog on a channel (no polling), raises gix status should_interrupt flag; interrupted iterator yields None, so expiry maps to Err (row keeps last-known) and the pool slot is released (gix Iter drop detaches producers).
Tests: deadline helper raises/doesn't raise; expired probe is error. Out of scope: per-path coalescing, bridged remote reads.
