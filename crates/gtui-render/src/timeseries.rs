use gtui_core::frame::{FieldType, Frame};
use ratatui::style::Color;
use ratatui::widgets::{Block, Borders, canvas::Canvas, canvas::Line};

pub struct TimeseriesRenderer;

impl TimeseriesRenderer {
    pub fn render<'a>(
        frame: &'a Frame,
        title: &str,
        bounds: [f64; 4], // [x_min, x_max, y_min, y_max]
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
                // In a real implementation this would perform LTTB downsampling.
                let y_field = frame.fields.iter().find(|f| f.ty == FieldType::Float64);
                if let Some(y) = y_field {
                    // Naive extraction for the MVP braille rendering.
                    let mut prev: Option<(f64, f64)> = None;
                    for (i, v) in y.floats().into_iter().enumerate() {
                        let x = i as f64; // Fallback to index if no X series
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

        let canvas = TimeseriesRenderer::render(&frame, "CPU", [0.0, 2.0, 0.0, 6.0]);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 10));

        canvas.render(buffer.area, &mut buffer);

        // Just assert it didn't panic and drew something (the borders at least)
        assert_eq!(buffer.cell((0, 0)).unwrap().symbol(), "┌");
    }
}
