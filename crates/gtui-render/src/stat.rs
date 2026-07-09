use gtui_core::frame::{FieldType, Frame};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};

pub struct StatRenderer;

impl StatRenderer {
    pub fn render<'a>(frame: &'a Frame, title: &str) -> Paragraph<'a> {
        let mut text = "No Data".to_string();

        if !frame.fields.is_empty() {
            let y_field = frame.fields.iter().find(|f| f.ty == FieldType::Float64);
            if let Some(y) = y_field
                && let Ok(y_vals) = y.series.f64()
                // For MVP, just take the last non-null value
                && let Some(val) = y_vals.into_iter().rev().flatten().next()
            {
                text = format!("{:.2}", val);
            }
        }

        Paragraph::new(text)
            .style(Style::default().fg(Color::Cyan))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title.to_string()),
            )
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
    fn test_stat_render() {
        let field = Field::new("val", FieldType::Float64, vec![1.0, 5.0, 42.0]);
        let frame = Frame::new(vec![field]);

        let p = StatRenderer::render(&frame, "Stat");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 10));
        p.render(buffer.area, &mut buffer);

        // Should render the last value "42.00"
        assert_eq!(buffer.cell((1, 1)).unwrap().symbol(), "4");
        assert_eq!(buffer.cell((2, 1)).unwrap().symbol(), "2");
    }
}
