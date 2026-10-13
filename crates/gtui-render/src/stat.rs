use gtui_core::frame::{FieldType, Frame};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Paragraph};

pub struct StatRenderer;

impl StatRenderer {
    pub fn render<'a>(
        frame: &'a Frame,
        title: &str,
        colors: &crate::RenderColors,
    ) -> Paragraph<'a> {
        let mut text = "No Data".to_string();

        if !frame.fields.is_empty() {
            let y_field = frame.fields.iter().find(|f| f.ty == FieldType::Float64);
            if let Some(y) = y_field
                // For MVP, just take the last value.
                && let Some(val) = y.floats().last()
            {
                text = format!("{val:.2}");
            }
        }

        Paragraph::new(text)
            .style(Style::default().fg(colors.value))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(colors.border))
                    .title_style(Style::default().fg(colors.title))
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

        let p = StatRenderer::render(&frame, "Stat", &crate::RenderColors::default());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 10));
        p.render(buffer.area, &mut buffer);

        // Should render the last value "42.00"
        assert_eq!(buffer.cell((1, 1)).unwrap().symbol(), "4");
        assert_eq!(buffer.cell((2, 1)).unwrap().symbol(), "2");
    }

    #[test]
    fn supplied_value_color_reaches_the_buffer() {
        let frame = Frame::new(vec![Field::new("val", FieldType::Float64, vec![42.0])]);
        let colors = crate::RenderColors {
            value: ratatui::style::Color::Rgb(1, 2, 3),
            ..crate::RenderColors::default()
        };
        let area = Rect::new(0, 0, 12, 5);
        let mut buffer = Buffer::empty(area);
        StatRenderer::render(&frame, "S", &colors).render(area, &mut buffer);
        assert_eq!(buffer.cell((1, 1)).unwrap().fg, colors.value);
    }
}
