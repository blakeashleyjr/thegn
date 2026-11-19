//! Bounded retention for state files that only grow (pty-shutdown notes,
//! stale database copies). Runs only inside the database maintenance pass
//! (`db_maintain`), on the disk worker thread, never on the event loop.

/// What one pass removed, for the maintenance debug line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GcReport {
    pub pty_notes: usize,
    pub db_copies: usize,
}

/// Most files a single pass may delete, across every store.
pub(crate) const MAX_UNLINKS_PER_PASS: usize = 256;

/// One bounded pass over the thegn state directory. `now` is Unix seconds.
pub(crate) fn run_pass(now: i64) -> GcReport {
    let _now = now; // stub: lane 4
    GcReport::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_pass_removes_nothing() {
        assert_eq!(run_pass(0), GcReport::default());
    }
}
