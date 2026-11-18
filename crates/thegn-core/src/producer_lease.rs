//! Producer leases (schema v114, THE-2208; v116 added `inbox`, THE-2214): at
//! most one headless producer per kind polls the network at a time.
//!
//! The daemon's producer owner acquires or renews a kind's lease every tick
//! (holder [`DAEMON_HOLDER`], TTL [`ttl_ms`]). A UI producer reads the lease
//! and, while the daemon holds it live, reloads from the DB cache instead of
//! fetching. If the daemon dies the lease lapses after the TTL and the UI
//! resumes polling. Persistence lives in `db_producer_lease.rs`.

/// One headless producer kind. The stored string ([`ProducerKind::as_str`])
/// is the `producer_leases.producer` value. Since v116 the column's CHECK
/// bounds only its length (1..=32); [`ProducerKind::parse`] is what gates
/// rows, so a name this build does not know is skipped on read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProducerKind {
    /// The tracker poll (issue events, autopilot pickup).
    Tracker,
    /// The PR poll (checks, review requests, landings).
    PrPoll,
    /// The forge mentions poll.
    Mentions,
    /// The PR queue drive.
    PrQueue,
    /// The native inbox poll (THE-2214).
    Inbox,
}

impl ProducerKind {
    /// Every kind, in a stable order.
    pub const ALL: [ProducerKind; 5] = [
        ProducerKind::Tracker,
        ProducerKind::PrPoll,
        ProducerKind::Mentions,
        ProducerKind::PrQueue,
        ProducerKind::Inbox,
    ];

    /// The stored name (the `producer_leases.producer` value).
    pub fn as_str(self) -> &'static str {
        match self {
            ProducerKind::Tracker => "tracker",
            ProducerKind::PrPoll => "pr_poll",
            ProducerKind::Mentions => "mentions",
            ProducerKind::PrQueue => "pr_queue",
            ProducerKind::Inbox => "inbox",
        }
    }

    /// Inverse of [`ProducerKind::as_str`]; `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// The holder name the daemon's producer owner writes.
pub const DAEMON_HOLDER: &str = "daemon";

/// The shortest lease, whatever the cadence.
pub const MIN_TTL_MS: i64 = 60_000;

/// One `producer_leases` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRow {
    pub kind: ProducerKind,
    pub holder: String,
    pub holder_pid: i64,
    pub acquired_at_ms: i64,
    pub expires_at_ms: i64,
}

/// Lease length for a producer ticking every `cadence_secs`: three missed
/// ticks, never below [`MIN_TTL_MS`]. Saturates instead of overflowing.
pub fn ttl_ms(cadence_secs: u64) -> i64 {
    let ms = cadence_secs.saturating_mul(3).saturating_mul(1000);
    i64::try_from(ms).unwrap_or(i64::MAX).max(MIN_TTL_MS)
}

/// True while the lease has not expired at `now_ms`.
pub fn live(row: &LeaseRow, now_ms: i64) -> bool {
    row.expires_at_ms > now_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trips_through_its_stored_name() {
        for kind in ProducerKind::ALL {
            assert_eq!(ProducerKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ProducerKind::parse("Tracker"), None);
        assert_eq!(ProducerKind::parse(""), None);
        let names: Vec<_> = ProducerKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            names,
            ["tracker", "pr_poll", "mentions", "pr_queue", "inbox"]
        );
        assert!(
            names.iter().all(|n| (1..=32).contains(&n.len())),
            "fits the v116 CHECK"
        );
    }

    #[test]
    fn ttl_has_a_floor_and_saturates() {
        assert_eq!(ttl_ms(0), MIN_TTL_MS);
        assert_eq!(ttl_ms(10), MIN_TTL_MS, "30 s is below the floor");
        assert_eq!(ttl_ms(20), MIN_TTL_MS);
        assert_eq!(ttl_ms(21), 63_000);
        assert_eq!(ttl_ms(300), 900_000);
        assert_eq!(ttl_ms(u64::MAX), i64::MAX);
        assert_eq!(ttl_ms(u64::MAX / 3), i64::MAX);
    }

    #[test]
    fn live_is_strictly_before_expiry() {
        let row = LeaseRow {
            kind: ProducerKind::Tracker,
            holder: DAEMON_HOLDER.into(),
            holder_pid: 1,
            acquired_at_ms: 0,
            expires_at_ms: 100,
        };
        assert!(live(&row, 99));
        assert!(!live(&row, 100));
        assert!(!live(&row, 101));
    }
}
