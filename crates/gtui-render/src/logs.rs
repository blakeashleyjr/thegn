use gtui_core::frame::{FieldType, Frame};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

pub struct LogsRenderer;

impl LogsRenderer {
    /// Build the logs widget for a panel `rows` tall and `width` wide. Only the
    /// first `rows` lines can ever be visible (no scroll state), so only those
    /// are built; each borrows from `frame` and is clipped to the width budget.
    pub fn render<'a>(frame: &'a Frame, title: &str, rows: usize, width: usize) -> Paragraph<'a> {
        let mut lines = Vec::new();

        if !frame.fields.is_empty() {
            let str_field = frame.fields.iter().find(|f| f.ty == FieldType::String);

            if let Some(s_field) = str_field {
                let budget = crate::clip_budget(width);
                for val_str in s_field.strings().iter().take(rows) {
                    lines.push(Line::from(vec![Span::styled(
                        crate::clip_chars(val_str, budget),
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

        let p = LogsRenderer::render(&frame, "Logs", 10, 40);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
        p.render(buffer.area, &mut buffer);

        // First log line "hello" drawn inside the border (row 1, col 1+).
        assert_eq!(buffer.cell((1, 1)).unwrap().symbol(), "h");
        assert_eq!(buffer.cell((2, 1)).unwrap().symbol(), "e");
    }

    /// The pre-borrow implementation: every line cloned, no clipping.
    fn reference(frame: &Frame, area: Rect) -> Buffer {
        let mut lines = Vec::new();
        let f = frame
            .fields
            .iter()
            .find(|f| f.ty == FieldType::String)
            .unwrap();
        for v in f.strings().to_vec() {
            lines.push(Line::from(vec![Span::styled(
                v,
                Style::default().fg(Color::Gray),
            )]));
        }
        let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("L"));
        let mut b = Buffer::empty(area);
        p.render(area, &mut b);
        b
    }

    #[test]
    fn viewport_output_matches_full_materialisation() {
        let long = "x".repeat(5000);
        let mut vals: Vec<String> = (0..500).map(|i| format!("line {i} 日本語")).collect();
        vals[3] = long;
        let frame = Frame::new(vec![Field::new_str("l", vals)]);
        for (w, h) in [(40u16, 10u16), (7, 3), (80, 24), (2, 2)] {
            let area = Rect::new(0, 0, w, h);
            let mut got = Buffer::empty(area);
            LogsRenderer::render(&frame, "L", h as usize, w as usize).render(area, &mut got);
            assert_eq!(got, reference(&frame, area), "{w}x{h}");
        }
    }
}
