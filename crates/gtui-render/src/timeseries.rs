use gtui_core::frame::{FieldType, Frame};
use ratatui::style::Color;
use ratatui::widgets::{Block, Borders, canvas::Canvas, canvas::Line};

pub struct TimeseriesRenderer;

/// Samples per horizontal braille pixel above which a series is reduced. Below
/// it every sample is drawn, so the output is identical to the naive walk.
const SAMPLES_PER_PIXEL: usize = 4;

/// The points to draw as `(index, value)`, bounded by the panel width.
///
/// A series of at most `SAMPLES_PER_PIXEL` samples per horizontal braille pixel
/// (`2 * width`) is returned whole. A longer one is reduced bucket-by-bucket
/// (one bucket per pixel) to the first, minimum, maximum and last finite sample
/// in index order, so extrema and the original X positions survive, plus the
/// first non-finite sample of a bucket so gaps keep their position. Work and
/// scratch are O(width), after one linear pass over the borrowed slice.
fn plot_points(values: &[f64], width: usize) -> Vec<(f64, f64)> {
    let pixels = width.saturating_mul(2).max(1);
    if values.len() <= pixels.saturating_mul(SAMPLES_PER_PIXEL) {
        return values
            .iter()
            .enumerate()
            .map(|(i, &v)| (i as f64, v))
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
        out.extend(idx.into_iter().map(|i| (i as f64, values[i])));
    }
    out
}

impl TimeseriesRenderer {
    /// `width` is the panel width in columns; it bounds how many samples are
    /// walked (see [`plot_points`]).
    pub fn render<'a>(
        frame: &'a Frame,
        title: &str,
        bounds: [f64; 4], // [x_min, x_max, y_min, y_max]
        width: usize,
    ) -> Canvas<'a, impl Fn(&mut ratatui::widgets::canvas::Context) + 'a> {
        Canvas::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title.to_string()),
            )
            .x_bounds([bounds[0], bounds[1]])
            .y_bounds([bounds[2], bounds[3]])
            .paint(move |ctx| {
                if frame.fields.is_empty() {
                    return;
                }

                // For MVP: assume single series, and we just use indices as X if no time field exists.
                let y_field = frame.fields.iter().find(|f| f.ty == FieldType::Float64);
                if let Some(y) = y_field {
                    let mut prev: Option<(f64, f64)> = None;
                    for (x, v) in plot_points(y.floats(), width) {
                        if let Some((px, py)) = prev {
                            ctx.draw(&Line {
                                x1: px,
                                y1: py,
                                x2: x,
                                y2: v,
                                color: Color::Green,
                            });
                        }
                        prev = Some((x, v));
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

        let canvas = TimeseriesRenderer::render(&frame, "CPU", [0.0, 2.0, 0.0, 6.0], 20);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 10));

        canvas.render(buffer.area, &mut buffer);

        // Just assert it didn't panic and drew something (the borders at least)
        assert_eq!(buffer.cell((0, 0)).unwrap().symbol(), "┌");
    }

    /// The pre-borrow walk: every sample, cloned vec.
    fn reference(frame: &Frame, bounds: [f64; 4], area: Rect) -> Buffer {
        let vals = frame.fields[0].floats().to_vec();
        let c = Canvas::default()
            .block(Block::default().borders(Borders::ALL).title(""))
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
                            color: Color::Green,
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
        let canvas = TimeseriesRenderer::render(&frame, "", bounds, 40);
        canvas.render(area, &mut got);
        assert_eq!(got, reference(&frame, bounds, area));
        assert_eq!(plot_points(frame.fields[0].floats(), 40).len(), 150);
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
    fn degenerate_inputs() {
        assert!(plot_points(&[], 0).is_empty());
        assert_eq!(plot_points(&[1.0], 0), vec![(0.0, 1.0)]);
        let nan = vec![f64::NAN; 1000];
        assert!(plot_points(&nan, 1).len() <= 10);
    }
}
