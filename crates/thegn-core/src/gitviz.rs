//! Commit-calendar math for the git overlay: bucket commit timestamps into
//! a week × weekday heat grid and weekly counts. Pure — the host fetches
//! `git log --format=%ct` off-thread and renders through the theme's heat
//! ramp.

const DAY: i64 = 86_400;
const WEEK: i64 = 7 * DAY;

/// The Monday 00:00 (UTC) on or before `t`. Unix epoch (1970-01-01) was a
/// Thursday, so weekday-from-epoch needs a +3 day shift to make Monday 0.
fn week_floor(t: i64) -> i64 {
    let days = t.div_euclid(DAY);
    let weekday = (days + 3).rem_euclid(7); // Mon=0 … Sun=6
    (days - weekday) * DAY
}

/// Weekday index of `t`: Mon=0 … Sun=6.
fn weekday(t: i64) -> usize {
    ((t.div_euclid(DAY) + 3).rem_euclid(7)) as usize
}

/// Per-day commit counts over the trailing `weeks` ending at `now`:
/// `grid[week][weekday]`, oldest week first, Mon=0.
pub fn day_counts(epochs: &[i64], now: i64, weeks: usize) -> Vec<[u32; 7]> {
    let mut grid = vec![[0u32; 7]; weeks];
    if weeks == 0 {
        return grid;
    }
    let last_week = week_floor(now);
    let first_week = last_week - (weeks as i64 - 1) * WEEK;
    for &t in epochs {
        if t < first_week || t >= last_week + WEEK {
            continue;
        }
        let w = ((week_floor(t) - first_week) / WEEK) as usize;
        if let Some(week) = grid.get_mut(w) {
            week[weekday(t)] += 1;
        }
    }
    grid
}

/// The heat grid: day counts quantized to levels 0..=4, scaled against the
/// busiest day in range (so one huge day doesn't flatten everything, levels
/// 1..=4 split the observed max evenly; 0 = no commits).
pub fn heat_grid(epochs: &[i64], now: i64, weeks: usize) -> Vec<[u8; 7]> {
    let counts = day_counts(epochs, now, weeks);
    let max = counts
        .iter()
        .flat_map(|w| w.iter())
        .copied()
        .max()
        .unwrap_or(0);
    counts
        .iter()
        .map(|week| {
            let mut out = [0u8; 7];
            for (d, &c) in week.iter().enumerate() {
                out[d] = if c == 0 || max == 0 {
                    0
                } else {
                    // 1..=4, evenly split over the observed max.
                    (1 + (c - 1) * 4 / max).min(4) as u8
                };
            }
            out
        })
        .collect()
}

/// Weekly totals over the trailing `weeks` (oldest first) — the velocity
/// graph's series.
pub fn weekly_counts(epochs: &[i64], now: i64, weeks: usize) -> Vec<u32> {
    day_counts(epochs, now, weeks)
        .iter()
        .map(|w| w.iter().sum())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-06-08 (a Monday) 00:00 UTC.
    const MON: i64 = 1_780_876_800;

    #[test]
    fn week_floor_and_weekday_anchor_on_monday() {
        assert_eq!(week_floor(MON), MON);
        assert_eq!(weekday(MON), 0);
        assert_eq!(weekday(MON + 3 * DAY), 3); // Thursday
        assert_eq!(weekday(MON + 6 * DAY + 3600), 6); // Sunday
        assert_eq!(week_floor(MON + 6 * DAY + 3600), MON);
        // Epoch itself was a Thursday.
        assert_eq!(weekday(0), 3);
        // Negative times stay sane (pre-epoch).
        assert_eq!(weekday(-DAY), 2);
    }

    #[test]
    fn day_counts_bucket_into_the_right_cells() {
        // now = this Monday + 1h; 2-week window = last Monday .. this Sunday.
        let now = MON + 3600;
        let epochs = vec![
            MON + 3600,     // this Mon
            MON + 3600,     // this Mon again
            MON + 2 * DAY,  // this Wed
            MON - 7 * DAY,  // last Mon
            MON - DAY,      // last Sun
            MON - 14 * DAY, // two Mondays ago — out of range
            MON + 8 * DAY,  // next week — out of range
        ];
        let grid = day_counts(&epochs, now, 2);
        assert_eq!(grid.len(), 2);
        assert_eq!(grid[0][0], 1, "last Mon");
        assert_eq!(grid[0][6], 1, "last Sun");
        assert_eq!(grid[1][0], 2, "this Mon (two commits)");
        assert_eq!(grid[1][2], 1, "this Wed");
        assert_eq!(grid[0].iter().sum::<u32>() + grid[1].iter().sum::<u32>(), 5);
    }

    #[test]
    fn heat_grid_quantizes_against_the_busiest_day() {
        let now = MON + 3600;
        // Busiest day = 8 commits → levels split 1..=4 over 8.
        let mut epochs = vec![MON; 8]; // this Mon ×8
        epochs.extend([MON + DAY; 2]); // this Tue ×2
        epochs.push(MON + 2 * DAY); // this Wed ×1
        let grid = heat_grid(&epochs, now, 1);
        assert_eq!(grid[0][0], 4, "max day is hottest");
        assert_eq!(grid[0][1], 1, "2 of 8 → low");
        assert_eq!(grid[0][2], 1, "1 of 8 → lowest non-zero");
        assert_eq!(grid[0][3], 0, "empty day stays 0");
        // All-empty input: all zeros, right shape.
        let empty = heat_grid(&[], now, 3);
        assert_eq!(empty.len(), 3);
        assert!(empty.iter().all(|w| w.iter().all(|&l| l == 0)));
    }

    #[test]
    fn weekly_counts_sum_days() {
        let now = MON + 3600;
        let epochs = vec![MON, MON + DAY, MON - 7 * DAY];
        assert_eq!(weekly_counts(&epochs, now, 2), vec![1, 2]);
        assert_eq!(weekly_counts(&epochs, now, 0), Vec::<u32>::new());
    }
}
