//! Counter delta helpers with reset and sampling-interval handling.

#[derive(Debug, Clone, Default)]
pub struct Counter {
    prev: Option<(u64, u64)>,
}
impl Counter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn rate(&mut self, cur: u64, now_ms: u64) -> Option<f64> {
        let previous = self.prev;
        if let Some((old, then_ms)) = previous {
            if now_ms < then_ms || cur < old {
                self.prev = Some((cur, now_ms));
                return None;
            }
            let dt = now_ms - then_ms;
            if dt < 100 {
                return None;
            }
            self.prev = Some((cur, now_ms));
            return Some((cur - old) as f64 * 1000.0 / dt as f64);
        }
        self.prev = Some((cur, now_ms));
        None
    }
}

#[derive(Debug, Clone, Default)]
pub struct Counter32 {
    prev: Option<(u32, u64)>,
}
impl Counter32 {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn rate(&mut self, cur: u32, now_ms: u64) -> Option<f64> {
        let Some((old, then_ms)) = self.prev else {
            self.prev = Some((cur, now_ms));
            return None;
        };
        if now_ms < then_ms {
            self.prev = Some((cur, now_ms));
            return None;
        }
        let delta = if cur < old && old > 0xC000_0000 {
            cur.wrapping_sub(old)
        } else if cur < old {
            self.prev = Some((cur, now_ms));
            return None;
        } else {
            cur - old
        };
        let dt = now_ms - then_ms;
        if dt < 100 {
            return None;
        }
        self.prev = Some((cur, now_ms));
        Some(f64::from(delta) * 1000.0 / dt as f64)
    }
}

pub fn rate(current: u64, previous: u64, previous_ms: Option<u64>, now_ms: u64) -> Option<f64> {
    let dt = now_ms.checked_sub(previous_ms?)?;
    if current < previous || dt < 100 {
        return None;
    }
    Some((current - previous) as f64 * 1000.0 / dt as f64)
}

pub fn jiffies_pct(delta: u64, dt_ms: u64, clk_tck: u64) -> Option<f64> {
    if dt_ms == 0 || clk_tck == 0 {
        None
    } else {
        Some(delta as f64 * 100_000.0 / (dt_ms as f64 * clk_tck as f64))
    }
}

#[cfg(test)]
mod tests {
    use super::{Counter, Counter32, jiffies_pct};
    #[test]
    fn counter_first_reset_and_backwards_time_reprime() {
        let mut counter = Counter::new();
        assert_eq!(counter.rate(10, 1000), None);
        assert_eq!(counter.rate(20, 900), None);
        assert_eq!(counter.rate(30, 1900), Some(10.0));
        assert_eq!(counter.rate(2, 2000), None);
        assert_eq!(counter.rate(12, 3000), Some(10.0));
    }
    #[test]
    fn short_interval_does_not_replace_previous_sample() {
        let mut counter = Counter::new();
        assert_eq!(counter.rate(10, 1000), None);
        assert_eq!(counter.rate(20, 1099), None);
        assert_eq!(counter.rate(30, 2000), Some(20.0));
    }
    #[test]
    fn counter32_detects_wrap_and_jiffies_handles_invalid_denominators() {
        let mut counter = Counter32::new();
        assert_eq!(counter.rate(0xFFFF_FFF0, 1000), None);
        assert_eq!(counter.rate(0x10, 2000), Some(32.0));
        assert_eq!(jiffies_pct(100, 1000, 100), Some(100.0));
        assert_eq!(jiffies_pct(1, 0, 100), None);
        assert_eq!(jiffies_pct(1, 1, 0), None);
    }

    #[test]
    fn direct_rate_rejects_resets_backwards_time_and_short_intervals() {
        assert_eq!(super::rate(20, 10, Some(1000), 2000), Some(10.0));
        assert_eq!(super::rate(9, 10, Some(1000), 2000), None);
        assert_eq!(super::rate(20, 10, Some(2000), 1000), None);
        assert_eq!(super::rate(20, 10, Some(1000), 1099), None);
    }
}
