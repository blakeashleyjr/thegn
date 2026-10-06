# THE-443 plan

Scope: crates/thegn-host/src/profile.rs, docs/help/debugging.md.
Approach: poll() only forwards a coalesced request over a bounded channel to a Background-QoS worker thread
that owns a Machine (Idle/Starting/Running/Publishing, generation, no mutex/poison). Reports published via
0700 dir check, O_EXCL|O_NOFOLLOW 0600 temp + hard_link (never overwrites), names include pid+generation.
sigaction result propagated. Backend trait seam; unit tests with a fake backend (unix, no pprof needed).
Out of scope: waker pulse (no UI result to show), config keys.
