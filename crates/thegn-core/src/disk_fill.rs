//! Days-to-full projection over recorded free-space history.
//!
//! Pure arithmetic: a least-squares line through `(seconds, free_bytes)`
//! samples, extrapolated to zero free bytes. It lives in core (not the host)
//! so it falls under the 95%-lines coverage gate and can be exercised with
//! fixtures rather than a live filesystem — a *wrong* projection is worse than
//! none, so the honesty gates below are the load-bearing logic.
//!
//! The projection is deliberately conservative. It returns `None` — "no honest
//! answer" — unless every gate passes:
//!
//! 1. **Enough samples** ([`MIN_SAMPLES`]) — two points can fit any line.
//! 2. **Enough span** ([`MIN_SPAN_SECS`]) — a slope over a few seconds of
//!    jitter says nothing about days.
//! 3. **A real decline** — the fitted slope must be negative *and* account for
//!    more than [`MIN_TREND_FRACTION`] of the current free space across the
//!    window, so sampling noise on a stable disk doesn't manufacture a trend.
//! 4. **Room left** — current free bytes must be positive.
//!
//! A flat or growing disk therefore reports no ETA rather than a reassuring
//! (and meaningless) large number, and the `disk_eta` alert stays inert.

/// Minimum finite samples before a projection is attempted.
const MIN_SAMPLES: usize = 8;

/// Minimum wall-clock span (seconds) the samples must cover. Ten minutes: short
/// enough that a fast fill is caught, long enough that a slope is a trend and
/// not a scheduler blip.
const MIN_SPAN_SECS: f64 = 600.0;

/// The fitted decline across the observed window must consume at least this
/// fraction of current free space, or the "trend" is treated as noise. A genuine
/// days-to-full fill clears this easily (a two-day fill consumes ~2% of free per
/// hour); a jittery stable disk does not.
const MIN_TREND_FRACTION: f64 = 0.005;

/// A downward free-space trend extrapolated to zero free bytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiskFillEta {
    /// Hours until free space reaches zero at the fitted rate.
    pub hours: f64,
    /// Fitted fill rate in bytes/second (positive — bytes consumed per second).
    pub bytes_per_sec: f64,
}

impl DiskFillEta {
    /// Hours until full, as an `f32` for the alert reading.
    pub fn hours_f32(&self) -> f32 {
        self.hours as f32
    }
}

/// Project time-to-full from `(seconds, free_bytes)` samples.
///
/// `points` need not be sorted or gap-free; non-finite samples are ignored. The
/// x axis is any consistent seconds base (its origin is irrelevant — only spans
/// and slopes are used). Returns `None` unless all honesty gates pass; see the
/// module docs.
pub fn project(points: &[(f64, f64)]) -> Option<DiskFillEta> {
    let pts: Vec<(f64, f64)> = points
        .iter()
        .copied()
        .filter(|(x, y)| x.is_finite() && y.is_finite())
        .collect();
    if pts.len() < MIN_SAMPLES {
        return None;
    }

    let (mut min_x, mut max_x) = (f64::INFINITY, f64::NEG_INFINITY);
    for &(x, _) in &pts {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
    }
    let span = max_x - min_x;
    if span < MIN_SPAN_SECS {
        return None;
    }

    let n = pts.len() as f64;
    let mean_x = pts.iter().map(|(x, _)| *x).sum::<f64>() / n;
    let mean_y = pts.iter().map(|(_, y)| *y).sum::<f64>() / n;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for &(x, y) in &pts {
        let dx = x - mean_x;
        sxx += dx * dx;
        sxy += dx * (y - mean_y);
    }
    if sxx <= 0.0 {
        return None;
    }
    // Bytes/second: negative while filling. `slope` is finite (finite inputs,
    // `sxx > 0`), so a plain `>= 0` cleanly rejects flat and growing disks.
    let slope = sxy / sxx;
    if slope >= 0.0 {
        return None;
    }
    let bytes_per_sec = -slope;

    // "Free now" is the most recent observed sample, not the fitted intercept:
    // more truthful for how much runway is actually left, and robust to a fit
    // that a burst dragged off the latest point.
    let free_now = pts
        .iter()
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, y)| *y)
        .unwrap_or(0.0);
    if free_now <= 0.0 {
        return None;
    }

    // Gate 3: the modeled decline across the window must be a real fraction of
    // free space, not noise. `bytes_per_sec * span` is how much the fit says was
    // consumed over the observed window.
    if bytes_per_sec * span < MIN_TREND_FRACTION * free_now {
        return None;
    }

    let hours = free_now / bytes_per_sec / 3600.0;
    Some(DiskFillEta {
        hours,
        bytes_per_sec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A steady fill: 8 GiB free, losing 1 MiB/s, sampled once a second for 20
    /// minutes. Free stays positive across the window; the projection is the
    /// remaining free over the rate.
    #[test]
    fn a_steady_fill_projects_time_to_zero() {
        let free0 = 8.0 * 1024.0 * 1024.0 * 1024.0;
        let rate = 1024.0 * 1024.0; // 1 MiB/s
        let pts: Vec<(f64, f64)> = (0..=1200)
            .map(|s| (s as f64, free0 - rate * s as f64))
            .collect();
        let eta = project(&pts).expect("steady fill projects");
        assert!(
            (eta.bytes_per_sec - rate).abs() < 1.0,
            "{}",
            eta.bytes_per_sec
        );
        // free_now after 1200s; hours = that / rate / 3600.
        let free_now = free0 - rate * 1200.0;
        let want_h = free_now / rate / 3600.0;
        assert!(
            (eta.hours - want_h).abs() < 0.01,
            "{} vs {}",
            eta.hours,
            want_h
        );
    }

    #[test]
    fn a_flat_disk_has_no_eta() {
        let pts: Vec<(f64, f64)> = (0..=1200).map(|s| (s as f64, 5.0e11)).collect();
        assert!(project(&pts).is_none());
    }

    #[test]
    fn a_growing_disk_has_no_eta() {
        // Free space rising (a clean freed the target dirs): no fill projection.
        let pts: Vec<(f64, f64)> = (0..=1200)
            .map(|s| (s as f64, 1.0e11 + 1.0e6 * s as f64))
            .collect();
        assert!(project(&pts).is_none());
    }

    #[test]
    fn thin_history_is_declined() {
        // Only a few samples over a short span: no honest slope.
        let pts = [(0.0, 100.0), (1.0, 99.0), (2.0, 98.0), (3.0, 97.0)];
        assert!(project(&pts).is_none());
        // Enough samples but under the span floor.
        let short: Vec<(f64, f64)> = (0..=20)
            .map(|s| (s as f64, 1.0e9 - 1.0e6 * s as f64))
            .collect();
        assert!(project(&short).is_none());
    }

    #[test]
    fn pure_noise_does_not_manufacture_a_trend() {
        // A large stable disk with sub-percent jitter and a hair of negative bias
        // must not project — the decline is below the noise floor.
        let free = 1.0e12;
        let pts: Vec<(f64, f64)> = (0..=1200)
            .map(|s| {
                let jitter = if s % 2 == 0 { 1.0e6 } else { -1.0e6 };
                (s as f64, free + jitter - (s as f64)) // -1 byte/s bias, ±1MB jitter
            })
            .collect();
        assert!(
            project(&pts).is_none(),
            "noise-driven micro-slope must be declined"
        );
    }

    #[test]
    fn an_already_full_disk_has_no_eta() {
        // free_now <= 0: nothing left to project.
        let pts: Vec<(f64, f64)> = (0..=1200).map(|s| (s as f64, -1.0 * s as f64)).collect();
        assert!(project(&pts).is_none());
    }

    #[test]
    fn non_finite_samples_are_ignored() {
        let free0 = 8.0 * 1024.0 * 1024.0 * 1024.0;
        let rate = 1024.0 * 1024.0;
        let mut pts: Vec<(f64, f64)> = (0..=1200)
            .map(|s| (s as f64, free0 - rate * s as f64))
            .collect();
        // Salt with NaNs (absent-metric gaps recorded as NaN in the ring).
        pts.insert(5, (5.5, f64::NAN));
        pts.insert(50, (f64::NAN, 1.0));
        let eta = project(&pts).expect("finite subset still projects");
        assert!(eta.hours.is_finite() && eta.hours > 0.0);
        assert_eq!(
            DiskFillEta {
                hours: 2.0,
                bytes_per_sec: 1.0
            }
            .hours_f32(),
            2.0
        );
    }
}
