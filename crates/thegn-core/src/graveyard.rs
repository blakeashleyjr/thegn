//! A bounded, TTL'd holding pen for things that have just died.
//!
//! The pane daemon drops a session the instant its PTY child exits, which loses
//! the one thing a supervisor came for: what the agent said and how it ended. A
//! `wait` or `snapshot` issued a second too late gets a bare 404 and the run is
//! unrecoverable. The fix is to keep the corpse around briefly — but "briefly"
//! has to mean *bounded*, because a retained screen plus a scrollback tail is
//! real memory and a busy fleet produces corpses continuously.
//!
//! So: **the count bound is load-bearing and the TTL is a courtesy.** Eviction at
//! `max` is what guarantees the ceiling; expiry just stops a quiet daemon from
//! holding stale screens for the rest of its life. Expiry is lazy on read and
//! batched in [`Graveyard::sweep`], so nothing here needs a timer of its own.
//!
//! Pure: the clock is injected on every call, exactly like [`crate::activity_step`].

use std::collections::{HashMap, VecDeque};

/// Bounded map of recently-dead things, oldest evicted first.
#[derive(Debug)]
pub struct Graveyard<T> {
    entries: HashMap<String, Entry<T>>,
    /// Insertion order, so eviction is O(1) and does not scan timestamps.
    order: VecDeque<String>,
    max: usize,
    ttl_ms: i64,
}

#[derive(Debug)]
struct Entry<T> {
    value: T,
    buried_at_ms: i64,
}

impl<T> Graveyard<T> {
    /// `max` entries, each readable for `ttl_ms`. A `max` of zero disables the
    /// graveyard entirely (every insert is dropped) rather than panicking.
    pub fn new(max: usize, ttl_ms: i64) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            max,
            ttl_ms,
        }
    }

    /// Bury `value` under `id`, evicting the oldest entry if that would exceed
    /// `max`. Re-burying an existing id replaces it and refreshes its position.
    pub fn insert(&mut self, id: String, value: T, now_ms: i64) {
        if self.max == 0 {
            return;
        }
        if self.entries.remove(&id).is_some() {
            self.order.retain(|k| k != &id);
        }
        while self.order.len() >= self.max {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.entries.remove(&oldest);
                }
                // Unreachable while `len() >= max >= 1`, but a `break` keeps the
                // loop total rather than trusting that invariant.
                None => break,
            }
        }
        self.order.push_back(id.clone());
        self.entries.insert(
            id,
            Entry {
                value,
                buried_at_ms: now_ms,
            },
        );
    }

    /// The entry for `id`, if it exists and has not outlived its TTL. An expired
    /// entry reads as absent without being removed — [`Self::sweep`] reclaims it.
    pub fn get(&self, id: &str, now_ms: i64) -> Option<&T> {
        let e = self.entries.get(id)?;
        (!self.is_expired(e, now_ms)).then_some(&e.value)
    }

    /// Whether `id` is present and unexpired.
    pub fn contains(&self, id: &str, now_ms: i64) -> bool {
        self.get(id, now_ms).is_some()
    }

    /// Drop every entry past its TTL.
    pub fn sweep(&mut self, now_ms: i64) {
        let ttl = self.ttl_ms;
        self.entries
            .retain(|_, e| !expired(e.buried_at_ms, ttl, now_ms));
        let live = &self.entries;
        self.order.retain(|k| live.contains_key(k));
    }

    /// Live (unexpired) entries, oldest first.
    pub fn iter(&self, now_ms: i64) -> impl Iterator<Item = (&str, &T)> {
        self.order.iter().filter_map(move |k| {
            let e = self.entries.get(k)?;
            (!self.is_expired(e, now_ms)).then_some((k.as_str(), &e.value))
        })
    }

    /// Entries held, including any that have expired but not yet been swept.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn is_expired(&self, e: &Entry<T>, now_ms: i64) -> bool {
        expired(e.buried_at_ms, self.ttl_ms, now_ms)
    }
}

/// A non-positive TTL means "never expire"; otherwise an entry dies once it is
/// strictly older than the TTL. Clock skew backwards is treated as not-expired.
fn expired(buried_at_ms: i64, ttl_ms: i64, now_ms: i64) -> bool {
    ttl_ms > 0 && now_ms.saturating_sub(buried_at_ms) > ttl_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: i64 = 10_000;

    fn g() -> Graveyard<&'static str> {
        Graveyard::new(3, TTL)
    }

    #[test]
    fn a_buried_entry_reads_back() {
        let mut g = g();
        g.insert("a".into(), "alpha", 0);
        assert_eq!(g.get("a", 0), Some(&"alpha"));
        assert!(g.contains("a", TTL));
        assert_eq!(g.len(), 1);
        assert!(!g.is_empty());
    }

    #[test]
    fn an_unknown_id_is_absent() {
        assert_eq!(g().get("nope", 0), None);
    }

    #[test]
    fn an_expired_entry_reads_as_absent_then_sweeps_away() {
        let mut g = g();
        g.insert("a".into(), "alpha", 0);
        assert_eq!(
            g.get("a", TTL),
            Some(&"alpha"),
            "exactly at the TTL is live"
        );
        assert_eq!(g.get("a", TTL + 1), None, "past it, absent");
        assert_eq!(g.len(), 1, "but not yet reclaimed");
        g.sweep(TTL + 1);
        assert_eq!(g.len(), 0);
        assert!(g.is_empty());
    }

    #[test]
    fn sweep_keeps_live_entries() {
        let mut g = g();
        g.insert("old".into(), "o", 0);
        g.insert("new".into(), "n", TTL);
        g.sweep(TTL + 1);
        assert_eq!(g.get("new", TTL + 1), Some(&"n"));
        assert_eq!(g.get("old", TTL + 1), None);
        assert_eq!(g.len(), 1);
    }

    #[test]
    fn the_count_bound_evicts_the_oldest() {
        let mut g = g(); // max 3
        for (i, id) in ["a", "b", "c", "d"].iter().enumerate() {
            g.insert((*id).into(), *id, i as i64);
        }
        assert_eq!(g.len(), 3, "the ceiling holds");
        assert_eq!(g.get("a", 0), None, "the oldest is gone");
        for id in ["b", "c", "d"] {
            assert!(g.contains(id, 0), "{id} should survive");
        }
    }

    #[test]
    fn reburying_replaces_and_refreshes_position() {
        let mut g = g();
        g.insert("a".into(), "first", 0);
        g.insert("b".into(), "b", 1);
        g.insert("a".into(), "second", 2);
        assert_eq!(g.get("a", 2), Some(&"second"));
        assert_eq!(g.len(), 2, "no duplicate row");
        // `a` is now the newest, so the next two inserts evict `b`, not `a`.
        g.insert("c".into(), "c", 3);
        g.insert("d".into(), "d", 4);
        assert!(g.contains("a", 4), "the refreshed entry survives");
        assert_eq!(g.get("b", 4), None);
    }

    #[test]
    fn iteration_is_oldest_first_and_skips_expired() {
        let mut g = Graveyard::new(4, TTL);
        g.insert("a".into(), "a", 0);
        g.insert("b".into(), "b", TTL);
        g.insert("c".into(), "c", TTL);
        let live: Vec<&str> = g.iter(TTL + 1).map(|(k, _)| k).collect();
        assert_eq!(live, vec!["b", "c"], "oldest first, `a` expired");
    }

    #[test]
    fn a_zero_max_holds_nothing() {
        let mut g: Graveyard<&str> = Graveyard::new(0, TTL);
        g.insert("a".into(), "a", 0);
        assert_eq!(g.len(), 0);
        assert_eq!(g.get("a", 0), None);
    }

    #[test]
    fn a_non_positive_ttl_never_expires() {
        let mut g: Graveyard<&str> = Graveyard::new(2, 0);
        g.insert("a".into(), "a", 0);
        assert_eq!(g.get("a", i64::MAX / 2), Some(&"a"));
        g.sweep(i64::MAX / 2);
        assert_eq!(g.len(), 1);
    }

    #[test]
    fn a_backwards_clock_does_not_expire_an_entry() {
        let mut g = g();
        g.insert("a".into(), "a", 5_000);
        assert_eq!(g.get("a", 0), Some(&"a"), "skew must not bury the living");
    }
}
