# THE-289 plan

Scope: crates/thegn-media only. New `helper.rs` bounded subprocess primitive
(deadline, per-stream byte cap, own process group, kill before reap, bounded reap,
Guard drop kills group; never killpg after wait). Route playerctl (probe now async),
osascript, mediaremote `get` through it. MediaRemote stream: own pgid, kill_on_drop,
group kill in Drop, bounded record length, cancel-safe record buffer; no read-idle
timer by design (0% idle). Unix pgroup/killpg helpers live in platform/mod.rs (already pinned).
Tests: src/helper/tests.rs (never-exit, infinite stdout/stderr, grandchild, cancel, repeated reload).
Out of scope: Windows Job Objects (none exist in this crate; direct-child kill only),
host media_watch time budget, zbus paths.
