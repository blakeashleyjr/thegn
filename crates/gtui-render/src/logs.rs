use gtui_core::frame::{FieldType, Frame};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

pub struct LogsRenderer;

impl LogsRenderer {
    pub fn render<'a>(frame: &'a Frame) -> Paragraph<'a> {
        let mut lines = Vec::new();

        if !frame.fields.is_empty() {
            let str_field = frame.fields.iter().find(|f| f.ty == FieldType::String);

            if let Some(s_field) = str_field {
                let len = s_field.series.len();
                for i in 0..len {
                    let val_str = match s_field.series.get(i) {
                        Ok(val) => val.to_string(),
                        Err(_) => "".to_string(),
                    };
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

        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Logs"))
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
        // Mock a String series for logs by using our current workaround (MVP uses f64 under the hood if it's the only supported type natively, but let's test the formatting fallback).
        let field = Field::new("line", FieldType::String, vec![1.0, 2.0]);
        let frame = Frame::new(vec![field]);

        let p = LogsRenderer::render(&frame);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
        p.render(buffer.area, &mut buffer);

        assert_eq!(buffer.cell((1, 1)).unwrap().symbol(), "1");
        assert_eq!(buffer.cell((2, 1)).unwrap().symbol(), ".");
        assert_eq!(buffer.cell((3, 1)).unwrap().symbol(), "0");
    }
}
