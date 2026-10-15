use gtui_core::frame::{FieldType, Frame};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, canvas::Canvas, canvas::Line};

pub struct TimeseriesRenderer;

/// Samples per horizontal braille pixel above which a series is reduced. Below
/// it every sample is drawn, so the output is identical to the naive walk.
const SAMPLES_PER_PIXEL: usize = 4;
/// Bound per-render scans while retaining the existing million-sample cases.
const MAX_SAMPLES_PER_SERIES: usize = 1 << 21;
/// Timestamp changes larger than three mean sample intervals break a line.
const GAP_FACTOR: f64 = 3.0;

/// The points to draw as `(index, value)`, bounded by the panel width.
///
/// A series of at most `SAMPLES_PER_PIXEL` samples per horizontal braille pixel
/// (`2 * width`) is returned whole. A longer one is reduced bucket-by-bucket
/// (one bucket per pixel) to the first, minimum, maximum and last finite sample
/// in index order, so extrema and the original X positions survive, plus the
/// first non-finite sample of a bucket so gaps keep their position. Work and
/// scratch are O(width), after one linear pass over the borrowed slice.
fn plot_points(values: &[f64], width: usize) -> Vec<(f64, f64)> {
    let start = values.len().saturating_sub(MAX_SAMPLES_PER_SERIES);
    let values = &values[start..];
    let pixels = width.saturating_mul(2).max(1);
    if values.len() <= pixels.saturating_mul(SAMPLES_PER_PIXEL) {
        return values
            .iter()
            .enumerate()
            .map(|(i, &v)| ((start + i) as f64, v))
            .collect();
    }
    let mut out = Vec::with_capacity(pixels * 4);
    let n = values.len();
    for b in 0..pixels {
        let (lo_i, hi_i) = (b * n / pixels, (b + 1) * n / pixels);
        let mut picks: [Option<usize>; 5] = [None; 5]; // first,min,max,last,non-finite
        for (i, &v) in values.iter().enumerate().take(hi_i).skip(lo_i) {
            if !v.is_finite() {
                picks[4].get_or_insert(i);
                continue;
            }
            picks[0].get_or_insert(i);
            picks[3] = Some(i);
            if picks[1].is_none_or(|m| v < values[m]) {
                picks[1] = Some(i);
            }
            if picks[2].is_none_or(|m| v > values[m]) {
                picks[2] = Some(i);
            }
        }
        let mut idx: Vec<usize> = picks.iter().flatten().copied().collect();
        idx.sort_unstable();
        idx.dedup();
        out.extend(idx.into_iter().map(|i| ((start + i) as f64, values[i])));
    }
    out
}

/// Plot samples by their timestamp, preserving their input order within each
/// time bucket. Out-of-order timestamps therefore remain out of order in the
/// connected line. Non-finite timestamps are skipped.
#[cfg(test)]
fn plot_time_points(
    times: &[f64],
    values: &[f64],
    width: usize,
) -> (Vec<(f64, f64)>, Option<[f64; 2]>) {
    plot_time_points_in(times, values, width, None)
}

/// Plot the newest bounded sample suffix by timestamp. A valid requested
/// window is inclusive at both ends, supplies the X domain, and excludes
/// samples before domain calculation, bucketing, and gap estimation. Invalid
/// windows are treated as absent. Input order is retained, including for
/// duplicate and out-of-order timestamps.
fn plot_time_points_in(
    times: &[f64],
    values: &[f64],
    width: usize,
    window: Option<[f64; 2]>,
) -> (Vec<(f64, f64)>, Option<[f64; 2]>) {
    let window = window.filter(|[lo, hi]| lo.is_finite() && hi.is_finite() && lo < hi);
    let samples = times.len().min(values.len());
    let start = samples.saturating_sub(MAX_SAMPLES_PER_SERIES);
    let times = &times[start..samples];
    let values = &values[start..samples];
    if let Some(window) = window {
        return plot_time_points_in_window(times, values, width, window);
    }
    let domain: Option<[f64; 2]> = times.iter().copied().filter(|time| time.is_finite()).fold(
        None,
        |domain, time| match domain {
            None => Some([time, time]),
            Some([min, max]) => Some([min.min(time), max.max(time)]),
        },
    );
    let Some([mut min_time, mut max_time]) = domain else {
        return (Vec::new(), None);
    };
    if min_time == max_time {
        min_time -= 0.5;
        max_time += 0.5;
    }
    let domain = Some([min_time, max_time]);
    let pixels = width.saturating_mul(2).max(1);
    let samples = times.len();
    if samples <= pixels.saturating_mul(SAMPLES_PER_PIXEL) {
        let points = times
            .iter()
            .copied()
            .zip(values.iter().copied())
            .filter(|(time, _)| time.is_finite())
            .collect();
        return (points, domain);
    }

    let mut buckets: Vec<[Option<usize>; 5]> = vec![[None; 5]; pixels];
    for (index, (&time, &value)) in times.iter().zip(values).enumerate() {
        if !time.is_finite() {
            continue;
        }
        let bucket = if max_time == min_time {
            0
        } else {
            (((time - min_time) / (max_time - min_time) * pixels as f64) as usize).min(pixels - 1)
        };
        let picks = &mut buckets[bucket];
        if !value.is_finite() {
            picks[4].get_or_insert(index);
            continue;
        }
        picks[0].get_or_insert(index);
        picks[3] = Some(index);
        if picks[1].is_none_or(|min| value < values[min]) {
            picks[1] = Some(index);
        }
        if picks[2].is_none_or(|max| value > values[max]) {
            picks[2] = Some(index);
        }
    }
    let mut indices = Vec::with_capacity(pixels.saturating_mul(5));
    for picks in buckets {
        indices.extend(picks.into_iter().flatten());
    }
    indices.sort_unstable();
    indices.dedup();
    let points = indices
        .into_iter()
        .map(|index| (times[index], values[index]))
        .collect();
    (points, domain)
}

fn plot_time_points_in_window(
    times: &[f64],
    values: &[f64],
    width: usize,
    window: [f64; 2],
) -> (Vec<(f64, f64)>, Option<[f64; 2]>) {
    let [min_time, max_time] = window;
    let pixels = width.saturating_mul(2).max(1);
    let samples = times.len().min(values.len());
    let in_window_count = times
        .iter()
        .take(samples)
        .filter(|time| time.is_finite() && **time >= min_time && **time <= max_time)
        .count();
    if in_window_count <= pixels.saturating_mul(SAMPLES_PER_PIXEL) {
        let points = times
            .iter()
            .copied()
            .zip(values.iter().copied())
            .take(samples)
            .filter(|(time, _)| time.is_finite() && *time >= min_time && *time <= max_time)
            .collect();
        return (points, Some(window));
    }

    let mut buckets: Vec<[Option<usize>; 5]> = vec![[None; 5]; pixels];
    for (index, (&time, &value)) in times.iter().zip(values).enumerate() {
        if !time.is_finite() || time < min_time || time > max_time {
            continue;
        }
        let bucket =
            (((time - min_time) / (max_time - min_time) * pixels as f64) as usize).min(pixels - 1);
        let picks = &mut buckets[bucket];
        if !value.is_finite() {
            picks[4].get_or_insert(index);
            continue;
        }
        picks[0].get_or_insert(index);
        picks[3] = Some(index);
        if picks[1].is_none_or(|min| value < values[min]) {
            picks[1] = Some(index);
        }
        if picks[2].is_none_or(|max| value > values[max]) {
            picks[2] = Some(index);
        }
    }
    let mut indices = Vec::with_capacity(pixels.saturating_mul(5));
    for picks in buckets {
        indices.extend(picks.into_iter().flatten());
    }
    indices.sort_unstable();
    indices.dedup();
    let points = indices
        .into_iter()
        .map(|index| (times[index], values[index]))
        .collect();
    (points, Some(window))
}

/// Convert ordered samples into drawable line segments, breaking at every
/// sample with a non-finite coordinate so missing values remain visible gaps.
fn segments(points: &[(f64, f64)]) -> impl Iterator<Item = [(f64, f64); 2]> + '_ {
    points.windows(2).filter_map(|pair| {
        let [left, right] = [pair[0], pair[1]];
        (left.0.is_finite() && left.1.is_finite() && right.0.is_finite() && right.1.is_finite())
            .then_some([left, right])
    })
}

/// Estimate a line-break threshold from the finite timestamp domain. The
/// threshold is three times the mean interval (`span / (finite_count - 1)`);
/// for width-reduced series it is at least two timestamp bucket widths so
/// bucket reduction does not create artificial gaps. Consequently, holes
/// narrower than two buckets are invisible at that width. Segment checks use
/// the absolute timestamp delta, so out-of-order samples follow the same rule.
#[cfg(test)]
fn gap_threshold(times: &[f64], domain: [f64; 2], width: usize) -> Option<f64> {
    gap_threshold_in(times, domain, width, None)
}

fn gap_threshold_in(
    times: &[f64],
    domain: [f64; 2],
    width: usize,
    window: Option<[f64; 2]>,
) -> Option<f64> {
    let window = window.filter(|[lo, hi]| lo.is_finite() && hi.is_finite() && lo < hi);
    let start = times.len().saturating_sub(MAX_SAMPLES_PER_SERIES);
    let times = &times[start..];
    let finite_count = times
        .iter()
        .filter(|time| {
            time.is_finite() && window.is_none_or(|[lo, hi]| **time >= lo && **time <= hi)
        })
        .count();
    if finite_count < 2 {
        return None;
    }
    let span = domain[1] - domain[0];
    if !span.is_finite() || span <= 0.0 {
        return None;
    }
    let mean_interval = span / (finite_count - 1) as f64;
    let mut threshold = GAP_FACTOR * mean_interval;
    let pixels = width.saturating_mul(2).max(1);
    let sample_count = if window.is_some() {
        finite_count
    } else {
        times.len()
    };
    if sample_count > pixels.saturating_mul(SAMPLES_PER_PIXEL) {
        let bucket_width = span / pixels as f64;
        threshold = threshold.max(2.0 * bucket_width);
    }
    threshold.is_finite().then_some(threshold)
}

fn segments_with_gap(
    points: &[(f64, f64)],
    gap: f64,
) -> impl Iterator<Item = [(f64, f64); 2]> + '_ {
    segments(points).filter(move |[left, right]| (right.0 - left.0).abs() <= gap)
}

impl TimeseriesRenderer {
    /// `width` is the panel width in columns; it bounds how many samples are
    /// walked (see [`plot_points`]).
    pub fn render<'a>(
        frame: &'a Frame,
        title: &str,
        bounds: [f64; 4], // [x_min, x_max, y_min, y_max]
        width: usize,
        colors: &crate::RenderColors,
    ) -> Canvas<'a, impl Fn(&mut ratatui::widgets::canvas::Context) + 'a> {
        Self::render_in_window(frame, title, bounds, None, width, colors)
    }

    /// Render a time series against an optional inclusive timestamp window.
    /// Frames without a matching time field continue to use index bounds.
    pub fn render_in_window<'a>(
        frame: &'a Frame,
        title: &str,
        bounds: [f64; 4], // [x_min, x_max, y_min, y_max]
        window: Option<[f64; 2]>,
        width: usize,
        colors: &crate::RenderColors,
    ) -> Canvas<'a, impl Fn(&mut ratatui::widgets::canvas::Context) + 'a> {
        let colors = *colors;
        let y_field = frame.fields.iter().find(|f| f.ty == FieldType::Float64);
        let time_field = y_field.and_then(|y| {
            frame
                .fields
                .iter()
                .find(|field| field.ty == FieldType::Time && field.len() == y.len())
        });
        let (points, x_bounds, gap) = match (time_field, y_field) {
            (Some(time), Some(y)) => {
                let times = time.floats();
                let times = &times[..times.len().min(y.floats().len())];
                let (points, domain) = plot_time_points_in(times, y.floats(), width, window);
                let x_bounds = domain.unwrap_or([bounds[0], bounds[1]]);
                (
                    points,
                    x_bounds,
                    gap_threshold_in(times, x_bounds, width, window),
                )
            }
            (_, Some(y)) => (plot_points(y.floats(), width), [bounds[0], bounds[1]], None),
            (_, None) => (Vec::new(), [bounds[0], bounds[1]], None),
        };
        Canvas::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(colors.border))
                    .title_style(Style::default().fg(colors.title))
                    .title(title.to_string()),
            )
            .x_bounds(x_bounds)
            .y_bounds([bounds[2], bounds[3]])
            .paint(move |ctx| {
                if let Some(gap) = gap {
                    for [(x1, y1), (x2, y2)] in segments_with_gap(&points, gap) {
                        ctx.draw(&Line {
                            x1,
                            y1,
                            x2,
                            y2,
                            color: colors.series,
                        });
                    }
                } else {
                    for [(x1, y1), (x2, y2)] in segments(&points) {
                        ctx.draw(&Line {
                            x1,
                            y1,
                            x2,
                            y2,
                            color: colors.series,
                        });
                    }
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtui_core::frame::Field;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    #[test]
    fn test_timeseries_render() {
        let field = Field::new("val", FieldType::Float64, vec![1.0, 5.0, 2.0]);
        let frame = Frame::new(vec![field]);

        let canvas = TimeseriesRenderer::render(
            &frame,
            "CPU",
            [0.0, 2.0, 0.0, 6.0],
            20,
            &crate::RenderColors::default(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 10));

        canvas.render(buffer.area, &mut buffer);

        // Just assert it didn't panic and drew something (the borders at least)
        assert_eq!(buffer.cell((0, 0)).unwrap().symbol(), "┌");
    }

    #[test]
    fn supplied_series_color_reaches_the_buffer() {
        let frame = Frame::new(vec![Field::new("val", FieldType::Float64, vec![0.0, 1.0])]);
        let colors = crate::RenderColors {
            series: ratatui::style::Color::Rgb(1, 2, 3),
            ..crate::RenderColors::default()
        };
        let area = Rect::new(0, 0, 20, 10);
        let mut buffer = Buffer::empty(area);
        TimeseriesRenderer::render(&frame, "T", [0.0, 1.0, 0.0, 1.0], 20, &colors)
            .render(area, &mut buffer);
        assert!(buffer.content.iter().any(|cell| cell.fg == colors.series));
    }

    /// The pre-borrow walk: every sample, cloned vec.
    fn reference(frame: &Frame, bounds: [f64; 4], area: Rect) -> Buffer {
        let vals = frame.fields[0].floats().to_vec();
        let c = Canvas::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(crate::RenderColors::default().border))
                    .title_style(Style::default().fg(crate::RenderColors::default().title))
                    .title(""),
            )
            .x_bounds([bounds[0], bounds[1]])
            .y_bounds([bounds[2], bounds[3]])
            .paint(move |ctx| {
                let mut prev: Option<(f64, f64)> = None;
                for (i, v) in vals.clone().into_iter().enumerate() {
                    let x = i as f64;
                    if let Some((px, py)) = prev {
                        ctx.draw(&Line {
                            x1: px,
                            y1: py,
                            x2: x,
                            y2: v,
                            color: crate::RenderColors::default().series,
                        });
                    }
                    prev = Some((x, v));
                }
            });
        let mut b = Buffer::empty(area);
        c.render(area, &mut b);
        b
    }

    #[test]
    fn within_budget_output_is_identical_to_naive_walk() {
        let vals: Vec<f64> = (0..150).map(|i| ((i as f64) * 0.3).sin() * 10.0).collect();
        let frame = Frame::new(vec![Field::new("v", FieldType::Float64, vals)]);
        let bounds = [0.0, 149.0, -11.0, 11.0];
        let area = Rect::new(0, 0, 40, 10);
        let mut got = Buffer::empty(area);
        let canvas =
            TimeseriesRenderer::render(&frame, "", bounds, 40, &crate::RenderColors::default());
        canvas.render(area, &mut got);
        assert_eq!(got, reference(&frame, bounds, area));
        assert_eq!(plot_points(frame.fields[0].floats(), 40).len(), 150);
    }

    #[test]
    fn irregular_timestamp_buffer_matches_raw_timestamp_golden() {
        let times = vec![0.0, 1.0, 4.0, 10.0];
        let values = vec![1.0, 4.0, 3.0, 8.0];
        let frame = Frame::new(vec![
            Field::new("time", FieldType::Time, times.clone()),
            Field::new("value", FieldType::Float64, values.clone()),
        ]);
        let colors = crate::RenderColors::default();
        let area = Rect::new(0, 0, 14, 8);
        let bounds = [0.0, 10.0, 0.0, 10.0];
        let mut actual = Buffer::empty(area);
        TimeseriesRenderer::render_in_window(&frame, "irregular", bounds, None, 14, &colors)
            .render(area, &mut actual);

        // Golden contract: the short irregular series reaches Canvas in its
        // actual timestamp coordinates and retains every sample in order.
        let expected = Canvas::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(colors.border))
                    .title_style(Style::default().fg(colors.title))
                    .title("irregular"),
            )
            .x_bounds([0.0, 10.0])
            .y_bounds([0.0, 10.0])
            .paint(move |ctx| {
                for pair in times.windows(2).zip(values.windows(2)) {
                    let (t, v) = pair;
                    ctx.draw(&Line {
                        x1: t[0],
                        y1: v[0],
                        x2: t[1],
                        y2: v[1],
                        color: colors.series,
                    });
                }
            });
        let mut golden = Buffer::empty(area);
        expected.render(area, &mut golden);
        assert_eq!(actual, golden);
    }

    #[test]
    fn seeded_timestamp_sweeps_keep_extrema_domain_order_and_width_bound() {
        let mut seed = 0x6a09_e667_f3bc_c909_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for _case in 0..160 {
            let count = 40 + (next() as usize % 661);
            let width = 1 + (next() as usize % 20);
            let times: Vec<f64> = (0..count).map(|_| (next() % 101) as f64).collect();
            let mut values: Vec<f64> = (0..count).map(|index| index as f64).collect();
            values[count / 3] = -10_000.0;
            values[count * 2 / 3] = 10_000.0;

            let (points, domain) = plot_time_points(&times, &values, width);
            let [min_time, max_time] = domain.expect("generated timestamps have a domain");
            assert!(points.len() <= 5 * 2 * width);
            assert!(
                points
                    .iter()
                    .all(|(time, _)| *time >= min_time && *time <= max_time)
            );
            assert!(points.iter().any(|(_, value)| *value == -10_000.0));
            assert!(points.iter().any(|(_, value)| *value == 10_000.0));

            let input_indices: Vec<usize> = points
                .iter()
                .map(|(_, value)| {
                    if *value == -10_000.0 {
                        count / 3
                    } else if *value == 10_000.0 {
                        count * 2 / 3
                    } else {
                        *value as usize
                    }
                })
                .collect();
            assert!(input_indices.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }

    #[test]
    fn flat_sparse_and_duplicate_timestamp_inputs_keep_their_samples() {
        let flat_times: Vec<f64> = (0..200).map(|index| index as f64).collect();
        let flat_values = vec![7.0; flat_times.len()];
        let (flat_points, _) = plot_time_points(&flat_times, &flat_values, 2);
        assert!(flat_points.len() <= 20);
        assert!(flat_points.iter().all(|(_, value)| *value == 7.0));

        let sparse = plot_time_points_in(&[1.0, 90.0], &[2.0, 3.0], 80, Some([0.0, 100.0]));
        assert_eq!(sparse, (vec![(1.0, 2.0), (90.0, 3.0)], Some([0.0, 100.0])));

        let duplicate = plot_time_points(&[5.0, 5.0, 2.0], &[10.0, 20.0, 30.0], 20);
        assert_eq!(duplicate.0, vec![(5.0, 10.0), (5.0, 20.0), (2.0, 30.0)]);
    }

    #[test]
    fn long_series_is_bounded_and_keeps_extrema_and_gaps() {
        let mut vals: Vec<f64> = (0..1_000_000).map(|i| (i % 100) as f64).collect();
        vals[500_000] = 9999.0;
        vals[700_001] = f64::NAN;
        let pts = plot_points(&vals, 40);
        assert!(pts.len() <= 80 * 5, "bounded by width, got {}", pts.len());
        assert!(pts.iter().any(|&(x, v)| x == 500_000.0 && v == 9999.0));
        assert!(pts.iter().any(|&(x, v)| x == 700_001.0 && v.is_nan()));
        assert!(
            pts.windows(2).all(|w| w[0].0 < w[1].0),
            "strictly ordered X"
        );
    }

    #[test]
    fn index_points_preserve_offset_after_sample_limit() {
        let count = MAX_SAMPLES_PER_SERIES + 10;
        let values = vec![1.0; count];
        let points = plot_points(&values, 40);

        assert!(points.iter().all(|&(x, _)| x >= 10.0));
        assert_eq!(points.last().map(|&(x, _)| x), Some((count - 1) as f64));
    }

    #[test]
    fn time_points_use_timestamp_coordinates_and_domain() {
        let (points, domain) = plot_time_points(&[0.0, 1.0, 2.0, 100.0], &[2.0, 4.0, 3.0, 8.0], 20);
        assert_eq!(
            points,
            vec![(0.0, 2.0), (1.0, 4.0), (2.0, 3.0), (100.0, 8.0)]
        );
        assert_eq!(domain, Some([0.0, 100.0]));
    }

    #[test]
    fn requested_window_excludes_outliers_and_sets_domain() {
        let (points, domain) = plot_time_points_in(
            &[0.0, 1.0, 2.0, 3.0, 10_000.0],
            &[0.0; 5],
            20,
            Some([0.0, 4.0]),
        );

        assert_eq!(points, vec![(0.0, 0.0), (1.0, 0.0), (2.0, 0.0), (3.0, 0.0)]);
        assert_eq!(domain, Some([0.0, 4.0]));
    }

    #[test]
    fn dense_requested_window_is_bounded_and_keeps_spike() {
        let times: Vec<f64> = (0..10_000).map(f64::from).collect();
        let mut values = vec![1.0; times.len()];
        values[150] = 10_000.0;
        let (points, domain) = plot_time_points_in(&times, &values, 10, Some([100.0, 200.0]));

        assert!(points.len() <= 20 * 5);
        assert!(
            points
                .iter()
                .any(|&(time, value)| time == 150.0 && value == 10_000.0)
        );
        assert!(
            points
                .iter()
                .all(|&(time, _)| (100.0..=200.0).contains(&time))
        );
        assert_eq!(domain, Some([100.0, 200.0]));
    }

    #[test]
    fn empty_requested_window_keeps_window_domain() {
        let (points, domain) =
            plot_time_points_in(&[0.0, 1.0, 2.0], &[1.0; 3], 20, Some([5.0, 8.0]));

        assert!(points.is_empty());
        assert_eq!(domain, Some([5.0, 8.0]));
    }

    #[test]
    fn invalid_requested_window_behaves_like_no_window() {
        let times = [0.0, 1.0, 2.0];
        let values = [1.0, 2.0, 3.0];
        let unbounded = plot_time_points(&times, &values, 20);

        assert_eq!(
            plot_time_points_in(&times, &values, 20, Some([2.0, 2.0])),
            unbounded
        );
        assert_eq!(
            plot_time_points_in(&times, &values, 20, Some([f64::NAN, 2.0])),
            unbounded
        );
    }

    #[test]
    fn requested_window_gap_threshold_ignores_outside_samples() {
        let threshold = gap_threshold_in(
            &[0.0, 1.0, 2.0, 3.0, 10_000.0],
            [0.0, 3.0],
            20,
            Some([0.0, 3.0]),
        );

        assert_eq!(threshold, Some(3.0));
    }

    #[test]
    fn no_window_gap_threshold_uses_all_samples_for_reduction_trigger() {
        let mut times = vec![0.0, 1.0, 2.0, 3.0, 100.0];
        times.extend(std::iter::repeat_n(f64::NAN, 20));

        let threshold = gap_threshold_in(&times, [0.0, 100.0], 1, None);

        assert_eq!(threshold, Some(100.0));
    }

    #[test]
    fn per_series_sample_limit_omits_oldest_samples() {
        let count = MAX_SAMPLES_PER_SERIES + 10;
        let times: Vec<f64> = (0..count).map(|index| index as f64).collect();
        let values = vec![1.0; count];
        let (points, _) = plot_time_points(&times, &values, 20);

        assert!(points.iter().all(|&(time, _)| time >= 10.0));
    }

    #[test]
    fn single_timestamp_domain_is_widened() {
        let (_, domain) = plot_time_points(&[42.0], &[1.0], 20);
        assert_eq!(domain, Some([41.5, 42.5]));
    }

    #[test]
    fn non_finite_samples_break_line_segments() {
        assert_eq!(
            segments(&[(0.0, 1.0), (1.0, f64::NAN), (2.0, 1.0), (3.0, 2.0)]).collect::<Vec<_>>(),
            vec![[(2.0, 1.0), (3.0, 2.0)]]
        );
    }

    #[test]
    fn timestamp_gap_breaks_line_without_breaking_even_intervals() {
        let irregular = [
            (0.0, 1.0),
            (1.0, 1.0),
            (2.0, 1.0),
            (3.0, 1.0),
            (100.0, 1.0),
            (101.0, 1.0),
            (102.0, 1.0),
        ];
        let threshold = gap_threshold(
            &irregular.iter().map(|point| point.0).collect::<Vec<_>>(),
            [0.0, 102.0],
            20,
        )
        .unwrap();
        let lines = segments_with_gap(&irregular, threshold).collect::<Vec<_>>();
        assert!(!lines.contains(&[(3.0, 1.0), (100.0, 1.0)]));

        let even = (0..10).map(|time| (time as f64, 1.0)).collect::<Vec<_>>();
        let threshold = gap_threshold(
            &even.iter().map(|point| point.0).collect::<Vec<_>>(),
            [0.0, 9.0],
            20,
        )
        .unwrap();
        assert_eq!(segments_with_gap(&even, threshold).count(), even.len() - 1);
    }

    #[test]
    fn dense_timestamp_series_preserves_every_segment_around_a_hole() {
        let times: Vec<f64> = (0..100_000)
            .map(|index| {
                if index < 50_000 {
                    index as f64
                } else {
                    index as f64 + 10_000.0
                }
            })
            .collect();
        let points: Vec<(f64, f64)> = times
            .iter()
            .enumerate()
            .map(|(index, &time)| (time, index as f64))
            .collect();
        let domain = [times[0], *times.last().unwrap()];
        let threshold = gap_threshold(&times, domain, 40).unwrap();
        let lines = segments_with_gap(&points, threshold).collect::<Vec<_>>();

        assert_eq!(lines.len(), points.len() - 2);
        assert!(
            lines
                .iter()
                .all(|line| line[0].0 >= 50_000.0 || line[1].0 < 60_000.0)
        );
    }

    #[test]
    fn dense_time_series_is_bounded_and_keeps_spike_at_its_timestamp() {
        let times: Vec<f64> = (0..1_000_000)
            .map(|index| (index as f64).powf(1.01))
            .collect();
        let mut values: Vec<f64> = (0..1_000_000).map(|index| (index % 17) as f64).collect();
        let spike_index = 500_123;
        values[spike_index] = 10_000.0;
        let (points, domain) = plot_time_points(&times, &values, 40);

        assert!(
            points.len() <= 80 * 5,
            "bounded by width, got {}",
            points.len()
        );
        assert!(
            points
                .iter()
                .any(|&(time, value)| { time == times[spike_index] && value == 10_000.0 })
        );
        assert_eq!(domain, Some([times[0], times[times.len() - 1]]));
    }

    #[test]
    fn non_finite_timestamps_are_skipped_and_input_order_is_retained() {
        let (points, domain) = plot_time_points(&[10.0, f64::NAN, 2.0], &[1.0, 2.0, 3.0], 20);
        assert_eq!(points, vec![(10.0, 1.0), (2.0, 3.0)]);
        assert_eq!(domain, Some([2.0, 10.0]));
    }

    #[test]
    fn degenerate_inputs() {
        assert!(plot_points(&[], 0).is_empty());
        assert_eq!(plot_points(&[1.0], 0), vec![(0.0, 1.0)]);
        let nan = vec![f64::NAN; 1000];
        assert!(plot_points(&nan, 1).len() <= 10);
    }
}
