//! Spawn-ownership ratchet for `thegn-core` (see `test_support::ratchet`).
//!
//! `std::process::Child` does not wait on drop, so a discarded handle is a
//! zombie. This rule keeps the discard shapes out of the tree; its allowlist is
//! seeded **empty** because the census found no real instance, so an entry is a
//! new leak rather than inherited debt. See THE-702.

use crate::test_support::ratchet::{discards_spawned_child, file_ratchet};

const WHY: &str = "A spawned child must be owned by something that waits for it: \
     bind it and `wait()`/`wait_with_output()`, or hand it to a supervisor. \
     `std::process::Child` does NOT wait on drop, so discarding the handle leaves \
     a zombie in the process table for the life of the process — one thegn \
     instance reached 4,408 of them in three days at zero CPU cost. This list is \
     seeded empty and frozen: pinning a file here is not the fix.";

#[test]
fn spawned_children_are_owned() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "spawn-ownership-core-ratchet.txt",
        &[],
        |_, body| discards_spawned_child(body),
        WHY,
    );
}
