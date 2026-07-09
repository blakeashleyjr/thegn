use gtui_core::frame::{FieldType, Frame};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

pub struct LogsRenderer;

impl LogsRenderer {
    pub fn render<'a>(frame: &'a Frame, title: &str) -> Paragraph<'a> {
        let mut lines = Vec::new();

        if !frame.fields.is_empty() {
            let str_field = frame.fields.iter().find(|f| f.ty == FieldType::String);

            if let Some(s_field) = str_field {
                for val_str in s_field.strings() {
                    lines.push(Line::from(vec![Span::styled(
                        val_str,
                        Style::default().fg(Color::Gray),
                    )]));
                }
            } else {
                lines.push(Line::from("No string field found for logs"));
            }
        } else {
            lines.push(Line::from("No Data"));
        }

        Paragraph::new(lines).block(
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
    fn test_logs_render() {
        let field = Field::new_str("line", vec!["hello".into(), "world".into()]);
        let frame = Frame::new(vec![field]);

        let p = LogsRenderer::render(&frame, "Logs");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
        p.render(buffer.area, &mut buffer);

        // First log line "hello" drawn inside the border (row 1, col 1+).
        assert_eq!(buffer.cell((1, 1)).unwrap().symbol(), "h");
        assert_eq!(buffer.cell((2, 1)).unwrap().symbol(), "e");
    }
}
