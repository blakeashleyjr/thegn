//! Time-series reduction: bucket a timestamped sample stream down to the
//! handful of columns a character-cell plot can draw, and map raw readings onto
//! the 0..=1 range that [`crate::viz`] consumes.
//!
//! Pure and numeric — no I/O, no formatting. `viz` is the string half of the
//! pair; this is the arithmetic half.
//!
//! # Why bucket by time rather than by index
//!
//! The host samples on a cadence the user controls (`[stats] refresh_secs`,
//! cycled at runtime) and which the UI itself raises to 500ms while a live
//! surface is open. Samples are therefore **not uniformly spaced**. Bucketing by
//! index would render eight minutes of fast samples with the same x-extent as an
//! hour of slow ones — a chart that silently lies about the time axis. Bucketing
//! by timestamp costs one extra ring of `u64` and makes suspend/resume gaps
//! visible for free.
//!
//! # Why min/max rather than LTTB
//!
//! Largest-Triangle-Three-Buckets picks a *subset of input points at
//! non-uniform x*. A braille plot has one dot column per fixed x slot, so LTTB's
//! output would have to be re-gridded anyway. More importantly, for a resource
//! monitor the question is "did it spike?", and [`Agg::Max`] *guarantees* a
//! single fast sample at 100% survives compression into an hour-wide bucket.
//! LTTB's triangle-area heuristic offers no such guarantee.

/// How the samples falling inside one bucket collapse to a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    /// The largest sample. The default for a filled area plot: a transient
    /// spike survives compression rather than being averaged away.
    Max,
    /// The arithmetic mean. For the numeric "average over the window" readout,
    /// not for plots — it is exactly what hides spikes.
    ///
    /// Over a **rolled-up range** (see [`bucket_ranged`]) this is the mean of
    /// range midpoints, i.e. an approximation — the midpoint is the only
    /// estimator a `(min, max)` pair admits. Equal weighting *is* time weighting
    /// there, because every rolled-up range covers the same fixed interval.
    Mean,
    /// Both extremes, drawn as a band by [`crate::viz::braille_band`]. Strictly
    /// more informative than `Max` at the same cost, and the honest choice once
    /// one dot column covers many samples.
    MinMax,
    /// The most recent sample in the bucket. Over a rolled-up range, the most
    /// recent *range*.
    Last,
}

/// What to do with a bucket no sample landed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gap {
    /// Read as zero. Correct for a rate (no samples ⇒ no traffic observed).
    Zero,
    /// Carry the previous bucket forward. Correct for a level (temperature does
    /// not drop to absolute zero because sampling paused).
    Hold,
    /// Leave empty and flag it, so the caller can draw a visible discontinuity.
    Mark,
}

/// How raw values map onto the 0..=1 plot range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scale {
    /// Divide by the visible window's own maximum. Shows **shape** and hides
    /// magnitude; the divisor floors above zero so an idle series reads flat
    /// rather than dividing by nothing.
    Window,
    /// Divide by a caller-supplied full scale (100 for a percentage, 100.0 for
    /// °C, a pinned byte rate). Comparable across time and across windows: a
    /// quiet signal reads honestly flat instead of being amplified into noise.
    Fixed(f32),
    /// `log1p(v/floor) / log1p(max/floor)`. For rates spanning orders of
    /// magnitude, where a 900 MB/s burst otherwise flattens a 200 KB/s baseline
    /// onto the floor. `floor` is clamped above zero.
    Log { floor: f32 },
}

/// A bucket's reduced value as `(lo, hi)`. Equal for every [`Agg`] but
/// [`Agg::MinMax`].
pub type Bucket = Option<(f32, f32)>;

/// Bucket a **monotone** `(timestamp_ms, value)` stream into `buckets` uniform
/// time buckets spanning `[t0, t1)`.
///
/// `None` marks a bucket no sample landed in; `f32::NAN` values are treated as
/// absent (the host records NaN for a metric the platform does not expose, so a
/// missing sensor reads as a gap rather than a flat zero). Single forward pass,
/// `O(n + buckets)`.
///
/// Takes an iterator rather than a slice on purpose: the caller's storage is a
/// `VecDeque`, which is not contiguous, and copying a multi-thousand-sample
/// window into scratch for each of a dozen plots would be hundreds of KiB of
/// memcpy per frame.
pub fn bucket_timed<I>(it: I, t0: u64, t1: u64, buckets: usize, agg: Agg) -> Vec<Bucket>
where
    I: Iterator<Item = (u64, f32)>,
{
    bucket_ranged(it.map(|(t, v)| (t, v, v)), t0, t1, buckets, agg)
}

/// Bucket a **monotone** `(timestamp_ms, lo, hi)` *range* stream into `buckets`
/// uniform time buckets spanning `[t0, t1)`.
///
/// This is the primitive [`bucket_timed`] is written in terms of — a point
/// sample is the degenerate range `(v, v)`.
///
/// # Why ranges are the primitive
///
/// A history that retains more than a few minutes cannot keep every sample, so
/// its long tail is a **rolled-up `(min, max)` per interval**. Re-bucketing that
/// through a point-sample API would have to discard one edge — which is exactly
/// the guarantee [`Agg::Max`] was chosen over LTTB to provide. Taking the range
/// as the unit means a spike that survived the roll-up also survives the plot.
///
/// A range with one `NaN` edge is read as its finite edge; both `NaN` is absent
/// (the host records `NaN` for a metric the platform does not expose, so a
/// missing sensor reads as a gap rather than a flat zero). Single forward pass,
/// `O(n + buckets)`.
pub fn bucket_ranged<I>(it: I, t0: u64, t1: u64, buckets: usize, agg: Agg) -> Vec<Bucket>
where
    I: Iterator<Item = (u64, f32, f32)>,
{
    let mut out: Vec<Bucket> = vec![None; buckets];
    if buckets == 0 || t1 <= t0 {
        return out;
    }
    let span = (t1 - t0) as f64;
    // Mean needs a running count; every other agg folds in place.
    let mut sums: Vec<(f64, u32)> = if agg == Agg::Mean {
        vec![(0.0, 0); buckets]
    } else {
        Vec::new()
    };
    for (t, lo, hi) in it {
        // A half-present range still carries a real reading; only a wholly
        // absent one is a gap.
        let (lo, hi) = match (lo.is_nan(), hi.is_nan()) {
            (true, true) => continue,
            (true, false) => (hi, hi),
            (false, true) => (lo, lo),
            // A reversed range (a caller that swapped the edges) is normalized
            // rather than producing an inverted band downstream.
            (false, false) => (lo.min(hi), lo.max(hi)),
        };
        if t < t0 || t >= t1 {
            continue;
        }
        // Truncating division: a sample exactly on t1 was excluded above, so
        // the index can never reach `buckets`.
        let i = (((t - t0) as f64 / span) * buckets as f64) as usize;
        let i = i.min(buckets - 1);
        match agg {
            Agg::Mean => {
                sums[i].0 += ((lo + hi) * 0.5) as f64;
                sums[i].1 += 1;
            }
            Agg::Last => out[i] = Some((lo, hi)),
            Agg::Max => {
                out[i] = Some(match out[i] {
                    Some((_, prev)) if prev >= hi => (prev, prev),
                    _ => (hi, hi),
                })
            }
            Agg::MinMax => {
                out[i] = Some(match out[i] {
                    Some((l, h)) => (l.min(lo), h.max(hi)),
                    None => (lo, hi),
                })
            }
        }
    }
    if agg == Agg::Mean {
        for (i, (sum, n)) in sums.into_iter().enumerate() {
            if n > 0 {
                let m = (sum / n as f64) as f32;
                out[i] = Some((m, m));
            }
        }
    }
    out
}

/// Fill empty buckets per `policy`, returning a parallel mask that is `true`
/// wherever a bucket had no sample of its own.
///
/// The mask is returned regardless of policy so a caller can render a filled
/// value *and* still know it was interpolated.
pub fn fill_gaps(b: &mut [Bucket], policy: Gap) -> Vec<bool> {
    let mask: Vec<bool> = b.iter().map(|x| x.is_none()).collect();
    match policy {
        Gap::Mark => {}
        Gap::Zero => {
            for x in b.iter_mut() {
                *x = Some(x.unwrap_or((0.0, 0.0)));
            }
        }
        Gap::Hold => {
            // Leading gaps have nothing to hold, so they stay empty rather than
            // inventing a value for time before the first sample.
            let mut prev: Option<(f32, f32)> = None;
            for x in b.iter_mut() {
                match *x {
                    Some(v) => prev = Some(v),
                    None => *x = prev,
                }
            }
        }
    }
    mask
}

/// Map raw values onto 0..=1 under `scale`, returning `(normalized, axis_max)`.
///
/// `axis_max` is the raw value the top of the plot represents, so the caller can
/// label the axis in natural units. NaN inputs map to 0.0 and never contribute
/// to the window maximum.
pub fn normalize(vals: &[f32], scale: Scale) -> (Vec<f32>, f32) {
    let finite_max = || {
        vals.iter()
            .copied()
            .filter(|v| v.is_finite())
            .fold(0.0_f32, f32::max)
    };
    match scale {
        Scale::Window => {
            // Floor the divisor above zero so an all-idle window reads flat
            // instead of dividing by nothing.
            let max = finite_max().max(f32::MIN_POSITIVE);
            (vals.iter().map(|v| ratio(*v, max)).collect(), max)
        }
        Scale::Fixed(full) => {
            let full = if full.is_finite() && full > 0.0 {
                full
            } else {
                1.0
            };
            (vals.iter().map(|v| ratio(*v, full)).collect(), full)
        }
        Scale::Log { floor } => {
            let floor = if floor.is_finite() && floor > 0.0 {
                floor
            } else {
                1.0
            };
            let max = finite_max().max(floor);
            let denom = (max / floor).ln_1p();
            if denom <= 0.0 {
                return (vec![0.0; vals.len()], max);
            }
            let out = vals
                .iter()
                .map(|v| {
                    if !v.is_finite() || *v <= 0.0 {
                        return 0.0;
                    }
                    ((v / floor).ln_1p() / denom).clamp(0.0, 1.0)
                })
                .collect();
            (out, max)
        }
    }
}

/// The raw value at normalized height `t` (0..=1) under `scale`, given the
/// `axis_max` [`normalize`] returned — the exact inverse of its per-value map.
///
/// Lives next to `normalize` so the forward and inverse maps cannot drift.
/// [`Scale::Log`]'s inverse is `floor * expm1(t * ln_1p(max / floor))`, which is
/// not something an axis-labelling caller should be re-deriving; and a log plot
/// is the one case where the rows are **not** evenly spaced in value, so its
/// gutter cannot be interpolated linearly.
pub fn denormalize(t: f32, scale: Scale, axis_max: f32) -> f32 {
    let t = if t.is_finite() {
        t.clamp(0.0, 1.0)
    } else {
        0.0
    };
    match scale {
        // Both divide by a single denominator, so the inverse is a multiply.
        // `normalize` always returns a finite `axis_max`, but a caller that
        // cached one across a config reload could hand back anything; a gutter
        // label of `NaN` would be worse than a label of `0`.
        Scale::Window => {
            if axis_max.is_finite() {
                t * axis_max
            } else {
                0.0
            }
        }
        Scale::Fixed(full) => {
            let full = if full.is_finite() && full > 0.0 {
                full
            } else {
                1.0
            };
            t * full
        }
        Scale::Log { floor } => {
            let floor = if floor.is_finite() && floor > 0.0 {
                floor
            } else {
                1.0
            };
            let max = if axis_max.is_finite() {
                axis_max.max(floor)
            } else {
                floor
            };
            let denom = (max / floor).ln_1p();
            if denom <= 0.0 {
                return 0.0;
            }
            floor * (t * denom).exp_m1()
        }
    }
}

fn ratio(v: f32, denom: f32) -> f32 {
    if v.is_finite() {
        (v / denom).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(ms, value)` pairs at a uniform `step`, starting at `t0`.
    fn at(t0: u64, step: u64, vals: &[f32]) -> Vec<(u64, f32)> {
        vals.iter()
            .enumerate()
            .map(|(i, &v)| (t0 + i as u64 * step, v))
            .collect()
    }

    #[test]
    fn buckets_span_the_requested_time_range() {
        // 4 samples over 4s into 4 buckets: one each.
        let s = at(0, 1000, &[1.0, 2.0, 3.0, 4.0]);
        let b = bucket_timed(s.into_iter(), 0, 4000, 4, Agg::Max);
        assert_eq!(
            b,
            vec![
                Some((1.0, 1.0)),
                Some((2.0, 2.0)),
                Some((3.0, 3.0)),
                Some((4.0, 4.0))
            ]
        );
    }

    #[test]
    fn non_uniform_spacing_lands_in_the_right_time_bucket() {
        // The whole point of time bucketing: a burst of fast samples must
        // occupy the time it actually covers, not one bucket per sample.
        let s = vec![(0, 1.0), (100, 2.0), (200, 3.0), (9_000, 9.0)];
        let b = bucket_timed(s.into_iter(), 0, 10_000, 10, Agg::Max);
        // Three samples inside the first 1s bucket collapse to their max...
        assert_eq!(b[0], Some((3.0, 3.0)));
        // ...the middle stays empty (no samples were taken there)...
        assert!(b[1..9].iter().all(|x| x.is_none()));
        // ...and the late sample lands in the bucket its timestamp implies.
        assert_eq!(b[9], Some((9.0, 9.0)));
    }

    #[test]
    fn max_preserves_a_lone_spike_in_a_wide_bucket() {
        // 100 samples of idle with one spike, compressed into 2 buckets. The
        // spike MUST survive — this is the guarantee that rules out LTTB/mean.
        let mut vals = vec![0.01_f32; 100];
        vals[70] = 1.0;
        let s = at(0, 100, &vals);
        let b = bucket_timed(s.into_iter(), 0, 10_000, 2, Agg::Max);
        assert_eq!(b[1].unwrap().1, 1.0);
        // Mean is what would hide it — kept as a contrast, and why plots don't
        // default to it.
        let s = at(0, 100, &vals);
        let m = bucket_timed(s.into_iter(), 0, 10_000, 2, Agg::Mean);
        assert!(m[1].unwrap().1 < 0.1, "mean smears the spike: {m:?}");
    }

    #[test]
    fn minmax_reports_both_extremes() {
        let s = at(0, 100, &[0.2, 0.9, 0.4, 0.1]);
        let b = bucket_timed(s.into_iter(), 0, 1000, 1, Agg::MinMax);
        assert_eq!(b[0], Some((0.1, 0.9)));
    }

    #[test]
    fn last_takes_the_most_recent_sample_in_the_bucket() {
        let s = at(0, 100, &[1.0, 2.0, 3.0]);
        let b = bucket_timed(s.into_iter(), 0, 1000, 1, Agg::Last);
        assert_eq!(b[0], Some((3.0, 3.0)));
    }

    #[test]
    fn nan_is_absent_not_zero() {
        // A metric the platform does not expose must read as a GAP. Recording
        // it as 0.0 would draw a flat line at zero — a wrong reading rather
        // than a missing one.
        let s = vec![(0, f32::NAN), (1000, f32::NAN)];
        let b = bucket_timed(s.into_iter(), 0, 2000, 2, Agg::Max);
        assert_eq!(b, vec![None, None]);
        // A NaN mixed with real data doesn't poison its bucket.
        let s = vec![(0, f32::NAN), (100, 5.0)];
        let b = bucket_timed(s.into_iter(), 0, 1000, 1, Agg::MinMax);
        assert_eq!(b[0], Some((5.0, 5.0)));
    }

    #[test]
    fn samples_outside_the_window_are_dropped() {
        let s = vec![(0, 1.0), (5000, 2.0), (99_000, 3.0)];
        let b = bucket_timed(s.into_iter(), 1000, 10_000, 3, Agg::Max);
        // t=0 is before t0, t=99000 is past t1; only t=5000 lands.
        assert_eq!(b.iter().filter(|x| x.is_some()).count(), 1);
        // t1 itself is EXCLUSIVE, so a sample exactly on it can't overflow the
        // index.
        let b = bucket_timed(vec![(10_000, 1.0)].into_iter(), 0, 10_000, 4, Agg::Max);
        assert!(b.iter().all(|x| x.is_none()));
    }

    #[test]
    fn degenerate_ranges_never_panic() {
        assert!(bucket_timed(vec![(0, 1.0)].into_iter(), 0, 1000, 0, Agg::Max).is_empty());
        let b = bucket_timed(vec![(0, 1.0)].into_iter(), 500, 500, 4, Agg::Max);
        assert_eq!(b, vec![None; 4]);
        // t1 < t0 (a clock jump) yields empties, not a panic.
        let b = bucket_timed(vec![(0, 1.0)].into_iter(), 900, 100, 4, Agg::Max);
        assert_eq!(b, vec![None; 4]);
        // An empty stream is fine.
        let b = bucket_timed(std::iter::empty(), 0, 1000, 3, Agg::Mean);
        assert_eq!(b, vec![None; 3]);
    }

    /// `(ms, lo, hi)` ranges at a uniform `step`, starting at `t0`.
    fn ranges(t0: u64, step: u64, vals: &[(f32, f32)]) -> Vec<(u64, f32, f32)> {
        vals.iter()
            .enumerate()
            .map(|(i, &(lo, hi))| (t0 + i as u64 * step, lo, hi))
            .collect()
    }

    #[test]
    fn bucket_timed_equals_bucket_ranged_over_degenerate_ranges() {
        // Makes "the point-sample suite is unaffected" an assertion rather than
        // a hope: `bucket_timed` IS `bucket_ranged` over `(v, v)`.
        let vals = [0.4_f32, 0.0, 9.1, 2.2, f32::NAN, 3.3, 0.1];
        for agg in [Agg::Max, Agg::Mean, Agg::MinMax, Agg::Last] {
            for buckets in [1usize, 2, 3, 7, 16] {
                let a = bucket_timed(at(0, 500, &vals).into_iter(), 0, 4000, buckets, agg);
                let b = bucket_ranged(
                    at(0, 500, &vals).into_iter().map(|(t, v)| (t, v, v)),
                    0,
                    4000,
                    buckets,
                    agg,
                );
                assert_eq!(a, b, "agg={agg:?} buckets={buckets}");
            }
        }
    }

    #[test]
    fn bucket_ranged_max_takes_the_upper_edge() {
        // THE headline guarantee: a spike that survived the roll-up into a
        // 10-second `(lo, hi)` range must also survive compression of that range
        // into an hour-wide plot column. Taking `lo` — or a midpoint — here is
        // what would lose it.
        let mut r = vec![(0.01_f32, 0.02_f32); 100];
        r[70] = (0.01, 100.0);
        let b = bucket_ranged(ranges(0, 100, &r).into_iter(), 0, 10_000, 2, Agg::Max);
        assert_eq!(b[1].unwrap().1, 100.0);
        // Mean over the same ranges smears it — the contrast that rules Mean out
        // for plots.
        let m = bucket_ranged(ranges(0, 100, &r).into_iter(), 0, 10_000, 2, Agg::Mean);
        assert!(m[1].unwrap().1 < 2.0, "mean smears the spike: {m:?}");
    }

    #[test]
    fn bucket_ranged_preserves_a_range_band() {
        // MinMax over ranges must widen to the extremes of every range in the
        // bucket, not just to the range endpoints of one of them.
        let r = [(2.0_f32, 5.0_f32), (1.0, 3.0), (4.0, 9.0)];
        let b = bucket_ranged(ranges(0, 100, &r).into_iter(), 0, 1000, 1, Agg::MinMax);
        assert_eq!(b[0], Some((1.0, 9.0)));
        // Last keeps the whole final range, not just its top.
        let b = bucket_ranged(ranges(0, 100, &r).into_iter(), 0, 1000, 1, Agg::Last);
        assert_eq!(b[0], Some((4.0, 9.0)));
    }

    #[test]
    fn bucket_ranged_mean_is_the_mean_of_midpoints() {
        // (0+10)/2 = 5 and (4+6)/2 = 5 → 5. The midpoint is the only estimator a
        // (min, max) pair admits.
        let r = [(0.0_f32, 10.0_f32), (4.0, 6.0)];
        let b = bucket_ranged(ranges(0, 100, &r).into_iter(), 0, 1000, 1, Agg::Mean);
        assert_eq!(b[0], Some((5.0, 5.0)));
    }

    #[test]
    fn bucket_ranged_reads_a_half_nan_range_as_its_finite_edge() {
        // A roll-up that observed exactly one finite sample carries it on one
        // edge only. That is a reading, not a gap.
        let r = [(f32::NAN, 7.0_f32)];
        let b = bucket_ranged(ranges(0, 100, &r).into_iter(), 0, 1000, 1, Agg::MinMax);
        assert_eq!(b[0], Some((7.0, 7.0)));
        let r = [(7.0_f32, f32::NAN)];
        let b = bucket_ranged(ranges(0, 100, &r).into_iter(), 0, 1000, 1, Agg::MinMax);
        assert_eq!(b[0], Some((7.0, 7.0)));
        // Both edges absent is still a gap.
        let r = [(f32::NAN, f32::NAN)];
        let b = bucket_ranged(ranges(0, 100, &r).into_iter(), 0, 1000, 1, Agg::MinMax);
        assert_eq!(b[0], None);
    }

    #[test]
    fn bucket_ranged_normalizes_a_reversed_range() {
        // Swapped edges must not produce an inverted band downstream.
        let fwd = bucket_ranged(vec![(0, 2.0, 8.0)].into_iter(), 0, 1000, 1, Agg::MinMax);
        let rev = bucket_ranged(vec![(0, 8.0, 2.0)].into_iter(), 0, 1000, 1, Agg::MinMax);
        assert_eq!(fwd, rev);
        assert_eq!(fwd[0], Some((2.0, 8.0)));
    }

    #[test]
    fn chained_tiers_bucket_as_one_stream() {
        // The two-tier read: coarse ranges for the old half, point samples for
        // the recent half, chained. Neither tier may drop a column or double one.
        let coarse = vec![(0_u64, 1.0_f32, 4.0_f32), (1_000, 2.0, 6.0)];
        let fine = vec![(2_000_u64, 3.0_f32), (3_000, 9.0)];
        let it = coarse
            .into_iter()
            .chain(fine.into_iter().map(|(t, v)| (t, v, v)));
        let b = bucket_ranged(it, 0, 4_000, 4, Agg::MinMax);
        assert_eq!(
            b,
            vec![
                Some((1.0, 4.0)),
                Some((2.0, 6.0)),
                Some((3.0, 3.0)),
                Some((9.0, 9.0)),
            ]
        );
    }

    #[test]
    fn fill_gaps_zero_and_hold_and_mark() {
        let base = vec![None, Some((1.0, 2.0)), None, Some((3.0, 3.0)), None];

        let mut b = base.clone();
        let mask = fill_gaps(&mut b, Gap::Zero);
        assert_eq!(mask, vec![true, false, true, false, true]);
        assert_eq!(b[0], Some((0.0, 0.0)));
        assert_eq!(b[4], Some((0.0, 0.0)));

        let mut b = base.clone();
        fill_gaps(&mut b, Gap::Hold);
        // A LEADING gap has nothing to hold — it must stay empty rather than
        // invent a value for time before the first sample.
        assert_eq!(b[0], None);
        assert_eq!(b[2], Some((1.0, 2.0)));
        assert_eq!(b[4], Some((3.0, 3.0)));

        let mut b = base.clone();
        let mask = fill_gaps(&mut b, Gap::Mark);
        assert_eq!(b, base);
        assert_eq!(mask, vec![true, false, true, false, true]);
    }

    #[test]
    fn window_scale_normalizes_against_the_visible_max() {
        let (n, ax) = normalize(&[50.0, 100.0], Scale::Window);
        assert_eq!(n, vec![0.5, 1.0]);
        assert_eq!(ax, 100.0);
        // All-zero must not divide by nothing.
        let (n, _) = normalize(&[0.0, 0.0], Scale::Window);
        assert_eq!(n, vec![0.0, 0.0]);
        assert!(n.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn fixed_scale_is_comparable_across_windows() {
        let (n, ax) = normalize(&[50.0, 100.0], Scale::Fixed(1000.0));
        assert_eq!(n, vec![0.05, 0.1]);
        assert_eq!(ax, 1000.0);
        // Same data, same result, regardless of what else is in the window —
        // that's the property Window scaling lacks.
        let (n2, _) = normalize(&[50.0, 100.0, 900.0], Scale::Fixed(1000.0));
        assert_eq!(&n2[..2], &n[..]);
        // A nonsense full scale falls back to 1.0 instead of producing inf.
        let (n, _) = normalize(&[0.5], Scale::Fixed(0.0));
        assert_eq!(n, vec![0.5]);
        let (n, _) = normalize(&[0.5], Scale::Fixed(f32::NAN));
        assert_eq!(n, vec![0.5]);
    }

    #[test]
    fn log_scale_is_monotonic_and_bounded() {
        let vals = [0.0, 1.0, 1e3, 1e6, 1e9];
        let (n, ax) = normalize(&vals, Scale::Log { floor: 1.0 });
        assert_eq!(ax, 1e9);
        for w in n.windows(2) {
            assert!(w[0] <= w[1], "not monotonic: {n:?}");
        }
        assert!(n.iter().all(|v| (0.0..=1.0).contains(v)), "{n:?}");
        assert_eq!(n[0], 0.0);
        assert!((n[4] - 1.0).abs() < 1e-6);
        // A small baseline is legible instead of pinned to the floor — the
        // whole reason Log exists.
        let (lin, _) = normalize(&vals, Scale::Window);
        assert!(
            n[2] > lin[2] * 100.0,
            "log lifts the baseline: {n:?} {lin:?}"
        );
    }

    #[test]
    fn log_scale_degenerate_inputs_never_produce_nan() {
        let (n, _) = normalize(&[0.0, 0.0], Scale::Log { floor: 1.0 });
        assert_eq!(n, vec![0.0, 0.0]);
        // Non-positive / nonsense floors are clamped rather than dividing by
        // zero or taking ln of a negative.
        for floor in [0.0_f32, -5.0, f32::NAN, f32::INFINITY] {
            let (n, _) = normalize(&[1.0, 10.0], Scale::Log { floor });
            assert!(n.iter().all(|v| v.is_finite()), "floor={floor} -> {n:?}");
        }
        // Negative readings clamp to the floor, not to a NaN.
        let (n, _) = normalize(&[-3.0, 4.0], Scale::Log { floor: 1.0 });
        assert_eq!(n[0], 0.0);
    }

    #[test]
    fn every_scale_tolerates_non_finite_values() {
        for scale in [
            Scale::Window,
            Scale::Fixed(100.0),
            Scale::Log { floor: 1.0 },
        ] {
            let (n, ax) = normalize(&[f32::NAN, f32::INFINITY, 50.0], scale);
            assert!(n.iter().all(|v| v.is_finite()), "{scale:?} -> {n:?}");
            assert!(ax.is_finite(), "{scale:?} axis {ax}");
            // A non-finite reading must not become the window maximum.
            assert_eq!(n[0], 0.0);
        }
    }

    #[test]
    fn denormalize_inverts_normalize() {
        // The property the axis gutter depends on: the label at row height `t`
        // must be the value the plot actually draws there.
        let vals = [0.0_f32, 1.0, 42.0, 1e3, 1e6];
        for scale in [
            Scale::Window,
            Scale::Fixed(100.0),
            Scale::Fixed(1e9),
            Scale::Log { floor: 1.0 },
        ] {
            let (norm, axis_max) = normalize(&vals, scale);
            for (v, t) in vals.iter().zip(norm.iter()) {
                let back = denormalize(*t, scale, axis_max);
                // Clamped inputs can't round-trip past the ceiling.
                if *v > axis_max {
                    continue;
                }
                let tol = (v.abs() * 1e-3).max(1e-3);
                assert!(
                    (back - v).abs() <= tol,
                    "{scale:?}: {v} -> {t} -> {back} (axis_max {axis_max})"
                );
            }
            // The endpoints are exact by construction.
            assert_eq!(denormalize(0.0, scale, axis_max), 0.0);
            assert!((denormalize(1.0, scale, axis_max) - axis_max).abs() <= axis_max * 1e-3);
        }
    }

    #[test]
    fn denormalize_degenerate_inputs_never_produce_nan() {
        for scale in [
            Scale::Window,
            Scale::Fixed(0.0),
            Scale::Fixed(f32::NAN),
            Scale::Log { floor: 0.0 },
            Scale::Log { floor: f32::NAN },
        ] {
            for t in [-1.0_f32, 0.0, 0.5, 1.0, 2.0, f32::NAN, f32::INFINITY] {
                for axis_max in [0.0_f32, 1.0, 1e9, f32::NAN] {
                    let v = denormalize(t, scale, axis_max);
                    assert!(v.is_finite(), "{scale:?} t={t} max={axis_max} -> {v}");
                }
            }
        }
    }

    #[test]
    fn normalize_preserves_length_and_clamps() {
        let (n, _) = normalize(&[], Scale::Window);
        assert!(n.is_empty());
        // Values above a fixed full scale clamp rather than overshooting the
        // plot area.
        let (n, _) = normalize(&[250.0], Scale::Fixed(100.0));
        assert_eq!(n, vec![1.0]);
        // Negative readings clamp to the floor.
        let (n, _) = normalize(&[-4.0, 2.0], Scale::Fixed(4.0));
        assert_eq!(n, vec![0.0, 0.5]);
    }
}
