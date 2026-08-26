//! What a session leaves behind when its child exits.
//!
//! The daemon used to drop a session the instant the PTY child died, and that
//! lost the one thing a supervisor came for. The sequence is unforgiving: the
//! actor sends `SessionExit`, a waiting `wait` wakes, re-queries the session
//! table — and the entry is already gone, so it gets `404` instead of the exit
//! code. `snapshot` fares worse: the emulator and scrollback are dropped with
//! the actor, so a supervisor that polled a second late never learns what the
//! agent said. That is not a race you can win by polling faster; it is a race
//! you win by keeping the corpse.
//!
//! So the actor buries one of these before it signals anything, and the ordering
//! is the whole fix — see the teardown in [`super::session`]. At no instant is a
//! session id absent from *both* the live table and the graveyard, so no
//! observer can look between them and see nothing.
//!
//! # Bounds
//!
//! A retained screen plus a scrollback tail is real memory, and a busy fleet
//! produces corpses continuously, so both bounds matter and they do different
//! jobs. [`MAX_TOMBSTONES`] is the ceiling that actually holds — the graveyard
//! evicts oldest-first, so memory is bounded whatever the workload.
//! [`TOMBSTONE_TTL_MS`] is the courtesy that stops a quiet daemon from holding
//! stale screens for the rest of its life.
//!
//! Worst case is roughly `MAX_TOMBSTONES` × (one screen + `TOMBSTONE_HISTORY_LINES`
//! of text) — on the order of a few MB. If you raise either constant, that is
//! the number you are trading.

use thegn_core::attention::PaneAgentState;
use thegn_core::control_wire::EventFrame;
use thegn_svc::control::SessionInfo;

use super::session::{LiveMeta, SessionMeta};

/// How many dead sessions to keep. The load-bearing bound.
pub(crate) const MAX_TOMBSTONES: usize = 32;

/// How long a dead session stays readable. Long enough for a supervisor that
/// checks in every few minutes; short enough that the memory is transient.
pub(crate) const TOMBSTONE_TTL_MS: i64 = 10 * 60 * 1000;

/// Scrollback lines retained per corpse. A quarter of the live snapshot budget:
/// enough to read an agent's last words and a stack trace, not a full session.
pub(crate) const TOMBSTONE_HISTORY_LINES: usize = 500;

/// A session that has exited, kept briefly so its result is still readable.
#[derive(Debug, Clone)]
pub(crate) struct Tombstone {
    /// The identity it had in life, so listings and lookups still resolve.
    pub meta: SessionMeta,
    /// The child's exit code, or `None` when it could not be reaped (a killed
    /// session, or a reader thread that died before `wait`).
    pub exit_code: Option<i32>,
    pub exited_at_ms: i64,
    /// The final screen as a `PaneSnapshot` frame — served verbatim to a
    /// `snapshot` call, so a late reader sees exactly what a live one would.
    pub final_screen: EventFrame,
    /// ANSI-stripped scrollback tail, oldest first. What `wait --until
    /// match:<regex>` scans when the pattern it wanted already scrolled by.
    pub history_tail: Vec<String>,
    /// The agent state at the moment of death, for a fleet listing that wants
    /// to say "finished" rather than merely "gone".
    pub last_state: PaneAgentState,
    /// Geometry at death, so a listing row is not blank.
    pub rows: u16,
    pub cols: u16,
    /// Path of the session's finalized asciicast recording, if it was recorded,
    /// so `session list` can point at the `.cast` for a short while after death.
    pub recording: Option<std::path::PathBuf>,
}

impl Tombstone {
    /// The listing row this session would have had, with no attached clients
    /// and no lease — both are meaningless for a corpse — plus the three
    /// fields that mark it finished, so a roster can say "done, exit 0" rather
    /// than silently dropping the row.
    pub(crate) fn info(&self) -> SessionInfo {
        let mut info = self.meta.info(
            &LiveMeta {
                rows: self.rows,
                cols: self.cols,
                attached: 0,
                recording: None,
            },
            None,
        );
        info.exited_at_ms = Some(self.exited_at_ms);
        info.exit_code = self.exit_code;
        info.final_state = Some(super::session::state_str(self.last_state).to_string());
        // A corpse isn't actively recording, but the finalized `.cast` path is
        // still worth reporting so a reader can find the recording.
        info.recording = self
            .recording
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        info
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use thegn_core::graveyard::Graveyard;

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            id: id.to_string(),
            worktree: Some("/wt/a".to_string()),
            program: "claude".to_string(),
            cwd: Some("/wt/a".to_string()),
            created_at_ms: 0,
            pid: Some(4242),
        }
    }

    pub(crate) fn tomb(id: &str, code: Option<i32>) -> Tombstone {
        Tombstone {
            meta: meta(id),
            exit_code: code,
            exited_at_ms: 0,
            final_screen: EventFrame::PaneSnapshot {
                session: id.to_string(),
                seq: 7,
                cols: 80,
                rows: 24,
                bytes: b"final".to_vec(),
            },
            history_tail: vec!["one".into(), "two".into()],
            last_state: PaneAgentState::Done,
            rows: 24,
            cols: 80,
            recording: None,
        }
    }

    #[test]
    fn a_tombstone_still_renders_a_listing_row() {
        let t = tomb("abc", Some(0));
        let info = t.info();
        assert_eq!(info.id, "abc");
        assert_eq!(info.program, "claude");
        assert_eq!(info.worktree.as_deref(), Some("/wt/a"));
        assert_eq!((info.rows, info.cols), (24, 80));
        assert_eq!(info.attached_clients, 0, "a corpse has no clients");
        assert_eq!(info.lease_expires_at, None, "and holds no lease");
        assert_eq!(info.pid, Some(4242));
        // The three fields that let a roster tell a corpse from a live row.
        assert_eq!(info.exited_at_ms, Some(0), "the row is marked finished");
        assert_eq!(info.exit_code, Some(0));
        assert_eq!(info.final_state.as_deref(), Some("done"));
    }

    /// A killed session that could not be reaped still lists, with no code —
    /// "finished, outcome unknown" must be representable, because a supervisor
    /// that sees no row at all would re-dispatch the work.
    #[test]
    fn an_unreapable_corpse_still_lists_as_finished() {
        let info = tomb("abc", None).info();
        assert_eq!(info.exited_at_ms, Some(0), "still marked finished");
        assert_eq!(info.exit_code, None);
    }

    /// The bounds are the contract; a change to either is a change to the
    /// daemon's memory ceiling and should be deliberate.
    #[test]
    fn the_graveyard_holds_the_documented_ceiling() {
        let mut g: Graveyard<Tombstone> = Graveyard::new(MAX_TOMBSTONES, TOMBSTONE_TTL_MS);
        for i in 0..MAX_TOMBSTONES + 8 {
            g.insert(format!("s{i}"), tomb(&format!("s{i}"), Some(0)), 0);
        }
        assert_eq!(g.len(), MAX_TOMBSTONES);
        assert!(g.get("s0", 0).is_none(), "the oldest were evicted");
        assert!(g.get(&format!("s{}", MAX_TOMBSTONES + 7), 0).is_some());
    }

    #[test]
    fn a_tombstone_expires_after_the_ttl() {
        let mut g: Graveyard<Tombstone> = Graveyard::new(MAX_TOMBSTONES, TOMBSTONE_TTL_MS);
        g.insert("a".into(), tomb("a", Some(2)), 0);
        assert!(g.get("a", TOMBSTONE_TTL_MS).is_some());
        assert!(g.get("a", TOMBSTONE_TTL_MS + 1).is_none());
    }
}
