# THE-55 Architect Revision 2

Status: REVISE

## Required correction

### 1. Make the source-compositor guard effective for the default profile

`crates/thegn-host/src/cmd/session_move.rs:125-131` relies on
`profile::instance_running()` to prevent a compositor from persisting the
source rows while the move deletes them. That check is ineffective for the
default profile: `crates/thegn-core/src/profile.rs:361` deliberately treats
default-profile lock contention as `Singleton::Acquired`, and the interactive
startup path therefore holds no lock (`crates/thegn-host/src/main.rs:908-923`).
Consequently, a default-profile compositor can remain open while
`thegn session move` reads and deletes its worktree/session rows, violating
the design's source-concurrency guard and the OpenSpec scenario “A source
compositor cannot race cleanup.”

Expected fix: provide a reliable source-instance check that covers the
default profile as well as named profiles, while preserving the existing
profile-wide singleton behavior and avoiding a second competing lock. Add a
deterministic test that proves a default-profile interactive owner blocks a
move before either migration database is opened; also retain the named-profile
case. The guard must fail closed on an owner/lock state that cannot be safely
disproved.
