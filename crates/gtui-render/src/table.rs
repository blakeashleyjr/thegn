use gtui_core::frame::Frame;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Row, Table};
use std::borrow::Cow;

pub struct TableRenderer;

impl TableRenderer {
    /// Build the table for a panel `rows` tall and `width` wide. Only the first
    /// `rows` data rows can be visible (no scroll state), so only those are
    /// built; text cells borrow from `frame` and are clipped to the width budget.
    pub fn render<'a>(frame: &'a Frame, title: &str, rows: usize, width: usize) -> Table<'a> {
        let headers: Vec<&str> = frame.fields.iter().map(|f| f.name.as_str()).collect();

        let mut out_rows = Vec::new();
        if !frame.fields.is_empty() {
            // Find max length among all series
            let max_len = frame.fields.iter().map(|f| f.len()).max().unwrap_or(0);
            let budget = crate::clip_budget(width);

            for i in 0..max_len.min(rows) {
                let row_data: Vec<Cow<'a, str>> = frame
                    .fields
                    .iter()
                    .map(|field| match field.cell_str(i) {
                        Cow::Borrowed(s) => Cow::Borrowed(crate::clip_chars(s, budget)),
                        Cow::Owned(s) => Cow::Owned(crate::clip_chars(&s, budget).to_string()),
                    })
                    .collect();
                out_rows.push(Row::new(row_data));
            }
        }

        // Just use proportional widths for MVP
        let widths: Vec<ratatui::layout::Constraint> = headers
            .iter()
            .map(|_| ratatui::layout::Constraint::Ratio(1, headers.len() as u32))
            .collect();

        Table::new(out_rows, widths)
            .header(Row::new(headers).style(Style::default().fg(Color::Yellow)))
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
    use gtui_core::frame::{Field, FieldType};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    #[test]
    fn test_table_render() {
        let field = Field::new("val", FieldType::Float64, vec![1.0, 5.0]);
        let frame = Frame::new(vec![field]);

        let t = TableRenderer::render(&frame, "Table", 10, 40);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
        t.render(buffer.area, &mut buffer);

        // Header
        assert_eq!(buffer.cell((1, 1)).unwrap().symbol(), "v");
        assert_eq!(buffer.cell((2, 1)).unwrap().symbol(), "a");
        assert_eq!(buffer.cell((3, 1)).unwrap().symbol(), "l");
    }

    /// The pre-borrow implementation: every row and cell materialised.
    fn reference(frame: &Frame, area: Rect) -> Buffer {
        let headers: Vec<String> = frame.fields.iter().map(|f| f.name.clone()).collect();
        let max_len = frame.fields.iter().map(|f| f.len()).max().unwrap_or(0);
        let rows: Vec<Row> = (0..max_len)
            .map(|i| Row::new(frame.fields.iter().map(|f| f.cell(i)).collect::<Vec<_>>()))
            .collect();
        let widths: Vec<ratatui::layout::Constraint> = headers
            .iter()
            .map(|_| ratatui::layout::Constraint::Ratio(1, headers.len() as u32))
            .collect();
        let t = Table::new(rows, widths)
            .header(Row::new(headers).style(Style::default().fg(Color::Yellow)))
            .block(Block::default().borders(Borders::ALL).title("T"));
        let mut b = Buffer::empty(area);
        t.render(area, &mut b);
        b
    }

    #[test]
    fn viewport_output_matches_full_materialisation() {
        let n = 400;
        let frame = Frame::new(vec![
            Field::new(
                "n",
                FieldType::Float64,
                (0..n).map(|i| i as f64 / 3.0).collect(),
            ),
            // ragged: shorter column
            Field::new_str("s", (0..n / 2).map(|i| format!("row {i} 日本")).collect()),
            Field::new_str("long", vec!["y".repeat(9000); n]),
        ]);
        for (w, h) in [(60u16, 12u16), (9, 4), (120, 30), (3, 3)] {
            let area = Rect::new(0, 0, w, h);
            let mut got = Buffer::empty(area);
            TableRenderer::render(&frame, "T", h as usize, w as usize).render(area, &mut got);
            assert_eq!(got, reference(&frame, area), "{w}x{h}");
        }
    }
}
